//! flags-core against a real Postgres. Each test gets a fresh, migrated
//! database from `#[sqlx::test]`; set `DATABASE_URL` to a superuser on a
//! scratch cluster (`podman compose up -d` gives you one on port 15434).

use chrono::Utc;
use flags_core::apps::{AppsExt, Rotate};
use flags_core::eval::{Context, Operator, Rule};
use flags_core::flags::{ChangeMeta, EnvPatch, FlagPatch, FlagsExt, NewFlag};
use flags_core::keys::KeyKind;
use flags_core::platform_events::{self, begin_live, Outcome};
use flags_core::rollback::{self, Mode, PolicyState, RollbackExt, Thresholds};
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

/// The columns' defaults and `Thresholds::default()` are one set of numbers
/// written twice; this keeps them from drifting.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn policy_column_defaults_match_the_rust_thresholds(pool: PgPool) {
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
                key: "f".into(),
                ..Default::default()
            },
            actor,
            None,
        )
        .await
        .unwrap();
    let row: (i64, i64, f64, Option<f64>, i32, bool) = sqlx::query_as(
        "INSERT INTO rollback_policies (org_id, app_id, flag_id, environment) \
         VALUES ($1, $2, $3, 'production') \
         RETURNING min_evaluations, min_errors, max_error_ratio, max_error_rate, \
                   window_hours, include_client_reports",
    )
    .bind(org)
    .bind(app.id)
    .bind(flag.id)
    .fetch_one(tx.conn())
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let from_columns = Thresholds {
        min_evaluations: row.0,
        min_errors: row.1,
        max_error_ratio: row.2,
        max_error_rate: row.3,
        window_hours: row.4,
        include_client_reports: row.5,
    };
    assert_eq!(from_columns, Thresholds::default());
}

// ---- auto-rollback policies, events, samples

/// An org with one app (`production`, `staging`) and one flag `f`.
async fn policy_fixture(
    db: &Db,
    org: OrgId,
    actor: UserId,
) -> (flags_core::apps::FlagApp, flags_core::flags::FeatureFlag) {
    let mut tx = db.begin(org).await.unwrap();
    let (app, _) = tx
        .create_app("web", &["production".into(), "staging".into()], actor)
        .await
        .unwrap();
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
    tx.commit().await.unwrap();
    (app, flag)
}

async fn audit_actions(tx: &mut otto_tenant::Tx<'_>) -> Vec<String> {
    sqlx::query_scalar(
        "SELECT action FROM audit_events WHERE action LIKE 'flags.rollback_policy.%' ORDER BY id",
    )
    .fetch_all(tx.conn())
    .await
    .unwrap()
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn rollback_policies_are_org_isolated_and_replaced_in_place(pool: PgPool) {
    let db = db(pool).await;
    let (a, b) = (OrgId::new(), OrgId::new());
    let actor = UserId::new();
    let other_actor = UserId::new();
    let (app, flag) = policy_fixture(&db, a, actor).await;

    let mut tx = db.begin(a).await.unwrap();
    let err = tx
        .set_policy(
            &app,
            &flag,
            "qa",
            Mode::Enforce,
            Thresholds::default(),
            actor,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "invalid_argument");
    let msg = err.to_string();
    assert!(
        msg.contains("production") && msg.contains("staging"),
        "{msg}"
    );
    let bad = Thresholds {
        window_hours: 99,
        ..Thresholds::default()
    };
    let err = tx
        .set_policy(&app, &flag, "production", Mode::Enforce, bad, actor)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "invalid_argument");

    let first = tx
        .set_policy(
            &app,
            &flag,
            "production",
            Mode::Enforce,
            Thresholds::default(),
            actor,
        )
        .await
        .unwrap();
    assert_eq!(first.state, PolicyState::Armed);
    assert_eq!(first.flag_key, "f");

    // Tripped state survives a replacement (decision 4).
    sqlx::query("UPDATE rollback_policies SET tripped_at = now()")
        .execute(tx.conn())
        .await
        .unwrap();
    let t2 = Thresholds {
        min_evaluations: 5,
        min_errors: 2,
        max_error_ratio: 3.0,
        max_error_rate: Some(0.5),
        window_hours: 6,
        include_client_reports: true,
    };
    let second = tx
        .set_policy(
            &app,
            &flag,
            "production",
            Mode::Observe,
            t2.clone(),
            other_actor,
        )
        .await
        .unwrap();
    assert_eq!(second.policy.id, first.policy.id);
    assert_eq!(second.policy.created_by, Some(actor));
    assert_eq!(second.policy.mode, Mode::Observe);
    assert_eq!(second.policy.thresholds, t2);
    assert!(second.policy.tripped_at.is_some());
    assert_eq!(second.state, PolicyState::Tripped);
    assert_eq!(tx.list_policies(&app, None).await.unwrap().len(), 1);
    assert_eq!(
        tx.list_policies(&app, Some(&flag)).await.unwrap(),
        vec![second.clone()]
    );
    assert_eq!(
        tx.find_policy(flag.id, "production").await.unwrap(),
        Some(second.policy.clone())
    );
    assert!(tx.find_policy(flag.id, "staging").await.unwrap().is_none());
    tx.commit().await.unwrap();

    let mut tx = db.begin(b).await.unwrap();
    assert!(tx.list_policies(&app, None).await.unwrap().is_empty());
    assert!(tx
        .find_policy(flag.id, "production")
        .await
        .unwrap()
        .is_none());
    assert!(!tx
        .remove_policy(&app, &flag, "production", actor)
        .await
        .unwrap());
    tx.commit().await.unwrap();

    let mut tx = db.begin(a).await.unwrap();
    assert!(tx
        .remove_policy(&app, &flag, "production", actor)
        .await
        .unwrap());
    assert!(!tx
        .remove_policy(&app, &flag, "production", actor)
        .await
        .unwrap());
    assert_eq!(
        audit_actions(&mut tx).await,
        vec![
            "flags.rollback_policy.set",
            "flags.rollback_policy.set",
            "flags.rollback_policy.removed"
        ]
    );
    tx.commit().await.unwrap();
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn list_policies_reports_tripped_and_missing_environments(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag) = policy_fixture(&db, org, actor).await;

    let mut tx = db.begin(org).await.unwrap();
    for env in ["production", "staging"] {
        tx.set_policy(
            &app,
            &flag,
            env,
            Mode::Enforce,
            Thresholds::default(),
            actor,
        )
        .await
        .unwrap();
    }
    sqlx::query("UPDATE rollback_policies SET tripped_at = now() WHERE environment = 'production'")
        .execute(tx.conn())
        .await
        .unwrap();
    let states = |v: Vec<flags_core::rollback::PolicyView>| -> Vec<(String, PolicyState)> {
        v.into_iter()
            .map(|p| (p.policy.environment, p.state))
            .collect()
    };
    assert_eq!(
        states(tx.list_policies(&app, None).await.unwrap()),
        vec![
            ("production".into(), PolicyState::Tripped),
            ("staging".into(), PolicyState::Armed)
        ]
    );

    let app = tx
        .set_app_environments(&app, &["production".into()], actor)
        .await
        .unwrap();
    assert_eq!(
        states(tx.list_policies(&app, None).await.unwrap()),
        vec![
            ("production".into(), PolicyState::Tripped),
            ("staging".into(), PolicyState::EnvironmentMissing)
        ]
    );
    tx.commit().await.unwrap();
}

fn eval_events(n: usize, enabled: bool) -> Vec<EvalEvent> {
    (0..n)
        .map(|_| EvalEvent {
            flag_key: "f".into(),
            enabled,
            variation: None,
            environment: "production".into(),
            at: Utc::now(),
        })
        .collect()
}

fn error_events(n: usize, enabled: bool, environment: Option<&str>) -> Vec<ErrorEvent> {
    (0..n)
        .map(|_| ErrorEvent {
            flag_key: "f".into(),
            flag_enabled: enabled,
            environment: environment.map(String::from),
            error_type: "E".into(),
            error_message: "m".into(),
            stack_trace: None,
            at: Utc::now(),
        })
        .collect()
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_default_policy_is_judged_on_server_reports_only(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag) = policy_fixture(&db, org, actor).await;
    let mut tx = db.begin(org).await.unwrap();

    // Client-key reports that would trip, and nothing from a server key.
    telemetry::record_evaluations(&mut tx, app.id, KeyKind::Client, eval_events(200, true))
        .await
        .unwrap();
    telemetry::record_evaluations(&mut tx, app.id, KeyKind::Client, eval_events(200, false))
        .await
        .unwrap();
    telemetry::record_errors(
        &mut tx,
        app.id,
        KeyKind::Client,
        error_events(30, true, Some("production")),
    )
    .await
    .unwrap();
    // A row from before the reporter column existed counts as client.
    sqlx::query(
        "UPDATE flag_errors SET reporter = NULL WHERE id = (SELECT min(id) FROM flag_errors)",
    )
    .execute(tx.conn())
    .await
    .unwrap();

    let defaults = Thresholds::default();
    let (_, s) = tx.sample(flag.id, "production", &defaults).await.unwrap();
    assert_eq!(s, rollback::Sample::default());

    let with_client = Thresholds {
        include_client_reports: true,
        ..defaults.clone()
    };
    let (_, s) = tx
        .sample(flag.id, "production", &with_client)
        .await
        .unwrap();
    assert_eq!(
        s,
        rollback::Sample {
            evals_on: 200,
            evals_off: 200,
            errors_on: 30,
            errors_off: 0
        }
    );

    // Server-key rows always count.
    telemetry::record_evaluations(&mut tx, app.id, KeyKind::Server, eval_events(50, true))
        .await
        .unwrap();
    telemetry::record_errors(
        &mut tx,
        app.id,
        KeyKind::Server,
        error_events(4, false, Some("production")),
    )
    .await
    .unwrap();
    let (_, s) = tx.sample(flag.id, "production", &defaults).await.unwrap();
    assert_eq!(
        s,
        rollback::Sample {
            evals_on: 50,
            evals_off: 0,
            errors_on: 0,
            errors_off: 4
        }
    );
    let (_, s) = tx
        .sample(flag.id, "production", &with_client)
        .await
        .unwrap();
    assert_eq!(
        s,
        rollback::Sample {
            evals_on: 250,
            evals_off: 200,
            errors_on: 30,
            errors_off: 4
        }
    );
    tx.commit().await.unwrap();
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn null_environment_errors_count_for_production_only(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag) = policy_fixture(&db, org, actor).await;
    let mut tx = db.begin(org).await.unwrap();
    telemetry::record_errors(
        &mut tx,
        app.id,
        KeyKind::Server,
        error_events(3, true, None),
    )
    .await
    .unwrap();
    telemetry::record_errors(
        &mut tx,
        app.id,
        KeyKind::Server,
        error_events(2, true, Some("staging")),
    )
    .await
    .unwrap();
    let t = Thresholds::default();
    let (_, prod) = tx.sample(flag.id, "production", &t).await.unwrap();
    let (_, staging) = tx.sample(flag.id, "staging", &t).await.unwrap();
    assert_eq!(prod.errors_on, 3);
    assert_eq!(staging.errors_on, 2);
    tx.commit().await.unwrap();
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn the_sample_window_starts_at_the_top_of_the_hour(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (_, flag) = policy_fixture(&db, org, actor).await;
    let mut tx = db.begin(org).await.unwrap();
    sqlx::query(
        "INSERT INTO flag_eval_hourly (org_id, flag_id, environment, hour, enabled, count, server_count) \
         VALUES ($1, $2, 'production', date_trunc('hour', now()) - interval '1 hour', true, 40, 40)",
    )
    .bind(org)
    .bind(flag.id)
    .execute(tx.conn())
    .await
    .unwrap();

    let one = Thresholds::default();
    let (start, s) = tx.sample(flag.id, "production", &one).await.unwrap();
    assert_eq!(s.evals_on, 0);
    let expected: chrono::DateTime<Utc> = sqlx::query_scalar("SELECT date_trunc('hour', now())")
        .fetch_one(tx.conn())
        .await
        .unwrap();
    assert_eq!(start, expected);

    let two = Thresholds {
        window_hours: 2,
        ..one
    };
    let (start2, s) = tx.sample(flag.id, "production", &two).await.unwrap();
    assert_eq!(s.evals_on, 40);
    assert_eq!(start2, expected - chrono::TimeDelta::hours(1));
    tx.commit().await.unwrap();
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn armed_lists_only_flags_with_an_untripped_policy(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag) = policy_fixture(&db, org, actor).await;
    let mut tx = db.begin(org).await.unwrap();
    let other = tx
        .create_flag(
            &app,
            NewFlag {
                key: "g".into(),
                ..Default::default()
            },
            actor,
            None,
        )
        .await
        .unwrap();
    let none = tx
        .create_flag(
            &app,
            NewFlag {
                key: "h".into(),
                ..Default::default()
            },
            actor,
            None,
        )
        .await
        .unwrap();
    let t = Thresholds::default();
    // f: one armed and one tripped policy (listed once); g: only tripped.
    tx.set_policy(&app, &flag, "production", Mode::Enforce, t.clone(), actor)
        .await
        .unwrap();
    tx.set_policy(&app, &flag, "staging", Mode::Enforce, t.clone(), actor)
        .await
        .unwrap();
    tx.set_policy(&app, &other, "production", Mode::Enforce, t.clone(), actor)
        .await
        .unwrap();
    tx.set_policy(&app, &flag, "production", Mode::Observe, t, actor)
        .await
        .unwrap();
    sqlx::query("UPDATE rollback_policies SET tripped_at = now() WHERE flag_id = $1")
        .bind(other.id)
        .execute(tx.conn())
        .await
        .unwrap();
    let got = rollback::armed(&mut tx, &[flag.id, other.id, none.id, flag.id])
        .await
        .unwrap();
    assert_eq!(got, vec![flag.id]);
    assert!(rollback::armed(&mut tx, &[]).await.unwrap().is_empty());
    tx.commit().await.unwrap();
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn rollback_events_are_newest_first_and_capped(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag) = policy_fixture(&db, org, actor).await;
    let mut tx = db.begin(org).await.unwrap();
    let policy = tx
        .set_policy(
            &app,
            &flag,
            "production",
            Mode::Enforce,
            Thresholds::default(),
            actor,
        )
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO rollback_events (org_id, policy_id, flag_id, environment, outcome, observed, created_at) \
         SELECT $1, $2, $3, 'production', \
                (ARRAY['rolled_back','would_roll_back','not_entitled','entitlement_unknown'])[1 + g % 4], \
                '{}'::jsonb, now() - make_interval(mins => 250 - g) \
         FROM generate_series(1, 250) g",
    )
    .bind(org)
    .bind(policy.policy.id)
    .bind(flag.id)
    .execute(tx.conn())
    .await
    .unwrap();

    let ten = tx.rollback_events(&app, Some(&flag), 10).await.unwrap();
    assert_eq!(ten.len(), 10);
    assert!(ten.windows(2).all(|w| w[0].created_at >= w[1].created_at));
    assert_eq!(ten[0].flag_key, "f");
    assert_eq!(ten[0].outcome, rollback::Outcome::NotEntitled); // g = 250 -> array index 1 + 250 % 4 = 3
    assert_eq!(
        tx.rollback_events(&app, None, 10_000).await.unwrap().len(),
        200
    );
    assert_eq!(tx.rollback_events(&app, None, 0).await.unwrap().len(), 1);
    assert_eq!(tx.rollback_events(&app, None, -5).await.unwrap().len(), 1);
    tx.commit().await.unwrap();
}
