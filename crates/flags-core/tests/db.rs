//! flags-core against a real Postgres. Each test gets a fresh, migrated
//! database from `#[sqlx::test]`; set `DATABASE_URL` to a superuser on a
//! scratch cluster (`podman compose up -d` gives you one on port 15434).

use chrono::Utc;
use flags_core::apps::{AppsExt, Rotate};
use flags_core::eval::{Context, Operator, Rule};
use flags_core::flags::{ChangeMeta, EnvPatch, FlagPatch, FlagsExt, NewFlag};
use flags_core::keys::KeyKind;
use flags_core::platform_events::{self, begin_live, Outcome};
use flags_core::telemetry::{self, ErrorEvent, EvalEvent};
use flags_core::{eval, keys, Error};
use otto_tenant::ids::{OrgId, UserId};
use otto_tenant::Db;
use serde_json::json;
use sqlx::{PgPool, Postgres};

fn meta(actor: UserId) -> ChangeMeta {
    ChangeMeta {
        actor,
        reason: None,
        expected_version: None,
    }
}

async fn db(pool: PgPool) -> Db {
    Db::from_pool(pool)
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn tenant_isolation_is_enforced_on_every_tenant_table(pool: PgPool) {
    let db = db(pool).await;
    let report = db.verify_tenant_isolation().await.expect("isolation");
    let summary = report.summary();
    assert!(summary.contains("otto_app"), "{summary}");
    for name in ["rollback_policies", "rollback_events"] {
        let table = report
            .tables
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("{name} is missing from the isolation report"));
        assert!(table.rls_enabled, "{name}: RLS not enabled");
        assert!(table.rls_forced, "{name}: RLS not forced");
    }
}

/// The SQLSTATE of a failed statement.
fn sqlstate(err: &sqlx::Error) -> String {
    err.as_database_error()
        .and_then(|e| e.code())
        .map(|c| c.into_owned())
        .unwrap_or_else(|| format!("not a database error: {err}"))
}

/// A transaction as `otto_app` with no org pinned, so `current_org()` is NULL.
/// This is what background code (the checker, the retention sweep) sees. The
/// test pool connects as a superuser, which bypasses RLS, so a plain pool
/// query would prove nothing about the unpinned policies.
async fn as_app_unpinned(pool: &PgPool) -> sqlx::Transaction<'static, Postgres> {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL ROLE otto_app")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx
}

/// The auto-rollback tables are tenant tables: pinned transactions see only
/// their own org, composite foreign keys stop cross-org references, and the
/// unpinned policies grant exactly a read of policies and a read+delete of
/// events. Postgres aborts a transaction on its first error, so every
/// expected failure runs in its own transaction.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn auto_rollback_tables_are_tenant_isolated(pool: PgPool) {
    let db = db(pool).await;
    let (a, b) = (OrgId::new(), OrgId::new());
    let actor = UserId::new();

    // Arrange, then commit before anything is allowed to fail.
    let mut tx = db.begin(a).await.unwrap();
    let (app_a, _) = tx
        .create_app("web", &["production".into()], actor)
        .await
        .unwrap();
    let flag_a = tx
        .create_flag(
            &app_a,
            NewFlag {
                key: "checkout".into(),
                ..Default::default()
            },
            actor,
            None,
        )
        .await
        .unwrap();
    let policy: (uuid::Uuid,) = sqlx::query_as(
        "INSERT INTO rollback_policies (org_id, app_id, flag_id, environment) \
         VALUES ($1, $2, $3, 'production') RETURNING id",
    )
    .bind(a)
    .bind(app_a.id)
    .bind(flag_a.id)
    .fetch_one(tx.conn())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO rollback_events (org_id, policy_id, flag_id, environment, outcome, observed) \
         VALUES ($1, $2, $3, 'production', 'would_roll_back', '{}')",
    )
    .bind(a)
    .bind(policy.0)
    .bind(flag_a.id)
    .execute(tx.conn())
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin(b).await.unwrap();
    let (app_b, _) = tx
        .create_app("web", &["production".into()], actor)
        .await
        .unwrap();
    tx.commit().await.unwrap();

    // Org A sees exactly its own rows.
    let mut tx = db.begin(a).await.unwrap();
    for table in ["rollback_policies", "rollback_events"] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(tx.conn())
            .await
            .unwrap();
        assert_eq!(n, 1, "org A must see its own {table} row");
    }
    drop(tx);

    // Org B sees neither row.
    let mut tx = db.begin(b).await.unwrap();
    for table in ["rollback_policies", "rollback_events"] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(tx.conn())
            .await
            .unwrap();
        assert_eq!(n, 0, "org B must not see org A's {table}");
    }
    drop(tx);

    // Unpinned otto_app, the succeeding statements, rolled back at the end.
    let mut tx = as_app_unpinned(db.pool()).await;
    let n: i64 = sqlx::query_scalar("SELECT count(*) FROM rollback_policies")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(n, 1, "rollback_policies_checker lets the checker read");
    let updated = sqlx::query("UPDATE rollback_policies SET mode = 'observe'")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_eq!(updated.rows_affected(), 0, "unpinned code cannot write");
    let deleted = sqlx::query("DELETE FROM rollback_policies")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_eq!(
        deleted.rows_affected(),
        0,
        "unpinned code cannot delete policies"
    );
    // The sweep's real statement: needs the DELETE and the SELECT policy.
    let swept = sqlx::query(
        "DELETE FROM rollback_events WHERE created_at < now() - make_interval(days => 0)",
    )
    .execute(&mut *tx)
    .await
    .unwrap();
    assert_eq!(swept.rows_affected(), 1, "the retention sweep deletes");
    tx.rollback().await.unwrap();

    // Each expected failure in a fresh transaction.
    // Org B's own app, org A's flag: only the composite flag FK can refuse.
    // 'staging', not 'production': org A's policy already holds (flag, 'production'),
    // and the unique index would fire first (23505) and hide the FK.
    let mut tx = db.begin(b).await.unwrap();
    let err = sqlx::query(
        "INSERT INTO rollback_policies (org_id, app_id, flag_id, environment) \
         VALUES ($1, $2, $3, 'staging')",
    )
    .bind(b)
    .bind(app_b.id)
    .bind(flag_a.id)
    .execute(tx.conn())
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err), "23503");
    drop(tx);

    let mut tx = db.begin(a).await.unwrap();
    let err = sqlx::query(
        "INSERT INTO rollback_events (org_id, policy_id, flag_id, environment, outcome, observed) \
         VALUES ($1, $2, $3, 'production', 'bogus', '{}')",
    )
    .bind(a)
    .bind(policy.0)
    .bind(flag_a.id)
    .execute(tx.conn())
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err), "23514");
    drop(tx);

    let mut tx = db.begin(a).await.unwrap();
    let err = sqlx::query(
        "INSERT INTO flag_errors (org_id, flag_id, flag_enabled, error_type, error_message, \
         occurred_at, reporter) VALUES ($1, $2, true, 'e', 'm', now(), 'browser')",
    )
    .bind(a)
    .bind(flag_a.id)
    .execute(tx.conn())
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err), "23514");
    drop(tx);

    let mut tx = as_app_unpinned(db.pool()).await;
    let err = sqlx::query(
        "INSERT INTO rollback_policies (org_id, app_id, flag_id, environment) \
         VALUES ($1, $2, $3, 'staging')",
    )
    .bind(a)
    .bind(app_a.id)
    .bind(flag_a.id)
    .execute(&mut *tx)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err), "42501");
    drop(tx);

    let mut tx = as_app_unpinned(db.pool()).await;
    let err = sqlx::query(
        "INSERT INTO rollback_events (org_id, policy_id, flag_id, environment, outcome, observed) \
         VALUES ($1, $2, $3, 'production', 'would_roll_back', '{}')",
    )
    .bind(a)
    .bind(policy.0)
    .bind(flag_a.id)
    .execute(&mut *tx)
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err), "42501");
    drop(tx);
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn an_app_and_its_keys_resolve_only_for_its_own_org(pool: PgPool) {
    let db = db(pool).await;
    let (org, other) = (OrgId::new(), OrgId::new());
    let actor = UserId::new();

    let mut tx = db.begin(org).await.unwrap();
    let (app, issued) = tx
        .create_app(
            "web",
            &["production".into(), "staging".into(), "staging".into()],
            actor,
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(app.environments, ["production", "staging"]);
    let client = issued.client_key.unwrap();
    let server = issued.server_key.unwrap();
    assert_eq!(app.client_key, client);

    let owner = keys::resolve(&db, &server).await.unwrap().unwrap();
    assert_eq!((owner.org_id, owner.app_id), (org, app.id));
    assert_eq!(owner.kind, keys::KeyKind::Server);
    assert!(keys::resolve(&db, &format!("srv_{}", "0".repeat(64)))
        .await
        .unwrap()
        .is_none());

    // The other org sees nothing, and cannot reach the app by id either.
    let mut tx = db.begin(other).await.unwrap();
    assert!(tx.list_apps().await.unwrap().is_empty());
    assert!(tx.get_app(app.id).await.unwrap().is_none());
    let err = tx.resolve_app(&app.id.to_string()).await.unwrap_err();
    assert_eq!(err.code(), "app_not_found");
    tx.rollback().await.unwrap();

    // Rotating the server key retires the old one at commit; the client key stays.
    let mut tx = db.begin(org).await.unwrap();
    let (after, rotated) = tx.rotate_keys(&app, Rotate::Server, actor).await.unwrap();
    tx.commit().await.unwrap();
    assert_eq!(after.client_key, client);
    assert!(rotated.client_key.is_none());
    assert!(keys::resolve(&db, &server).await.unwrap().is_none());
    assert!(keys::resolve(&db, &rotated.server_key.unwrap())
        .await
        .unwrap()
        .is_some());
    assert!(keys::resolve(&db, &client).await.unwrap().is_some());
}

/// Foreign-key checks bypass row-level security. The composite (org_id, id)
/// keys are what stop a flag being attached to another org's app.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_flag_cannot_be_attached_to_another_orgs_app(pool: PgPool) {
    let db = db(pool).await;
    let (org, other) = (OrgId::new(), OrgId::new());
    let mut tx = db.begin(org).await.unwrap();
    let (app, _) = tx.create_app("web", &[], UserId::new()).await.unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin(other).await.unwrap();
    let r = sqlx::query(
        "INSERT INTO feature_flags (org_id, app_id, key, name) VALUES ($1, $2, 'sneaky', 'sneaky')",
    )
    .bind(other)
    .bind(app.id)
    .execute(tx.conn())
    .await;
    assert!(r.is_err(), "another org's app id must be refused");
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn flags_are_versioned_and_roll_back(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let mut tx = db.begin(org).await.unwrap();
    let (app, _) = tx
        .create_app("web", &["production".into()], actor)
        .await
        .unwrap();

    let flag = tx
        .create_flag(
            &app,
            NewFlag {
                key: "new-checkout".into(),
                variations: Some(json!({"control": {}, "treatment": {"weight": 1}})),
                ..Default::default()
            },
            actor,
            Some("launch prep".into()),
        )
        .await
        .unwrap();
    assert_eq!(flag.version, 1);
    assert_eq!(flag.environments, json!({"production": {"enabled": false}}));
    let dup = tx
        .create_flag(
            &app,
            NewFlag {
                key: "new-checkout".into(),
                ..Default::default()
            },
            actor,
            None,
        )
        .await;
    assert_eq!(dup.unwrap_err().code(), "duplicate_flag_key");
    tx.rollback().await.unwrap();

    // The duplicate poisoned that transaction; redo the create in a fresh one.
    let mut tx = db.begin(org).await.unwrap();
    let (app, _) = tx
        .create_app("web", &["production".into()], actor)
        .await
        .unwrap();
    tx.create_flag(
        &app,
        NewFlag {
            key: "new-checkout".into(),
            variations: Some(json!({"control": {}, "treatment": {}})),
            ..Default::default()
        },
        actor,
        None,
    )
    .await
    .unwrap();

    let on = tx
        .set_environment(
            &app,
            "new-checkout",
            "production",
            EnvPatch {
                enabled: Some(true),
                rollout_percentage: Some(10.0),
                rules: Some(vec![Rule {
                    attribute: "plan".into(),
                    operator: Operator::Equals,
                    values: vec![json!("enterprise")],
                    enabled: None,
                    variation: Some("treatment".into()),
                    description: None,
                }]),
                default_variation: None,
            },
            ChangeMeta {
                expected_version: Some(1),
                ..meta(actor)
            },
        )
        .await
        .unwrap();
    assert_eq!(on.version, 2);

    // A stale writer is refused.
    let stale = tx
        .set_environment(
            &app,
            "new-checkout",
            "production",
            EnvPatch {
                rollout_percentage: Some(50.0),
                ..Default::default()
            },
            ChangeMeta {
                expected_version: Some(1),
                ..meta(actor)
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(stale, Error::VersionConflict { actual: 2, .. }));

    // An unknown environment names the real ones.
    let err = tx
        .set_environment(
            &app,
            "new-checkout",
            "prod",
            EnvPatch::default(),
            meta(actor),
        )
        .await
        .unwrap_err();
    assert!(err.to_string().contains("production"), "{err}");

    // Removing a variation a rule still uses is refused.
    let err = tx
        .update_flag(
            &app,
            "new-checkout",
            FlagPatch {
                variations: Some(json!({"control": {}})),
                ..Default::default()
            },
            meta(actor),
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "invalid_argument");

    let mut ctx = Context {
        user_id: Some("u1".into()),
        ..Default::default()
    };
    ctx.attributes.insert("plan".into(), json!("enterprise"));
    let e = eval::evaluate(on.view(), "production", &ctx);
    assert!(e.enabled);
    assert_eq!(e.variation.as_deref(), Some("treatment"));

    let (back, to) = tx
        .rollback_flag(
            &app,
            "new-checkout",
            None,
            ChangeMeta {
                reason: Some("errors".into()),
                ..meta(actor)
            },
        )
        .await
        .unwrap();
    assert_eq!((to, back.version), (1, 3));
    assert_eq!(back.environments, json!({"production": {"enabled": false}}));

    let history = tx.flag_history(&back, 10).await.unwrap();
    let changes: Vec<_> = history
        .iter()
        .map(|v| (v.version, v.change.as_str()))
        .collect();
    assert_eq!(
        changes,
        [(3, "rollback"), (2, "set_environment"), (1, "create")]
    );
    assert_eq!(history[0].reason.as_deref(), Some("rollback to v1: errors"));

    let archived = tx
        .set_archived(&app, "new-checkout", true, meta(actor))
        .await
        .unwrap();
    assert!(archived.archived_at.is_some());
    assert!(tx.list_flags(&app, false).await.unwrap().is_empty());
    assert_eq!(tx.list_flags(&app, true).await.unwrap().len(), 1);
    tx.commit().await.unwrap();
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn telemetry_rolls_up_and_feeds_health(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let mut tx = db.begin(org).await.unwrap();
    let (app, _) = tx.create_app("web", &[], actor).await.unwrap();
    let flag = tx
        .create_flag(
            &app,
            NewFlag {
                key: "f".into(),
                ..Default::default()
            },
            actor,
            None,
        )
        .await
        .unwrap();

    let now = Utc::now();
    let mut events: Vec<EvalEvent> = (0..300)
        .map(|i| EvalEvent {
            flag_key: "f".into(),
            enabled: i % 2 == 0,
            variation: None,
            environment: "production".into(),
            at: now,
        })
        .collect();
    events.push(EvalEvent {
        flag_key: "gone".into(),
        enabled: true,
        variation: None,
        environment: "production".into(),
        at: now,
    });
    let r = telemetry::record_evaluations(&mut tx, app.id, KeyKind::Server, events.clone())
        .await
        .unwrap();
    assert_eq!((r.receipt.accepted, r.receipt.unknown_flags), (300, 1));
    // A second batch adds to the same bucket rather than duplicating it.
    telemetry::record_evaluations(&mut tx, app.id, KeyKind::Server, events)
        .await
        .unwrap();

    let errors = (0..40)
        .map(|i| ErrorEvent {
            flag_key: "f".into(),
            flag_enabled: i < 38,
            environment: Some("production".into()),
            error_type: "TypeError".into(),
            error_message: "x is undefined".repeat(500),
            stack_trace: None,
            at: now,
        })
        .collect();
    telemetry::record_errors(&mut tx, app.id, KeyKind::Server, errors)
        .await
        .unwrap();

    let h = telemetry::health(&mut tx, flag.id, Some("production"), 24)
        .await
        .unwrap();
    assert_eq!((h.evaluations.enabled, h.evaluations.disabled), (300, 300));
    assert_eq!((h.errors.enabled, h.errors.disabled), (38, 2));
    assert!(
        h.assessment.contains("Consider rolling back"),
        "{}",
        h.assessment
    );
    // Shown shortened (it is untrusted text headed for an agent); stored at
    // the 2,000-character cap.
    assert_eq!(h.top_errors[0].last_message.chars().count(), 301);
    let stored: i32 = sqlx::query_scalar("SELECT max(length(error_message)) FROM flag_errors")
        .fetch_one(tx.conn())
        .await
        .unwrap();
    assert_eq!(stored, 2_000);
    tx.commit().await.unwrap();
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn telemetry_records_the_reporting_key_kind(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let mut tx = db.begin(org).await.unwrap();
    let (app, _) = tx.create_app("web", &[], actor).await.unwrap();
    let flag = tx
        .create_flag(
            &app,
            NewFlag {
                key: "f".into(),
                ..Default::default()
            },
            actor,
            None,
        )
        .await
        .unwrap();

    let now = Utc::now();
    let evals = |key: &str, n: usize| -> Vec<EvalEvent> {
        (0..n)
            .map(|_| EvalEvent {
                flag_key: key.into(),
                enabled: true,
                variation: Some("v".into()),
                environment: "production".into(),
                at: now,
            })
            .collect()
    };
    let mut server_batch = evals("f", 3);
    server_batch.extend(evals("gone", 1));
    let recorded = telemetry::record_evaluations(&mut tx, app.id, KeyKind::Server, server_batch)
        .await
        .unwrap();
    assert_eq!(
        (recorded.receipt.accepted, recorded.receipt.unknown_flags),
        (3, 1)
    );
    // Listed once however many events named it; unknown keys are excluded.
    assert_eq!(recorded.flags, vec![flag.id]);
    let recorded = telemetry::record_evaluations(&mut tx, app.id, KeyKind::Client, evals("f", 2))
        .await
        .unwrap();
    assert_eq!(recorded.flags, vec![flag.id]);

    let (count, server_count): (i64, i64) =
        sqlx::query_as("SELECT count, server_count FROM flag_eval_hourly WHERE flag_id = $1")
            .bind(flag.id)
            .fetch_one(tx.conn())
            .await
            .unwrap();
    assert_eq!((count, server_count), (5, 3));

    let error = || {
        vec![ErrorEvent {
            flag_key: "f".into(),
            flag_enabled: true,
            environment: Some("production".into()),
            error_type: "E".into(),
            error_message: "m".into(),
            stack_trace: None,
            at: now,
        }]
    };
    telemetry::record_errors(&mut tx, app.id, KeyKind::Server, error())
        .await
        .unwrap();
    let recorded = telemetry::record_errors(&mut tx, app.id, KeyKind::Client, error())
        .await
        .unwrap();
    assert_eq!(recorded.flags, vec![flag.id]);
    let mut reporters: Vec<Option<String>> = sqlx::query_scalar("SELECT reporter FROM flag_errors")
        .fetch_all(tx.conn())
        .await
        .unwrap();
    reporters.sort();
    assert_eq!(
        reporters,
        vec![Some("client".to_string()), Some("server".to_string())]
    );
    tx.commit().await.unwrap();
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn org_deleted_purges_everything_and_revokes_keys(pool: PgPool) {
    let db = db(pool).await;
    let (org, other) = (OrgId::new(), OrgId::new());
    let actor = UserId::new();
    let mut keys_issued = vec![];
    for o in [org, other] {
        let mut tx = db.begin(o).await.unwrap();
        let (app, issued) = tx.create_app("web", &[], actor).await.unwrap();
        tx.create_flag(
            &app,
            NewFlag {
                key: "f".into(),
                ..Default::default()
            },
            actor,
            None,
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        keys_issued.push(issued.server_key.unwrap());
    }

    let body = otto_resource::webhook::body(
        uuid::Uuid::new_v4(),
        "org.deleted",
        Utc::now(),
        &json!({"org_id": org.as_uuid()}),
    );
    let event = otto_resource::webhook::parse(&body).unwrap();
    assert!(matches!(
        platform_events::apply(&db, &event).await.unwrap(),
        Outcome::Applied { .. }
    ));
    assert_eq!(
        platform_events::apply(&db, &event).await.unwrap(),
        Outcome::Duplicate
    );

    assert!(keys::resolve(&db, &keys_issued[0]).await.unwrap().is_none());
    assert!(keys::resolve(&db, &keys_issued[1]).await.unwrap().is_some());
    assert!(matches!(
        begin_live(&db, org, None).await,
        Err(Error::AccessRevoked)
    ));

    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM feature_flags WHERE org_id = $1")
        .bind(org)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(left, 0);
    let mut tx = begin_live(&db, other, None).await.unwrap();
    assert_eq!(tx.list_apps().await.unwrap().len(), 1);
    tx.rollback().await.unwrap();
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_removed_member_is_refused_but_others_are_not(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let (gone, stays) = (UserId::new(), UserId::new());
    let body = otto_resource::webhook::body(
        uuid::Uuid::new_v4(),
        "member.removed",
        Utc::now(),
        &json!({"org_id": org.as_uuid(), "user_id": gone.as_uuid()}),
    );
    platform_events::apply(&db, &otto_resource::webhook::parse(&body).unwrap())
        .await
        .unwrap();
    assert!(platform_events::revoked(&db, org, gone).await.unwrap());
    assert!(!platform_events::revoked(&db, org, stays).await.unwrap());
    assert!(begin_live(&db, org, Some(stays)).await.is_ok());
}
