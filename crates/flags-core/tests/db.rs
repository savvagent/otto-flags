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
use flags_core::telemetry::{
    self, ErrorEvent, EvalEvent, ERROR_RETENTION_DAYS, EVAL_RETENTION_DAYS,
};
use flags_core::{eval, keys, Error};
use otto_tenant::ids::{OrgId, UserId};
use otto_tenant::Db;
use serde_json::json;
use sqlx::migrate::MigrateError;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Postgres};

fn meta(actor: UserId) -> ChangeMeta {
    ChangeMeta {
        actor: Some(actor),
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

/// [`as_app_unpinned`] with the maintenance marker set, as the background
/// sweeps and the checker's backstop set it.
async fn as_app_maintenance(pool: &PgPool) -> sqlx::Transaction<'static, Postgres> {
    let mut tx = as_app_unpinned(pool).await;
    sqlx::query("SET LOCAL app.maintenance = 'on'")
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

    // Unpinned otto_app without the maintenance marker reads nothing (#27).
    let mut tx = as_app_unpinned(db.pool()).await;
    for table in ["rollback_policies", "rollback_events"] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(n, 0, "{table} is readable unpinned without the marker");
    }
    let swept = sqlx::query("DELETE FROM rollback_events")
        .execute(&mut *tx)
        .await
        .unwrap();
    assert_eq!(
        swept.rows_affected(),
        0,
        "unmarked code cannot delete events"
    );
    tx.rollback().await.unwrap();

    // Unpinned otto_app with the marker, the succeeding statements, rolled
    // back at the end.
    let mut tx = as_app_maintenance(db.pool()).await;
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

    // The server share of a bucket can never exceed its total.
    let mut tx = db.begin(a).await.unwrap();
    let err = sqlx::query(
        "INSERT INTO flag_eval_hourly (org_id, flag_id, environment, hour, enabled, count, server_count) \
         VALUES ($1, $2, 'production', date_trunc('hour', now()), true, 1, 2)",
    )
    .bind(a)
    .bind(flag_a.id)
    .execute(tx.conn())
    .await
    .unwrap_err();
    assert_eq!(sqlstate(&err), "23514");
    drop(tx);

    // Both checks on the existing tables are added NOT VALID, so adding them
    // scans nothing under the table lock; they still check every new row.
    let unvalidated: Vec<String> = sqlx::query_scalar(
        "SELECT conname::text FROM pg_constraint \
         WHERE conname IN ('flag_errors_reporter_check', 'flag_eval_hourly_server_count_check') \
           AND NOT convalidated ORDER BY 1",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(
        unvalidated,
        [
            "flag_errors_reporter_check",
            "flag_eval_hourly_server_count_check"
        ]
    );

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

/// A rolled-back release must migrate cleanly against a database that a
/// newer release already migrated: the applied version it does not know is
/// ignored (`ignore_missing`), see `flags_core::MIGRATOR`.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_release_boots_against_a_database_a_newer_release_migrated(pool: PgPool) {
    let newest = flags_core::MIGRATOR
        .iter()
        .map(|m| m.version)
        .max()
        .expect("migrations are embedded");
    sqlx::query(
        "INSERT INTO _sqlx_migrations \
         (version, description, success, checksum, execution_time) \
         VALUES ($1, 'from a newer release', true, '\\xdeadbeef'::bytea, 0)",
    )
    .bind(newest + 1)
    .execute(&pool)
    .await
    .unwrap();

    flags_core::MIGRATOR.run(&pool).await.unwrap();

    // The newer release's row is left alone, not removed or re-applied.
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM _sqlx_migrations")
        .fetch_one(&pool)
        .await
        .unwrap();
    let known = flags_core::MIGRATOR
        .iter()
        .filter(|m| !m.migration_type.is_down_migration())
        .count();
    assert_eq!(rows, i64::try_from(known).unwrap() + 1);

    // The boot path (`flags_core::migrate`) uses the same migrator.
    flags_core::migrate(&db(pool).await).await.unwrap();
}

/// A newer release's migration recorded as failed still refuses to boot,
/// even though this release does not know its version.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_failed_unknown_migration_still_refuses_to_boot(pool: PgPool) {
    let newest = flags_core::MIGRATOR
        .iter()
        .map(|m| m.version)
        .max()
        .expect("migrations are embedded");
    sqlx::query(
        "INSERT INTO _sqlx_migrations \
         (version, description, success, checksum, execution_time) \
         VALUES ($1, 'failed in a newer release', false, '\\xdeadbeef'::bytea, 0)",
    )
    .bind(newest + 1)
    .execute(&pool)
    .await
    .unwrap();

    let err = flags_core::MIGRATOR.run(&pool).await.unwrap_err();
    assert!(
        matches!(err, MigrateError::Dirty(v) if v == newest + 1),
        "expected Dirty({}), got {err:?}",
        newest + 1
    );
}

/// Ignoring unknown versions must not disable integrity: a known
/// migration whose applied checksum differs still refuses to boot.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_tampered_known_migration_still_refuses_to_boot(pool: PgPool) {
    let tampered = sqlx::query(
        "UPDATE _sqlx_migrations SET checksum = '\\xdeadbeef'::bytea WHERE version = 1",
    )
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(tampered.rows_affected(), 1, "version 1 must exist");

    let err = flags_core::MIGRATOR.run(&pool).await.unwrap_err();
    assert!(
        matches!(err, MigrateError::VersionMismatch(1)),
        "expected VersionMismatch(1), got {err:?}"
    );
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
async fn error_batches_land_one_row_per_known_event(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let mut tx = db.begin(org).await.unwrap();
    let (app, _) = tx.create_app("web", &[], actor).await.unwrap();
    let mut ids = vec![];
    for key in ["f", "g"] {
        let flag = tx
            .create_flag(
                &app,
                NewFlag {
                    key: key.into(),
                    ..Default::default()
                },
                actor,
                None,
            )
            .await
            .unwrap();
        ids.push(flag.id);
    }
    let (f, g) = (ids[0], ids[1]);

    let now = Utc::now();
    let event = |key: &str, enabled: bool, message: String, at| ErrorEvent {
        flag_key: key.into(),
        flag_enabled: enabled,
        environment: Some("production".into()),
        error_type: "E".into(),
        error_message: message,
        stack_trace: Some("at main".into()),
        at,
    };
    let mut disabled = event(
        "g",
        false,
        "disabled".into(),
        now - chrono::Duration::minutes(1),
    );
    disabled.environment = None;
    disabled.stack_trace = None;
    let batch = vec![
        event("f", true, "first".into(), now),
        disabled,
        event(
            "f",
            true,
            "x".repeat(3_000),
            now + chrono::Duration::hours(1),
        ),
        event("missing", true, "lost".into(), now),
    ];
    let recorded = telemetry::record_errors(&mut tx, app.id, KeyKind::Server, batch)
        .await
        .unwrap();
    let after = Utc::now();

    assert_eq!(
        recorded.receipt,
        telemetry::Receipt {
            accepted: 3,
            unknown_flags: 1
        }
    );
    let mut expected = vec![f, g];
    expected.sort();
    assert_eq!(recorded.flags, expected);

    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM flag_errors")
        .fetch_one(tx.conn())
        .await
        .unwrap();
    assert_eq!(count, 3);

    let (environment, stack, enabled): (Option<String>, Option<String>, bool) = sqlx::query_as(
        "SELECT environment, stack_trace, flag_enabled FROM flag_errors \
         WHERE flag_id = $1 AND error_message = 'disabled'",
    )
    .bind(g)
    .fetch_one(tx.conn())
    .await
    .unwrap();
    assert_eq!((environment, stack, enabled), (None, None, false));
    let (environment, stack): (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT environment, stack_trace FROM flag_errors WHERE flag_id = $1 AND error_message = 'first'",
    )
    .bind(f)
    .fetch_one(tx.conn())
    .await
    .unwrap();
    assert_eq!(
        (environment.as_deref(), stack.as_deref()),
        (Some("production"), Some("at main"))
    );

    let (length, occurred_at): (i32, chrono::DateTime<Utc>) = sqlx::query_as(
        "SELECT length(error_message), occurred_at FROM flag_errors \
         WHERE flag_id = $1 AND error_message LIKE 'xxx%'",
    )
    .bind(f)
    .fetch_one(tx.conn())
    .await
    .unwrap();
    assert_eq!(length, 2_000);
    // A future timestamp is stored as roughly now. Compare against Rust clocks:
    // SQL now() is fixed at the transaction's start. One microsecond of slack:
    // Postgres keeps microseconds, chrono nanoseconds.
    let slack = chrono::Duration::microseconds(1);
    assert!(
        occurred_at >= now - slack && occurred_at <= after,
        "{occurred_at} not in {now}..={after}"
    );

    let reporters: Vec<Option<String>> =
        sqlx::query_scalar("SELECT DISTINCT reporter FROM flag_errors")
            .fetch_all(tx.conn())
            .await
            .unwrap();
    assert_eq!(reporters, vec![Some("server".to_string())]);
    tx.commit().await.unwrap();
}

/// Each column of a batched error insert keeps its own event's value: a
/// distinct value per field, so arrays bound in the wrong order would show.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn error_batch_columns_keep_their_own_values(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let mut tx = db.begin(org).await.unwrap();
    let (app, _) = tx.create_app("web", &[], actor).await.unwrap();
    let mut ids = vec![];
    for key in ["f", "g"] {
        let flag = tx
            .create_flag(
                &app,
                NewFlag {
                    key: key.into(),
                    ..Default::default()
                },
                actor,
                None,
            )
            .await
            .unwrap();
        ids.push(flag.id);
    }
    let (f, g) = (ids[0], ids[1]);

    // Nothing known: no rows, an honest receipt, and no error.
    for batch in [vec![], vec!["missing"]] {
        let events = batch
            .into_iter()
            .map(|key: &str| ErrorEvent {
                flag_key: key.into(),
                flag_enabled: true,
                environment: None,
                error_type: "E".into(),
                error_message: "m".into(),
                stack_trace: None,
                at: Utc::now(),
            })
            .collect::<Vec<_>>();
        let unknown = events.len();
        let recorded = telemetry::record_errors(&mut tx, app.id, KeyKind::Server, events)
            .await
            .unwrap();
        assert_eq!(
            recorded.receipt,
            telemetry::Receipt {
                accepted: 0,
                unknown_flags: unknown
            }
        );
        assert!(recorded.flags.is_empty());
    }

    let now = Utc::now();
    let earlier = now - chrono::Duration::minutes(5);
    let batch = vec![
        ErrorEvent {
            flag_key: "f".into(),
            flag_enabled: true,
            environment: Some("e".repeat(100)),
            error_type: "T".repeat(300),
            error_message: "one".into(),
            stack_trace: Some("s".repeat(9_000)),
            at: now - chrono::Duration::days(2),
        },
        ErrorEvent {
            flag_key: "g".into(),
            flag_enabled: false,
            environment: Some("staging".into()),
            error_type: "TypeTwo".into(),
            error_message: "two".into(),
            stack_trace: Some("stack two".into()),
            at: earlier,
        },
    ];
    telemetry::record_errors(&mut tx, app.id, KeyKind::Client, batch)
        .await
        .unwrap();
    let after = Utc::now();

    type Row = (
        uuid::Uuid,
        uuid::Uuid,
        Option<String>,
        bool,
        String,
        Option<String>,
        chrono::DateTime<Utc>,
        Option<String>,
    );
    let row = |message: &'static str| {
        sqlx::query_as::<_, Row>(
            "SELECT org_id, flag_id, environment, flag_enabled, error_type, stack_trace, \
                    occurred_at, reporter \
             FROM flag_errors WHERE error_message = $1",
        )
        .bind(message)
    };
    let one = row("one").fetch_one(tx.conn()).await.unwrap();
    let two = row("two").fetch_one(tx.conn()).await.unwrap();

    // Over-long fields are cut to their caps; a two-day-old timestamp is
    // stored as now.
    assert_eq!((one.0, one.1, one.3), (org.as_uuid(), f.as_uuid(), true));
    assert_eq!(one.2.as_deref().map(str::len), Some(64));
    assert_eq!(one.4.len(), 200);
    assert_eq!(one.5.as_deref().map(str::len), Some(8_000));
    let slack = chrono::Duration::microseconds(1);
    assert!(one.6 >= now - slack && one.6 <= after, "{}", one.6);
    assert_eq!(one.7.as_deref(), Some("client"));

    // A recent timestamp is kept as reported (to Postgres' microsecond).
    assert_eq!((two.0, two.1, two.3), (org.as_uuid(), g.as_uuid(), false));
    assert_eq!(two.2.as_deref(), Some("staging"));
    assert_eq!(two.4, "TypeTwo");
    assert_eq!(two.5.as_deref(), Some("stack two"));
    assert!((two.6 - earlier).abs() <= slack, "{} vs {earlier}", two.6);
    assert_eq!(two.7.as_deref(), Some("client"));
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

    // The org has a rollback policy and an event; the purge must take both.
    let mut tx = db.begin(org).await.unwrap();
    let app = tx.list_apps().await.unwrap().remove(0);
    let flag = tx.require_flag(&app, "f").await.unwrap();
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
        "INSERT INTO rollback_events (org_id, policy_id, flag_id, environment, outcome, observed) \
         VALUES ($1, $2, $3, 'production', 'rolled_back', '{}'::jsonb)",
    )
    .bind(org)
    .bind(policy.policy.id)
    .bind(flag.id)
    .execute(tx.conn())
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let body = otto_resource::webhook::body(
        uuid::Uuid::new_v4(),
        "org.deleted",
        Utc::now(),
        &json!({"org_id": org.as_uuid()}),
    );
    let event = otto_resource::webhook::parse(&body).unwrap();
    let Outcome::Applied { detail } = platform_events::apply(&db, &event).await.unwrap() else {
        panic!("first delivery should apply");
    };
    assert_eq!(detail["rollback_policies"], 1, "{detail}");
    assert_eq!(detail["rollback_events"], 1, "{detail}");
    assert_eq!(
        platform_events::apply(&db, &event).await.unwrap(),
        Outcome::Duplicate
    );
    for table in ["rollback_policies", "rollback_events"] {
        let left: i64 =
            sqlx::query_scalar(&format!("SELECT count(*) FROM {table} WHERE org_id = $1"))
                .bind(org)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(left, 0, "{table}");
    }

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
    // The audit trail goes too, here on the superuser pool (see
    // org_deleted_purges_the_audit_trail_as_otto_app for otto_app).
    let audit: i64 = sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE org_id = $1")
        .bind(org)
        .fetch_one(db.pool())
        .await
        .unwrap();
    assert_eq!(audit, 0);
    let mut tx = begin_live(&db, other, None).await.unwrap();
    assert_eq!(tx.list_apps().await.unwrap().len(), 1);
    tx.rollback().await.unwrap();
}

/// Audit rows for an org, counted on the (superuser) test pool.
async fn audit_rows(pool: &PgPool, org: OrgId) -> i64 {
    sqlx::query_scalar("SELECT count(*) FROM audit_events WHERE org_id = $1")
        .bind(org)
        .fetch_one(pool)
        .await
        .unwrap()
}

/// `otto_app` has no DELETE on the append-only audit trail, so the purge goes
/// through the guarded function; the pool's role must not matter (#21).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn org_deleted_purges_the_audit_trail_as_otto_app(pool: PgPool) {
    let db = db(pool.clone()).await;
    let (org, other) = (OrgId::new(), OrgId::new());
    let actor = UserId::new();
    for o in [org, other] {
        let mut tx = db.begin(o).await.unwrap();
        tx.create_app("web", &[], actor).await.unwrap();
        tx.commit().await.unwrap();
    }
    let (rows, other_rows) = (audit_rows(&pool, org).await, audit_rows(&pool, other).await);
    assert!(rows > 0 && other_rows > 0);

    let app_db = otto_app_db(&pool).await;
    let body = otto_resource::webhook::body(
        uuid::Uuid::new_v4(),
        "org.deleted",
        Utc::now(),
        &json!({"org_id": org.as_uuid()}),
    );
    let event = otto_resource::webhook::parse(&body).unwrap();
    let Outcome::Applied { detail } = platform_events::apply(&app_db, &event).await.unwrap() else {
        panic!("first delivery should apply");
    };
    assert_eq!(detail["audit_events"], rows, "{detail}");
    assert_eq!(audit_rows(&pool, org).await, 0);
    assert_eq!(audit_rows(&pool, other).await, other_rows);
    assert_eq!(
        platform_events::apply(&app_db, &event).await.unwrap(),
        Outcome::Duplicate
    );
    assert_eq!(audit_rows(&pool, other).await, other_rows);
    app_db.pool().close().await;
}

/// The purge function only reaches the pinned org once it is tombstoned, and
/// only the real `deleted_orgs` counts: the body names it by schema, so a
/// caller's temporary table cannot stand in for it (#21).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn audit_purge_only_reaches_deleted_orgs(pool: PgPool) {
    let db = db(pool.clone()).await;
    let (org, dead) = (OrgId::new(), OrgId::new());
    for o in [org, dead] {
        let mut tx = db.begin(o).await.unwrap();
        tx.create_app("web", &[], UserId::new()).await.unwrap();
        tx.commit().await.unwrap();
    }
    let (rows, dead_rows) = (audit_rows(&pool, org).await, audit_rows(&pool, dead).await);
    assert!(rows > 0 && dead_rows > 0);
    // Another org, tombstoned: still out of reach from a transaction pinned
    // to `org`.
    sqlx::query("INSERT INTO deleted_orgs (org_id) VALUES ($1)")
        .bind(dead)
        .execute(&pool)
        .await
        .unwrap();

    let app_db = otto_app_db(&pool).await;
    let mut tx = app_db.begin(org).await.unwrap();
    let purged: i64 = sqlx::query_scalar("SELECT purge_deleted_org_audit($1)")
        .bind(org)
        .fetch_one(tx.conn())
        .await
        .unwrap();
    assert_eq!(purged, 0);

    sqlx::query("CREATE TEMP TABLE deleted_orgs (org_id uuid)")
        .execute(tx.conn())
        .await
        .unwrap();
    sqlx::query("INSERT INTO pg_temp.deleted_orgs (org_id) VALUES ($1)")
        .bind(org)
        .execute(tx.conn())
        .await
        .unwrap();
    let purged: i64 = sqlx::query_scalar("SELECT purge_deleted_org_audit($1)")
        .bind(org)
        .fetch_one(tx.conn())
        .await
        .unwrap();
    assert_eq!(
        purged, 0,
        "a temporary deleted_orgs must not satisfy the guard"
    );

    sqlx::query("SAVEPOINT other_org")
        .execute(tx.conn())
        .await
        .unwrap();
    let err = sqlx::query_scalar::<_, i64>("SELECT purge_deleted_org_audit($1)")
        .bind(dead)
        .fetch_one(tx.conn())
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err), "42501", "only the pinned org: {err}");
    sqlx::query("ROLLBACK TO SAVEPOINT other_org")
        .execute(tx.conn())
        .await
        .unwrap();

    let err = sqlx::query("DELETE FROM audit_events WHERE org_id = $1")
        .bind(org)
        .execute(tx.conn())
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err), "42501");
    tx.rollback().await.unwrap();

    assert_eq!(audit_rows(&pool, org).await, rows);
    assert_eq!(audit_rows(&pool, dead).await, dead_rows);
    app_db.pool().close().await;
}

/// An owner subject to row-level security would see no audit rows and purge
/// nothing in silence, so the function refuses to run for one (#21).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn audit_purge_refuses_an_owner_subject_to_rls(pool: PgPool) {
    let db = db(pool.clone()).await;
    let org = OrgId::new();
    // Roles are cluster-wide: a unique name, dropped before returning.
    let role = format!("purge_owner_{}", uuid::Uuid::new_v4().simple());
    let owner: String = sqlx::query_scalar("SELECT current_user::text")
        .fetch_one(&pool)
        .await
        .unwrap();
    for statement in [
        format!("CREATE ROLE {role} NOLOGIN NOSUPERUSER NOBYPASSRLS"),
        format!("ALTER FUNCTION purge_deleted_org_audit(uuid) OWNER TO {role}"),
    ] {
        sqlx::query(&statement).execute(&pool).await.unwrap();
    }

    let mut tx = db.begin(org).await.unwrap();
    let err = sqlx::query_scalar::<_, i64>("SELECT purge_deleted_org_audit($1)")
        .bind(org)
        .fetch_one(tx.conn())
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("exempt from row-level security"),
        "{err}"
    );
    tx.rollback().await.unwrap();

    for statement in [
        format!("ALTER FUNCTION purge_deleted_org_audit(uuid) OWNER TO \"{owner}\""),
        format!("DROP ROLE {role}"),
    ] {
        sqlx::query(&statement).execute(&pool).await.unwrap();
    }
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn old_rollback_events_are_swept(pool: PgPool) {
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
    for days in [91, 1] {
        sqlx::query(
            "INSERT INTO rollback_events (org_id, policy_id, flag_id, environment, outcome, observed, created_at) \
             VALUES ($1, $2, $3, 'production', 'rolled_back', '{}'::jsonb, now() - make_interval(days => $4))",
        )
        .bind(org)
        .bind(policy.policy.id)
        .bind(flag.id)
        .bind(days)
        .execute(tx.conn())
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();

    assert_eq!(telemetry::sweep(&db).await.unwrap(), 1);
    let ages: Vec<bool> = sqlx::query_scalar(
        "SELECT created_at < now() - interval '2 days' FROM rollback_events WHERE org_id = $1",
    )
    .bind(org)
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(ages, vec![false]);
}

/// A `Db` whose every connection runs as `otto_app`, so unpinned statements
/// are subject to RLS (the test pool's superuser bypasses it). `Db::begin`
/// still pins and works. Close `.pool()` before the test returns so sqlx can
/// drop the test database.
async fn otto_app_db(pool: &PgPool) -> Db {
    let app_pool = PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|conn, _meta| {
            Box::pin(async move {
                sqlx::query("SET ROLE otto_app").execute(conn).await?;
                Ok(())
            })
        })
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    Db::from_pool(app_pool)
}

/// The unpinned read policies need the maintenance marker as well as an
/// unpinned transaction: a request path that forgot to pin (current_org()
/// NULL) still reads nothing from any of these tables (#27).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn unpinned_reads_need_the_maintenance_marker(pool: PgPool) {
    let admin = db(pool.clone()).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag) = policy_fixture(&admin, org, actor).await;
    let mut tx = admin.begin(org).await.unwrap();
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
    telemetry::record_evaluations(&mut tx, app.id, KeyKind::Server, eval_events(1, true))
        .await
        .unwrap();
    telemetry::record_errors(
        &mut tx,
        app.id,
        KeyKind::Server,
        error_events(1, true, Some("production")),
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO rollback_events (org_id, policy_id, flag_id, environment, outcome, observed) \
         VALUES ($1, $2, $3, 'production', 'rolled_back', '{}'::jsonb)",
    )
    .bind(org)
    .bind(policy.policy.id)
    .bind(flag.id)
    .execute(tx.conn())
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO platform_events (event_id, org_id, kind) VALUES ($1, $2, 'member.removed')",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(org)
    .execute(tx.conn())
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let tables = [
        "rollback_policies",
        "rollback_events",
        "flag_errors",
        "flag_eval_hourly",
        "platform_events",
    ];
    let count = |table: &'static str, marked: bool| {
        let pool = pool.clone();
        async move {
            let mut tx = if marked {
                as_app_maintenance(&pool).await
            } else {
                as_app_unpinned(&pool).await
            };
            let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            tx.rollback().await.unwrap();
            n
        }
    };
    for table in tables {
        assert_eq!(count(table, false).await, 0, "{table} without the marker");
        assert_eq!(count(table, true).await, 1, "{table} with the marker");
    }
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn unpinned_sweeps_delete_as_otto_app(pool: PgPool) {
    let admin = db(pool.clone()).await;
    let (org, other) = (OrgId::new(), OrgId::new());
    let actor = UserId::new();
    let (app, flag) = policy_fixture(&admin, org, actor).await;
    let mut tx = admin.begin(org).await.unwrap();
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
    tx.commit().await.unwrap();

    // One expired and one fresh row per swept table. The platform sweep's
    // retention is the `keep_days` passed to it.
    let keep_days = 30;
    for hours in [(ERROR_RETENTION_DAYS + 1) * 24, 1] {
        sqlx::query(
            "INSERT INTO flag_errors (org_id, flag_id, flag_enabled, error_type, error_message, \
             occurred_at, received_at) \
             VALUES ($1, $2, true, 'e', 'm', now(), now() - make_interval(hours => $3))",
        )
        .bind(org)
        .bind(flag.id)
        .bind(hours)
        .execute(&pool)
        .await
        .unwrap();
    }
    for days in [EVAL_RETENTION_DAYS + 1, 0] {
        sqlx::query(
            "INSERT INTO flag_eval_hourly (org_id, flag_id, environment, hour, enabled, count, server_count) \
             VALUES ($1, $2, 'production', date_trunc('hour', now()) - make_interval(days => $3), true, 1, 1)",
        )
        .bind(org)
        .bind(flag.id)
        .bind(days)
        .execute(&pool)
        .await
        .unwrap();
    }
    for days in [EVAL_RETENTION_DAYS + 1, 1] {
        sqlx::query(
            "INSERT INTO rollback_events (org_id, policy_id, flag_id, environment, outcome, observed, created_at) \
             VALUES ($1, $2, $3, 'production', 'rolled_back', '{}'::jsonb, now() - make_interval(days => $4))",
        )
        .bind(org)
        .bind(policy.policy.id)
        .bind(flag.id)
        .bind(days)
        .execute(&pool)
        .await
        .unwrap();
    }
    for days in [keep_days + 1, 1] {
        sqlx::query(
            "INSERT INTO platform_events (event_id, org_id, kind, received_at) \
             VALUES ($1, $2, 'member.removed', now() - make_interval(days => $3))",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(org)
        .bind(days)
        .execute(&pool)
        .await
        .unwrap();
    }

    let app_db = otto_app_db(&pool).await;
    assert_eq!(telemetry::sweep(&app_db).await.unwrap(), 3);
    assert_eq!(platform_events::sweep(&app_db, keep_days).await.unwrap(), 1);

    for (table, column, cutoff_days) in [
        ("flag_errors", "received_at", ERROR_RETENTION_DAYS),
        ("flag_eval_hourly", "hour", EVAL_RETENTION_DAYS),
        ("rollback_events", "created_at", EVAL_RETENTION_DAYS),
        ("platform_events", "received_at", keep_days),
    ] {
        let (total, expired): (i64, i64) = sqlx::query_as(&format!(
            "SELECT count(*), count(*) FILTER (WHERE {column} < now() - make_interval(days => {cutoff_days})) \
             FROM {table}"
        ))
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((total, expired), (1, 0), "{table}");
    }

    // The new policies widen nothing pinned: a fresh row of the other org in
    // every swept table must stay invisible to a transaction pinned to `org`.
    let (other_app, other_flag) = policy_fixture(&admin, other, actor).await;
    let mut tx = admin.begin(other).await.unwrap();
    let other_policy = tx
        .set_policy(
            &other_app,
            &other_flag,
            "production",
            Mode::Enforce,
            Thresholds::default(),
            actor,
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    sqlx::query(
        "INSERT INTO flag_errors (org_id, flag_id, flag_enabled, error_type, error_message, occurred_at) \
         VALUES ($1, $2, true, 'e', 'm', now())",
    )
    .bind(other)
    .bind(other_flag.id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO flag_eval_hourly (org_id, flag_id, environment, hour, enabled, count, server_count) \
         VALUES ($1, $2, 'production', date_trunc('hour', now()), true, 1, 1)",
    )
    .bind(other)
    .bind(other_flag.id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO rollback_events (org_id, policy_id, flag_id, environment, outcome, observed) \
         VALUES ($1, $2, $3, 'production', 'rolled_back', '{}'::jsonb)",
    )
    .bind(other)
    .bind(other_policy.policy.id)
    .bind(other_flag.id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO platform_events (event_id, org_id, kind) VALUES ($1, $2, 'member.removed')",
    )
    .bind(uuid::Uuid::new_v4())
    .bind(other)
    .execute(&pool)
    .await
    .unwrap();
    let mut tx = app_db.begin(org).await.unwrap();
    for table in [
        "flag_errors",
        "flag_eval_hourly",
        "rollback_events",
        "platform_events",
    ] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(tx.conn())
            .await
            .unwrap();
        assert_eq!(n, 1, "{table} pinned to the first org");
    }
    tx.rollback().await.unwrap();

    app_db.pool().close().await;
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

/// How close to the next hour a test that records telemetry and then samples
/// it may start. More than any such test takes to run.
const HOUR_MARGIN_SECS: f64 = 15.0;

/// A rollback sample starts at the top of the current hour (the database's
/// clock), so telemetry recorded just before an hour turns and sampled just
/// after it falls out of the window. Called before arranging such a test: if
/// the database clock is within [`HOUR_MARGIN_SECS`] of the next hour, wait
/// until that hour has begun.
async fn clear_of_the_hour(db: &Db) {
    let left: f64 = sqlx::query_scalar(
        "SELECT extract(epoch FROM date_trunc('hour', now()) + interval '1 hour' - now())::float8",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    if left < HOUR_MARGIN_SECS {
        tokio::time::sleep(std::time::Duration::from_secs_f64(left + 1.0)).await;
    }
}

/// An org with one app (`production`, `staging`) and one flag `f`, clear of
/// the top of the hour (see [`clear_of_the_hour`]).
async fn policy_fixture(
    db: &Db,
    org: OrgId,
    actor: UserId,
) -> (flags_core::apps::FlagApp, flags_core::flags::FeatureFlag) {
    clear_of_the_hour(db).await;
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

    // A flag from another app is refused.
    let mut stray = flag.clone();
    stray.app_id = flags_core::ids::FlagAppId::new();
    let err = tx
        .set_policy(
            &app,
            &stray,
            "production",
            Mode::Enforce,
            Thresholds::default(),
            actor,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "invalid_argument");

    // Tripped state survives a replacement (decision 4).
    sqlx::query("UPDATE rollback_policies SET last_checked_at = now() - interval '5 minutes'")
        .execute(tx.conn())
        .await
        .unwrap();
    let checked: Option<chrono::DateTime<Utc>> =
        sqlx::query_scalar("SELECT last_checked_at FROM rollback_policies")
            .fetch_one(tx.conn())
            .await
            .unwrap();
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
    assert_eq!(second.policy.last_checked_at, checked);
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
    assert!(!tx.remove_policy(&flag, "production", actor).await.unwrap());
    tx.commit().await.unwrap();

    let mut tx = db.begin(a).await.unwrap();
    assert!(tx.remove_policy(&flag, "production", actor).await.unwrap());
    assert!(!tx.remove_policy(&flag, "production", actor).await.unwrap());
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

/// Errors are windowed like evaluations, and counted for their own
/// environment. Stamped from the database clock, as the window is.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn errors_outside_the_window_or_another_environment_do_not_count(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (_, flag) = policy_fixture(&db, org, actor).await;
    let mut tx = db.begin(org).await.unwrap();
    // In the previous hour; this hour; this hour but staging.
    sqlx::query(
        "INSERT INTO flag_errors \
           (org_id, flag_id, environment, flag_enabled, error_type, error_message, \
            occurred_at, reporter) \
         VALUES ($1, $2, 'production', true, 'E', 'm', \
                 date_trunc('hour', now()) - interval '30 minutes', 'server'), \
                ($1, $2, 'production', true, 'E', 'm', now(), 'server'), \
                ($1, $2, 'staging', true, 'E', 'm', now(), 'server')",
    )
    .bind(org)
    .bind(flag.id)
    .execute(tx.conn())
    .await
    .unwrap();

    let one = Thresholds::default();
    let two = Thresholds {
        window_hours: 2,
        ..one.clone()
    };
    let errors_on = |s: rollback::Sample| s.errors_on;
    assert_eq!(
        errors_on(tx.sample(flag.id, "production", &one).await.unwrap().1),
        1,
        "the previous hour is outside a 1-hour window, and staging is not production"
    );
    assert_eq!(
        errors_on(tx.sample(flag.id, "production", &two).await.unwrap().1),
        2,
        "a 2-hour window reaches the previous hour"
    );
    assert_eq!(
        errors_on(tx.sample(flag.id, "staging", &two).await.unwrap().1),
        1
    );
    tx.rollback().await.unwrap();
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
    let all = [flag.id, other.id, none.id, flag.id];
    let got = rollback::armed(&mut tx, &all, KeyKind::Server)
        .await
        .unwrap();
    assert_eq!(got, vec![flag.id]);
    assert!(rollback::armed(&mut tx, &[], KeyKind::Server)
        .await
        .unwrap()
        .is_empty());
    // Client-key telemetry hints only flags whose armed policy counts it.
    assert!(rollback::armed(&mut tx, &all, KeyKind::Client)
        .await
        .unwrap()
        .is_empty());
    let counts_clients = Thresholds {
        include_client_reports: true,
        ..Thresholds::default()
    };
    tx.set_policy(&app, &flag, "staging", Mode::Enforce, counts_clients, actor)
        .await
        .unwrap();
    assert_eq!(
        rollback::armed(&mut tx, &all, KeyKind::Client)
            .await
            .unwrap(),
        vec![flag.id]
    );
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

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn every_outcome_text_satisfies_the_events_check(pool: PgPool) {
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
    let all = [
        rollback::Outcome::RolledBack,
        rollback::Outcome::WouldRollBack,
        rollback::Outcome::NotEntitled,
        rollback::Outcome::EntitlementUnknown,
    ];
    for o in all {
        sqlx::query(
            "INSERT INTO rollback_events (org_id, policy_id, flag_id, environment, outcome, observed) \
             VALUES ($1, $2, $3, 'production', $4, '{}'::jsonb)",
        )
        .bind(org)
        .bind(policy.policy.id)
        .bind(flag.id)
        .bind(o.as_str())
        .execute(tx.conn())
        .await
        .unwrap();
    }
    let mut got: Vec<_> = tx
        .rollback_events(&app, None, 10)
        .await
        .unwrap()
        .into_iter()
        .map(|e| e.outcome.as_str())
        .collect();
    got.sort();
    let mut want: Vec<_> = all.iter().map(|o| o.as_str()).collect();
    want.sort();
    assert_eq!(got, want);
    tx.commit().await.unwrap();
}

/// A flag write in progress does not block telemetry for that flag: the
/// write's row lock is FOR NO KEY UPDATE, which the telemetry insert's FK
/// check (FOR KEY SHARE) does not wait on.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_flag_write_in_progress_does_not_block_its_telemetry(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, _) = policy_fixture(&db, org, actor).await;

    // Holds the flag's row lock until it ends.
    let mut writer = db.begin(org).await.unwrap();
    writer
        .set_environment(&app, "f", "production", on(true), meta(actor))
        .await
        .unwrap();

    let mut tx = db.begin(org).await.unwrap();
    sqlx::query("SET LOCAL lock_timeout = '1s'")
        .execute(tx.conn())
        .await
        .unwrap();
    telemetry::record_errors(
        &mut tx,
        app.id,
        KeyKind::Server,
        error_events(1, true, Some("production")),
    )
    .await
    .expect("telemetry waited on the flag write's lock");
    telemetry::record_evaluations(&mut tx, app.id, KeyKind::Server, eval_events(1, true))
        .await
        .expect("telemetry waited on the flag write's lock");
    tx.commit().await.unwrap();
    writer.commit().await.unwrap();
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn turning_an_environment_back_on_rearms_its_policy(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag) = policy_fixture(&db, org, actor).await;
    let on = |enabled| EnvPatch {
        enabled: Some(enabled),
        ..Default::default()
    };

    let mut tx = db.begin(org).await.unwrap();
    tx.set_policy(
        &app,
        &flag,
        "production",
        Mode::Enforce,
        Thresholds::default(),
        actor,
    )
    .await
    .unwrap();
    async fn tripped(tx: &mut otto_tenant::Tx<'_>) -> bool {
        sqlx::query_scalar("SELECT tripped_at IS NOT NULL FROM rollback_policies")
            .fetch_one(tx.conn())
            .await
            .unwrap()
    }
    async fn trip(tx: &mut otto_tenant::Tx<'_>) {
        sqlx::query("UPDATE rollback_policies SET tripped_at = now()")
            .execute(tx.conn())
            .await
            .unwrap();
    }

    tx.set_environment(&app, "f", "production", on(true), meta(actor))
        .await
        .unwrap();
    tx.set_environment(&app, "f", "production", on(false), meta(actor))
        .await
        .unwrap();
    trip(&mut tx).await;

    // Another environment going on does not re-arm production.
    tx.set_environment(&app, "f", "staging", on(true), meta(actor))
        .await
        .unwrap();
    assert!(tripped(&mut tx).await);

    // A change that leaves `enabled` alone does not either.
    tx.update_flag(
        &app,
        "f",
        FlagPatch {
            name: Some("renamed".into()),
            ..Default::default()
        },
        meta(actor),
    )
    .await
    .unwrap();
    assert!(tripped(&mut tx).await);

    // Turning production back on does.
    let v = tx
        .set_environment(&app, "f", "production", on(true), meta(actor))
        .await
        .unwrap()
        .version;
    assert!(!tripped(&mut tx).await);

    // Trip again, then roll back to a version where production was on.
    trip(&mut tx).await;
    tx.set_environment(&app, "f", "production", on(false), meta(actor))
        .await
        .unwrap();
    trip(&mut tx).await;
    tx.rollback_flag(&app, "f", Some(v), meta(actor))
        .await
        .unwrap();
    assert!(!tripped(&mut tx).await);
    tx.commit().await.unwrap();
}

// ---- the checker: acting on one flag

use flags_core::entitlements::Entitlement;
use flags_core::rollback::{check_flag, due_policies, Outcome as RollbackOutcome};

fn on(enabled: bool) -> EnvPatch {
    EnvPatch {
        enabled: Some(enabled),
        ..Default::default()
    }
}

/// `policy_fixture` with the given environments switched on and a policy in
/// `mode` on production.
async fn checker_fixture(
    db: &Db,
    org: OrgId,
    actor: UserId,
    enabled: &[&str],
    mode: Mode,
) -> (
    flags_core::apps::FlagApp,
    flags_core::flags::FeatureFlag,
    uuid::Uuid,
) {
    let (app, _) = policy_fixture(db, org, actor).await;
    let mut tx = db.begin(org).await.unwrap();
    for env in enabled {
        tx.set_environment(&app, "f", env, on(true), meta(actor))
            .await
            .unwrap();
    }
    let flag = tx.require_flag(&app, "f").await.unwrap();
    let policy = tx
        .set_policy(
            &app,
            &flag,
            "production",
            mode,
            Thresholds::default(),
            actor,
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    (app, flag, policy.policy.id)
}

/// Server-key telemetry on production that trips the default thresholds:
/// 200 evaluations each side, 30 errors with the flag on, none off.
async fn tripping_telemetry(db: &Db, org: OrgId, app: &flags_core::apps::FlagApp) {
    let mut tx = db.begin(org).await.unwrap();
    for enabled in [true, false] {
        telemetry::record_evaluations(&mut tx, app.id, KeyKind::Server, eval_events(200, enabled))
            .await
            .unwrap();
    }
    telemetry::record_errors(
        &mut tx,
        app.id,
        KeyKind::Server,
        error_events(30, true, Some("production")),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
}

#[derive(Debug)]
struct CheckerState {
    version: i32,
    versions: i64,
    production: bool,
    staging: bool,
    tripped: bool,
    checked: bool,
    events: Vec<(String, Option<i32>)>,
}

async fn checker_state(db: &Db, org: OrgId, app: &flags_core::apps::FlagApp) -> CheckerState {
    let mut tx = db.begin(org).await.unwrap();
    let flag = tx.require_flag(app, "f").await.unwrap();
    let enabled = |env: &str| flag.environments[env]["enabled"].as_bool().unwrap_or(false);
    let versions: i64 =
        sqlx::query_scalar("SELECT count(*) FROM flag_versions WHERE org_id = $1 AND flag_id = $2")
            .bind(org)
            .bind(flag.id)
            .fetch_one(tx.conn())
            .await
            .unwrap();
    let (tripped, checked): (bool, bool) = sqlx::query_as(
        "SELECT tripped_at IS NOT NULL, last_checked_at IS NOT NULL FROM rollback_policies \
         WHERE org_id = $1 AND flag_id = $2 AND environment = 'production'",
    )
    .bind(org)
    .bind(flag.id)
    .fetch_one(tx.conn())
    .await
    .unwrap();
    let events: Vec<(String, Option<i32>)> = sqlx::query_as(
        "SELECT outcome, flag_version FROM rollback_events WHERE org_id = $1 ORDER BY id",
    )
    .bind(org)
    .fetch_all(tx.conn())
    .await
    .unwrap();
    let state = CheckerState {
        version: flag.version,
        versions,
        production: enabled("production"),
        staging: enabled("staging"),
        tripped,
        checked,
        events,
    };
    tx.rollback().await.unwrap();
    state
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_tripping_policy_disables_only_its_environment(pool: PgPool) {
    let db = db(pool.clone()).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag, policy) =
        checker_fixture(&db, org, actor, &["production", "staging"], Mode::Enforce).await;
    tripping_telemetry(&db, org, &app).await;
    let before = checker_state(&db, org, &app).await;
    assert!(before.production && before.staging);

    let mut listener = sqlx::postgres::PgListener::connect_with(&pool)
        .await
        .unwrap();
    listener.listen(flags_core::notify::CHANNEL).await.unwrap();

    let report = check_flag(&db, &Entitlement::Granted, org, flag.id)
        .await
        .unwrap();
    assert_eq!(report.checked, 1);
    assert_eq!(report.outcomes, vec![(policy, RollbackOutcome::RolledBack)]);

    let after = checker_state(&db, org, &app).await;
    assert_eq!(after.version, before.version + 1);
    assert_eq!(after.versions, before.versions + 1);
    assert!(!after.production, "production must be off");
    assert!(after.staging, "staging must be untouched");
    assert!(after.tripped);
    assert!(after.checked);
    assert_eq!(
        after.events,
        vec![("rolled_back".to_string(), Some(after.version))]
    );

    let mut tx = db.begin(org).await.unwrap();
    let (change, actor_id, reason): (String, Option<uuid::Uuid>, Option<String>) = sqlx::query_as(
        "SELECT change, actor_user_id, reason FROM flag_versions \
         WHERE org_id = $1 AND flag_id = $2 AND version = $3",
    )
    .bind(org)
    .bind(flag.id)
    .bind(after.version)
    .fetch_one(tx.conn())
    .await
    .unwrap();
    assert_eq!(change, "auto_rollback");
    assert_eq!(actor_id, None);
    let reason = reason.expect("an auto-rollback names its cause");
    assert!(
        reason.starts_with(&format!(
            "auto-rollback by policy {policy} (offHasNoErrors): error rate on 15.00% vs off 0.00%"
        )),
        "{reason}"
    );
    assert!(reason.contains("over 400 evaluations"), "{reason}");
    let observed: serde_json::Value =
        sqlx::query_scalar("SELECT observed FROM rollback_events WHERE org_id = $1")
            .bind(org)
            .fetch_one(tx.conn())
            .await
            .unwrap();
    assert_eq!(observed["sample"]["errorsOn"], 30);
    assert_eq!(observed["decision"], "trip");
    assert_eq!(observed["trigger"], "offHasNoErrors");
    tx.rollback().await.unwrap();

    let note = tokio::time::timeout(std::time::Duration::from_secs(5), listener.recv())
        .await
        .expect("a change notification after commit")
        .unwrap();
    let change: serde_json::Value = serde_json::from_str(note.payload()).unwrap();
    assert_eq!(change["key"], "f");
    assert_eq!(change["version"], after.version);
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_tripped_policy_does_not_act_again(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag, _) = checker_fixture(&db, org, actor, &["production"], Mode::Enforce).await;
    tripping_telemetry(&db, org, &app).await;
    check_flag(&db, &Entitlement::Granted, org, flag.id)
        .await
        .unwrap();
    let first = checker_state(&db, org, &app).await;

    let report = check_flag(&db, &Entitlement::Granted, org, flag.id)
        .await
        .unwrap();
    assert_eq!(report.checked, 0);
    assert!(report.outcomes.is_empty());
    let second = checker_state(&db, org, &app).await;
    assert_eq!(second.version, first.version);
    assert_eq!(second.versions, first.versions);
    assert_eq!(second.events.len(), 1);
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn observe_mode_records_once_an_hour_and_changes_nothing(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag, policy) =
        checker_fixture(&db, org, actor, &["production"], Mode::Observe).await;
    tripping_telemetry(&db, org, &app).await;
    let before = checker_state(&db, org, &app).await;

    for _ in 0..2 {
        let report = check_flag(&db, &Entitlement::Granted, org, flag.id)
            .await
            .unwrap();
        assert_eq!(
            report.outcomes,
            vec![(policy, RollbackOutcome::WouldRollBack)]
        );
    }
    let after = checker_state(&db, org, &app).await;
    assert_eq!(after.version, before.version);
    assert!(after.production);
    assert!(!after.tripped);
    assert!(after.checked);
    assert_eq!(after.events, vec![("would_roll_back".to_string(), None)]);
}

async fn assert_suppressed(pool: PgPool, answer: Entitlement, outcome: RollbackOutcome) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag, policy) =
        checker_fixture(&db, org, actor, &["production"], Mode::Enforce).await;
    tripping_telemetry(&db, org, &app).await;
    let before = checker_state(&db, org, &app).await;

    for _ in 0..2 {
        let report = check_flag(&db, &answer, org, flag.id).await.unwrap();
        assert_eq!(report.outcomes, vec![(policy, outcome)]);
    }
    let after = checker_state(&db, org, &app).await;
    assert_eq!(after.version, before.version);
    assert_eq!(after.versions, before.versions);
    assert!(after.production);
    assert!(!after.tripped);
    assert!(after.checked);
    assert_eq!(after.events, vec![(outcome.as_str().to_string(), None)]);
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn an_unentitled_org_records_but_does_not_act(pool: PgPool) {
    assert_suppressed(
        pool,
        Entitlement::Denied {
            plan: "Free".into(),
        },
        RollbackOutcome::NotEntitled,
    )
    .await;
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn an_unknown_entitlement_fails_closed(pool: PgPool) {
    assert_suppressed(
        pool,
        Entitlement::Unknown,
        RollbackOutcome::EntitlementUnknown,
    )
    .await;
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_healthy_flag_only_advances_last_checked(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag, _) = checker_fixture(&db, org, actor, &["production"], Mode::Enforce).await;
    // Plenty of traffic, errors at the same rate on both sides.
    let mut tx = db.begin(org).await.unwrap();
    for enabled in [true, false] {
        telemetry::record_evaluations(&mut tx, app.id, KeyKind::Server, eval_events(200, enabled))
            .await
            .unwrap();
        telemetry::record_errors(
            &mut tx,
            app.id,
            KeyKind::Server,
            error_events(12, enabled, Some("production")),
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    let before = checker_state(&db, org, &app).await;
    assert!(!before.checked);

    let platform = Scripted::new(&db, org, &app, During::Nothing);
    let report = check_flag(&db, &platform, org, flag.id).await.unwrap();
    assert_eq!(
        platform.calls(),
        0,
        "a healthy flag never asks the platform"
    );
    assert_eq!(report.checked, 1);
    assert!(report.outcomes.is_empty());
    let after = checker_state(&db, org, &app).await;
    assert!(after.checked);
    assert!(!after.tripped);
    assert!(after.production);
    assert_eq!(after.version, before.version);
    assert!(after.events.is_empty());
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn an_already_disabled_environment_is_marked_tripped_without_a_version(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag, policy) = checker_fixture(&db, org, actor, &["staging"], Mode::Enforce).await;
    tripping_telemetry(&db, org, &app).await;
    let before = checker_state(&db, org, &app).await;
    assert!(!before.production);

    let report = check_flag(&db, &Entitlement::Granted, org, flag.id)
        .await
        .unwrap();
    assert_eq!(report.outcomes, vec![(policy, RollbackOutcome::RolledBack)]);
    let after = checker_state(&db, org, &app).await;
    assert!(after.tripped);
    assert_eq!(after.version, before.version);
    assert_eq!(after.versions, before.versions);
    assert!(after.staging);
    assert_eq!(after.events, vec![("rolled_back".to_string(), None)]);
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn archived_flags_and_missing_environments_are_skipped(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag, _) = checker_fixture(&db, org, actor, &["production"], Mode::Enforce).await;
    tripping_telemetry(&db, org, &app).await;

    // Archived: skipped, no event, but checked.
    let mut tx = db.begin(org).await.unwrap();
    tx.set_archived(&app, "f", true, meta(actor)).await.unwrap();
    tx.commit().await.unwrap();
    let before = checker_state(&db, org, &app).await;
    let report = check_flag(&db, &Entitlement::Granted, org, flag.id)
        .await
        .unwrap();
    assert!(report.outcomes.is_empty());
    let after = checker_state(&db, org, &app).await;
    assert!(after.checked);
    assert!(!after.tripped);
    assert!(after.events.is_empty());
    assert_eq!(after.version, before.version);

    // Restored, but production is no longer one of the app's environments.
    let mut tx = db.begin(org).await.unwrap();
    tx.set_archived(&app, "f", false, meta(actor))
        .await
        .unwrap();
    tx.set_app_environments(&app, &["staging".into()], actor)
        .await
        .unwrap();
    sqlx::query("UPDATE rollback_policies SET last_checked_at = NULL")
        .execute(tx.conn())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let before = checker_state(&db, org, &app).await;
    let report = check_flag(&db, &Entitlement::Granted, org, flag.id)
        .await
        .unwrap();
    assert!(report.outcomes.is_empty());
    let after = checker_state(&db, org, &app).await;
    assert!(after.checked);
    assert!(!after.tripped);
    assert!(after.production, "a missing environment is never patched");
    assert!(after.events.is_empty());
    assert_eq!(after.version, before.version);
}

/// The real statement, unpinned, as `otto_app`: the
/// `rollback_policies_checker` row-level policy must let it see every org.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn due_policies_sees_every_org_as_otto_app(pool: PgPool) {
    let admin = db(pool.clone()).await;
    let (a, b) = (OrgId::new(), OrgId::new());
    let actor = UserId::new();
    let (_, flag_a, _) = checker_fixture(&admin, a, actor, &[], Mode::Enforce).await;
    let (_, flag_b, _) = checker_fixture(&admin, b, actor, &[], Mode::Enforce).await;

    let app_db = otto_app_db(&pool).await;
    let due = due_policies(&app_db, std::time::Duration::from_secs(300), 200)
        .await
        .unwrap();
    assert_eq!(due.len(), 2, "{due:?}");
    assert!(due.contains(&(a, flag_a.id)) && due.contains(&(b, flag_b.id)));
    app_db.pool().close().await;
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn due_policies_sees_every_org_unpinned(pool: PgPool) {
    let db = db(pool).await;
    let (a, b) = (OrgId::new(), OrgId::new());
    let actor = UserId::new();
    let (_, flag_a, _) = checker_fixture(&db, a, actor, &[], Mode::Enforce).await;
    let (_, flag_b, _) = checker_fixture(&db, b, actor, &[], Mode::Enforce).await;
    let five_minutes = std::time::Duration::from_secs(300);

    let due = due_policies(&db, five_minutes, 200).await.unwrap();
    assert_eq!(due.len(), 2, "{due:?}");
    assert!(due.contains(&(a, flag_a.id)) && due.contains(&(b, flag_b.id)));
    assert_eq!(due_policies(&db, five_minutes, 1).await.unwrap().len(), 1);

    // Checked just now: no longer due.
    check_flag(&db, &Entitlement::Granted, a, flag_a.id)
        .await
        .unwrap();
    assert_eq!(
        due_policies(&db, five_minutes, 200).await.unwrap(),
        vec![(b, flag_b.id)]
    );
    // Unless the caller asks for anything older than nothing at all.
    assert_eq!(
        due_policies(&db, std::time::Duration::ZERO, 200)
            .await
            .unwrap()
            .len(),
        2
    );

    // Tripped policies are never due.
    let mut tx = db.begin(b).await.unwrap();
    sqlx::query("UPDATE rollback_policies SET tripped_at = now()")
        .execute(tx.conn())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert!(due_policies(&db, five_minutes, 200)
        .await
        .unwrap()
        .is_empty());
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_deleted_org_is_not_checked(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag, _) = checker_fixture(&db, org, actor, &["production"], Mode::Enforce).await;
    tripping_telemetry(&db, org, &app).await;

    let body = otto_resource::webhook::body(
        uuid::Uuid::new_v4(),
        "org.deleted",
        Utc::now(),
        &json!({"org_id": org.as_uuid()}),
    );
    let event = otto_resource::webhook::parse(&body).unwrap();
    platform_events::apply(&db, &event).await.unwrap();

    let err = check_flag(&db, &Entitlement::Granted, org, flag.id)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::AccessRevoked), "{err:?}");
    assert_eq!(err.code(), "access_revoked");
}

// ---- the checker loop

use flags_core::rollback::{Checker, CheckerConfig};

/// Spawn a checker, optionally hint the flag, and wait for the policy to trip;
/// then stop the checker and wait for it to return.
async fn run_checker_until_tripped(pool: PgPool, config: CheckerConfig, hint: bool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag, _) = checker_fixture(&db, org, actor, &["production"], Mode::Enforce).await;
    tripping_telemetry(&db, org, &app).await;

    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let (hints, task) = Checker::spawn(db.clone(), Entitlement::Granted, config, shutdown);
    if hint {
        hints.send(org, vec![flag.id]);
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !checker_state(&db, org, &app).await.tripped {
        assert!(
            std::time::Instant::now() < deadline,
            "the checker did not act"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let state = checker_state(&db, org, &app).await;
    assert!(!state.production, "production must be off");

    stop.send(true).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("the checker stops on shutdown")
        .unwrap();
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn the_checker_acts_on_a_hint(pool: PgPool) {
    // A zero backstop limit lists nothing, so only the hint can drive it.
    let config = CheckerConfig {
        backstop_limit: 0,
        ..CheckerConfig::default()
    };
    run_checker_until_tripped(pool, config, true).await;
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn the_checker_ends_when_its_shutdown_sender_is_dropped(pool: PgPool) {
    let db = db(pool).await;
    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let (_hints, task) =
        Checker::spawn(db, Entitlement::Granted, CheckerConfig::default(), shutdown);
    drop(stop);
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("the checker ends once its shutdown sender is gone")
        .unwrap();
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn the_checker_backstop_finds_unhinted_flags(pool: PgPool) {
    run_checker_until_tripped(pool, CheckerConfig::default(), false).await;
}

// ---- the checker's re-check under the flag lock

use std::sync::atomic::{AtomicUsize, Ordering};

/// What happens while the checker waits on the platform.
enum During {
    Nothing,
    /// Another checker checks the same flag and acts first.
    NestedCheck,
    /// A person turns production off.
    DisableProduction,
    /// The production policy is switched to observe.
    SwitchToObserve,
    /// The production policy is removed.
    RemovePolicy,
}

/// An `EntitlementSource` that does something to the database between the
/// checker's two transactions, then answers `Granted`, and counts its calls.
struct Scripted {
    db: Db,
    org: OrgId,
    app: flags_core::apps::FlagApp,
    actor: UserId,
    during: During,
    calls: AtomicUsize,
}

impl Scripted {
    fn new(db: &Db, org: OrgId, app: &flags_core::apps::FlagApp, during: During) -> Self {
        Self {
            db: db.clone(),
            org,
            app: app.clone(),
            actor: UserId::new(),
            during,
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl flags_core::entitlements::EntitlementSource for Scripted {
    async fn check(&self, _org: OrgId, _feature: &str) -> Entitlement {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let (db, org, app, actor) = (&self.db, self.org, &self.app, self.actor);
        match self.during {
            During::Nothing => {}
            During::NestedCheck => {
                let mut tx = db.begin(org).await.unwrap();
                let flag = tx.require_flag(app, "f").await.unwrap();
                tx.rollback().await.unwrap();
                let inner = check_flag(db, &Entitlement::Granted, org, flag.id)
                    .await
                    .unwrap();
                assert_eq!(inner.outcomes.len(), 1, "the nested check acts");
            }
            During::DisableProduction => {
                let mut tx = db.begin(org).await.unwrap();
                tx.set_environment(app, "f", "production", on(false), meta(actor))
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
            }
            During::SwitchToObserve => {
                let mut tx = db.begin(org).await.unwrap();
                let flag = tx.require_flag(app, "f").await.unwrap();
                tx.set_policy(
                    app,
                    &flag,
                    "production",
                    Mode::Observe,
                    Thresholds::default(),
                    actor,
                )
                .await
                .unwrap();
                tx.commit().await.unwrap();
            }
            During::RemovePolicy => {
                let mut tx = db.begin(org).await.unwrap();
                let flag = tx.require_flag(app, "f").await.unwrap();
                assert!(tx.remove_policy(&flag, "production", actor).await.unwrap());
                tx.commit().await.unwrap();
            }
        }
        Entitlement::Granted
    }
}

/// The versions of flag `f` recorded with change `auto_rollback`.
async fn auto_rollback_versions(db: &Db, org: OrgId) -> Vec<i32> {
    let mut tx = db.begin(org).await.unwrap();
    let v = sqlx::query_scalar(
        "SELECT version FROM flag_versions WHERE org_id = $1 AND change = 'auto_rollback' \
         ORDER BY version",
    )
    .bind(org)
    .fetch_all(tx.conn())
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    v
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn two_checkers_roll_back_at_most_once(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag, _) = checker_fixture(&db, org, actor, &["production"], Mode::Enforce).await;
    tripping_telemetry(&db, org, &app).await;

    let platform = Scripted::new(&db, org, &app, During::NestedCheck);
    let outer = check_flag(&db, &platform, org, flag.id).await.unwrap();
    assert_eq!(platform.calls(), 1);
    // The nested checker tripped the policy; the outer one finds it no longer armed.
    assert!(outer.outcomes.is_empty(), "{outer:?}");
    let after = checker_state(&db, org, &app).await;
    assert_eq!(auto_rollback_versions(&db, org).await, vec![after.version]);
    assert_eq!(
        after.events,
        vec![("rolled_back".to_string(), Some(after.version))]
    );
    assert!(after.tripped);
    assert!(!after.production);
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_person_turning_it_off_meanwhile_leaves_no_auto_version(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag, policy) =
        checker_fixture(&db, org, actor, &["production"], Mode::Enforce).await;
    tripping_telemetry(&db, org, &app).await;
    let before = checker_state(&db, org, &app).await;

    let platform = Scripted::new(&db, org, &app, During::DisableProduction);
    let report = check_flag(&db, &platform, org, flag.id).await.unwrap();
    assert_eq!(report.outcomes, vec![(policy, RollbackOutcome::RolledBack)]);
    let after = checker_state(&db, org, &app).await;
    // Only the person's version.
    assert_eq!(after.version, before.version + 1);
    assert!(auto_rollback_versions(&db, org).await.is_empty());
    assert!(after.tripped);
    assert!(!after.production);
    assert_eq!(after.events, vec![("rolled_back".to_string(), None)]);
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_policy_switched_to_observe_meanwhile_only_records(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag, policy) =
        checker_fixture(&db, org, actor, &["production"], Mode::Enforce).await;
    tripping_telemetry(&db, org, &app).await;
    let before = checker_state(&db, org, &app).await;

    let platform = Scripted::new(&db, org, &app, During::SwitchToObserve);
    let report = check_flag(&db, &platform, org, flag.id).await.unwrap();
    assert_eq!(
        report.outcomes,
        vec![(policy, RollbackOutcome::WouldRollBack)]
    );
    let after = checker_state(&db, org, &app).await;
    assert_eq!(after.version, before.version);
    assert!(after.production);
    assert!(!after.tripped);
    assert_eq!(after.events, vec![("would_roll_back".to_string(), None)]);
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_policy_removed_meanwhile_does_nothing(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag, _) = checker_fixture(&db, org, actor, &["production"], Mode::Enforce).await;
    tripping_telemetry(&db, org, &app).await;
    let before = checker_state(&db, org, &app).await;

    let platform = Scripted::new(&db, org, &app, During::RemovePolicy);
    let report = check_flag(&db, &platform, org, flag.id).await.unwrap();
    assert!(report.outcomes.is_empty());
    let mut tx = db.begin(org).await.unwrap();
    let (events, versions, enabled): (i64, i32, bool) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM rollback_events WHERE org_id = $1), \
                version, (environments->'production'->>'enabled')::boolean \
         FROM feature_flags WHERE org_id = $1 AND id = $2",
    )
    .bind(org)
    .bind(flag.id)
    .fetch_one(tx.conn())
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(events, 0);
    assert_eq!(versions, before.version);
    assert!(enabled);
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn two_tripping_policies_roll_back_both_environments_in_one_check(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag, production) =
        checker_fixture(&db, org, actor, &["production", "staging"], Mode::Enforce).await;
    let mut tx = db.begin(org).await.unwrap();
    let staging = tx
        .set_policy(
            &app,
            &flag,
            "staging",
            Mode::Enforce,
            Thresholds::default(),
            actor,
        )
        .await
        .unwrap()
        .policy
        .id;
    // The same tripping telemetry, in staging too.
    let in_staging = |mut evals: Vec<EvalEvent>| {
        for e in &mut evals {
            e.environment = "staging".into();
        }
        evals
    };
    for enabled in [true, false] {
        telemetry::record_evaluations(
            &mut tx,
            app.id,
            KeyKind::Server,
            in_staging(eval_events(200, enabled)),
        )
        .await
        .unwrap();
    }
    telemetry::record_errors(
        &mut tx,
        app.id,
        KeyKind::Server,
        error_events(30, true, Some("staging")),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    tripping_telemetry(&db, org, &app).await;
    let before = checker_state(&db, org, &app).await;

    let platform = Scripted::new(&db, org, &app, During::Nothing);
    let mut report = check_flag(&db, &platform, org, flag.id).await.unwrap();
    assert_eq!(platform.calls(), 1, "one platform call per flag");
    assert_eq!(report.checked, 2);
    report.outcomes.sort_by_key(|(id, _)| *id);
    let mut want = vec![
        (production, RollbackOutcome::RolledBack),
        (staging, RollbackOutcome::RolledBack),
    ];
    want.sort_by_key(|(id, _)| *id);
    assert_eq!(report.outcomes, want);

    let after = checker_state(&db, org, &app).await;
    assert_eq!(
        auto_rollback_versions(&db, org).await,
        vec![before.version + 1, before.version + 2]
    );
    assert!(!after.production && !after.staging);
    let mut events = after.events.clone();
    events.sort();
    assert_eq!(
        events,
        vec![
            ("rolled_back".to_string(), Some(before.version + 1)),
            ("rolled_back".to_string(), Some(before.version + 2)),
        ]
    );
    let mut tx = db.begin(org).await.unwrap();
    let armed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM rollback_policies WHERE org_id = $1 AND tripped_at IS NULL",
    )
    .bind(org)
    .fetch_one(tx.conn())
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(armed, 0);
}

// ---- the checker's deadlines and per-policy isolation

/// A check that meets the flag's row lock held by someone else gives up after
/// the checker's lock timeout, as a transient error, instead of waiting.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_check_blocked_on_the_flag_lock_times_out_as_transient(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag, policy) =
        checker_fixture(&db, org, actor, &["production"], Mode::Enforce).await;
    tripping_telemetry(&db, org, &app).await;

    // Holds the flag's row lock until it ends.
    let mut writer = db.begin(org).await.unwrap();
    writer
        .set_environment(&app, "f", "staging", on(true), meta(actor))
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let err = check_flag(&db, &Entitlement::Granted, org, flag.id)
        .await
        .unwrap_err();
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "{:?}",
        started.elapsed()
    );
    match &err {
        Error::Db(e) => assert_eq!(sqlstate(e), "55P03", "{e:?}"),
        other => panic!("expected a lock timeout, got {other:?}"),
    }
    assert!(err.is_transient());
    writer.rollback().await.unwrap();

    // Once the lock is gone the same check acts.
    let report = check_flag(&db, &Entitlement::Granted, org, flag.id)
        .await
        .unwrap();
    assert_eq!(report.outcomes, vec![(policy, RollbackOutcome::RolledBack)]);
}

/// Hangs on its first call, then grants.
struct HangsOnce {
    calls: AtomicUsize,
}

impl flags_core::entitlements::EntitlementSource for HangsOnce {
    async fn check(&self, _org: OrgId, _feature: &str) -> Entitlement {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            std::future::pending::<()>().await;
        }
        Entitlement::Granted
    }
}

/// A check that hangs is abandoned at the check deadline and retried, not
/// left to block the checker or cooled down for an hour.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_hung_check_is_abandoned_and_retried(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag, _) = checker_fixture(&db, org, actor, &["production"], Mode::Enforce).await;
    tripping_telemetry(&db, org, &app).await;

    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let config = CheckerConfig {
        debounce: std::time::Duration::from_millis(50),
        backstop_every: std::time::Duration::from_secs(1),
        check_timeout: std::time::Duration::from_millis(300),
        ..CheckerConfig::default()
    };
    let (hints, task) = Checker::spawn(
        db.clone(),
        HangsOnce {
            calls: AtomicUsize::new(0),
        },
        config,
        shutdown,
    );
    hints.send(org, vec![flag.id]);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !checker_state(&db, org, &app).await.tripped {
        assert!(
            std::time::Instant::now() < deadline,
            "the hung check was never retried"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(!checker_state(&db, org, &app).await.production);
    stop.send(true).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .expect("the checker stops on shutdown")
        .unwrap();
}

/// One policy whose action fails does not undo another policy's rollback in
/// the same check, and is reported.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn one_policys_failure_does_not_undo_anothers_rollback(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag, production) =
        checker_fixture(&db, org, actor, &["production", "staging"], Mode::Enforce).await;
    let mut tx = db.begin(org).await.unwrap();
    let staging = tx
        .set_policy(
            &app,
            &flag,
            "staging",
            Mode::Enforce,
            Thresholds::default(),
            actor,
        )
        .await
        .unwrap()
        .policy
        .id;
    for enabled in [true, false] {
        let mut evals = eval_events(200, enabled);
        for e in &mut evals {
            e.environment = "staging".into();
        }
        telemetry::record_evaluations(&mut tx, app.id, KeyKind::Server, evals)
            .await
            .unwrap();
    }
    telemetry::record_errors(
        &mut tx,
        app.id,
        KeyKind::Server,
        error_events(30, true, Some("staging")),
    )
    .await
    .unwrap();
    // Production still reads as on, but the patch refuses its stored config;
    // production sorts first, so it fails before staging acts.
    sqlx::query(
        "UPDATE feature_flags \
         SET environments = jsonb_set(environments, '{production,rules}', '\"not a list\"') \
         WHERE org_id = $1 AND id = $2",
    )
    .bind(org)
    .bind(flag.id)
    .execute(tx.conn())
    .await
    .unwrap();
    tx.commit().await.unwrap();
    tripping_telemetry(&db, org, &app).await;
    let before = checker_state(&db, org, &app).await;

    let report = check_flag(&db, &Entitlement::Granted, org, flag.id)
        .await
        .unwrap();
    assert_eq!(
        report.outcomes,
        vec![(staging, RollbackOutcome::RolledBack)]
    );
    assert_eq!(report.failed.len(), 1, "{report:?}");
    assert_eq!(report.failed[0].policy, production);
    assert_eq!(report.failed[0].code, "invalid_argument");
    assert!(!report.failed[0].transient);

    let after = checker_state(&db, org, &app).await;
    assert!(after.production, "the failed policy changed nothing");
    assert!(!after.staging, "staging was rolled back");
    assert_eq!(
        auto_rollback_versions(&db, org).await,
        vec![before.version + 1]
    );
    assert!(!after.tripped, "the failed policy is still armed");
    assert!(
        !after.checked,
        "the failed policy is not marked checked, so it is retried"
    );
    assert_eq!(
        after.events,
        vec![("rolled_back".to_string(), Some(before.version + 1))]
    );

    // The failure is visible on the failing policy only, as a code.
    let errors = |db: Db| async move {
        let mut tx = db.begin(org).await.unwrap();
        let app = tx.resolve_app("web").await.unwrap();
        let views = tx.list_policies(&app, None).await.unwrap();
        tx.rollback().await.unwrap();
        views
            .into_iter()
            .map(|v| {
                (
                    v.policy.environment,
                    v.policy.last_error_code,
                    v.policy.last_error_at.is_some(),
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        errors(db.clone()).await,
        vec![
            (
                "production".to_string(),
                Some("invalid_argument".to_string()),
                true
            ),
            ("staging".to_string(), None, false),
        ]
    );

    // Repaired, the next check acts and clears it.
    let mut tx = db.begin(org).await.unwrap();
    sqlx::query(
        "UPDATE feature_flags \
         SET environments = jsonb_set(environments, '{production,rules}', '[]') \
         WHERE org_id = $1 AND id = $2",
    )
    .bind(org)
    .bind(flag.id)
    .execute(tx.conn())
    .await
    .unwrap();
    tx.commit().await.unwrap();
    let report = check_flag(&db, &Entitlement::Granted, org, flag.id)
        .await
        .unwrap();
    assert_eq!(
        report.outcomes,
        vec![(production, RollbackOutcome::RolledBack)]
    );
    assert!(report.failed.is_empty());
    assert_eq!(
        errors(db.clone()).await,
        vec![
            ("production".to_string(), None, false),
            ("staging".to_string(), None, false),
        ]
    );
}

// ---- propose: the dry run

use flags_core::rollback::{propose, ThresholdOverrides};

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn propose_judges_the_stored_policy_with_overrides(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag, policy) =
        checker_fixture(&db, org, actor, &["production"], Mode::Enforce).await;
    tripping_telemetry(&db, org, &app).await;

    let mut tx = db.begin(org).await.unwrap();
    let p = propose(
        &mut tx,
        &app,
        &flag,
        "production",
        &ThresholdOverrides::default(),
    )
    .await
    .unwrap();
    assert_eq!(p.policy_id, Some(policy));
    assert!(p.would_trip && p.can_trip && p.can_act, "{p:?}");
    assert_eq!(p.observation.sample.errors_on, 30);
    assert!(p.note.contains("would trip"), "{}", p.note);

    // An override replaces only its field.
    let p = propose(
        &mut tx,
        &app,
        &flag,
        "production",
        &ThresholdOverrides {
            min_errors: Some(31),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(!p.would_trip);
    assert_eq!(p.observation.thresholds.min_errors, 31);
    assert_eq!(p.observation.thresholds.min_evaluations, 100);

    // An invalid override is refused like set_policy refuses it.
    let err = propose(
        &mut tx,
        &app,
        &flag,
        "production",
        &ThresholdOverrides {
            window_hours: Some(99),
            ..Default::default()
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.code(), "invalid_argument");

    // No policy: the defaults, and an unknown environment is refused with
    // the same message set_policy gives.
    let p = propose(
        &mut tx,
        &app,
        &flag,
        "staging",
        &ThresholdOverrides::default(),
    )
    .await
    .unwrap();
    assert_eq!(p.policy_id, None);
    assert_eq!(p.observation.thresholds, Thresholds::default());
    let proposed = propose(&mut tx, &app, &flag, "qa", &ThresholdOverrides::default())
        .await
        .unwrap_err()
        .to_string();
    let set = tx
        .set_policy(
            &app,
            &flag,
            "qa",
            Mode::Enforce,
            Thresholds::default(),
            actor,
        )
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(proposed, set);
    tx.rollback().await.unwrap();
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn propose_says_a_policy_that_cannot_act_would_not_trip(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, _, _) = checker_fixture(&db, org, actor, &["production"], Mode::Enforce).await;
    tripping_telemetry(&db, org, &app).await;

    // Archived.
    let mut tx = db.begin(org).await.unwrap();
    let flag = tx.set_archived(&app, "f", true, meta(actor)).await.unwrap();
    let p = propose(
        &mut tx,
        &app,
        &flag,
        "production",
        &ThresholdOverrides::default(),
    )
    .await
    .unwrap();
    assert!(!p.can_act && !p.would_trip, "{p:?}");
    assert!(matches!(
        p.observation.decision,
        rollback::Decision::Trip(_)
    ));
    assert!(p.note.contains("archived"), "{}", p.note);
    tx.rollback().await.unwrap();

    // The stored policy's environment is gone from the app.
    let mut tx = db.begin(org).await.unwrap();
    let flag = tx.require_flag(&app, "f").await.unwrap();
    let app = tx
        .set_app_environments(&app, &["staging".into()], actor)
        .await
        .unwrap();
    let p = propose(
        &mut tx,
        &app,
        &flag,
        "production",
        &ThresholdOverrides::default(),
    )
    .await
    .unwrap();
    assert!(p.policy_id.is_some());
    assert!(!p.can_act && !p.would_trip, "{p:?}");
    assert!(p.note.contains("no longer has"), "{}", p.note);
    tx.rollback().await.unwrap();
}

/// Removing a policy keeps what it did: its events stay, listed for the
/// flag, with no policy id (#28).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn removing_a_policy_keeps_its_events(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, flag, policy) =
        checker_fixture(&db, org, actor, &["production"], Mode::Enforce).await;
    tripping_telemetry(&db, org, &app).await;
    let report = check_flag(&db, &Entitlement::Granted, org, flag.id)
        .await
        .unwrap();
    assert_eq!(report.outcomes, vec![(policy, RollbackOutcome::RolledBack)]);

    let mut tx = db.begin(org).await.unwrap();
    let before = tx.rollback_events(&app, Some(&flag), 10).await.unwrap();
    assert_eq!(before.len(), 1);
    assert_eq!(before[0].policy_id, Some(policy));
    assert!(tx.remove_policy(&flag, "production", actor).await.unwrap());
    let after = tx.rollback_events(&app, Some(&flag), 10).await.unwrap();
    assert_eq!(after.len(), 1, "the event outlives its policy");
    assert_eq!(after[0].policy_id, None);
    assert_eq!(after[0].outcome, RollbackOutcome::RolledBack);
    assert_eq!(after[0].flag_version, before[0].flag_version);
    assert_eq!(tx.rollback_events(&app, None, 10).await.unwrap().len(), 1);
    tx.commit().await.unwrap();
}

/// An unpinned DELETE with no WHERE needs no SELECT policy, so 0001's
/// DELETE-only retention policies alone would let an unmarked one empty the
/// table across orgs. The restrictive maintenance_delete policies refuse it
/// without the marker; with it, or pinned to an org, deletes still work.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn unmarked_unpinned_deletes_touch_nothing(pool: PgPool) {
    let admin = db(pool.clone()).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let (app, _) = policy_fixture(&admin, org, actor).await;
    let tables = ["flag_errors", "flag_eval_hourly", "platform_events"];
    let seed = || async {
        let mut tx = admin.begin(org).await.unwrap();
        telemetry::record_evaluations(&mut tx, app.id, KeyKind::Server, eval_events(1, true))
            .await
            .unwrap();
        telemetry::record_errors(
            &mut tx,
            app.id,
            KeyKind::Server,
            error_events(1, true, Some("production")),
        )
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO platform_events (event_id, org_id, kind) \
             VALUES ($1, $2, 'member.removed')",
        )
        .bind(uuid::Uuid::new_v4())
        .bind(org)
        .execute(tx.conn())
        .await
        .unwrap();
        tx.commit().await.unwrap();
    };
    seed().await;

    for table in tables {
        let mut tx = as_app_unpinned(&pool).await;
        let n = sqlx::query(&format!("DELETE FROM {table}"))
            .execute(&mut *tx)
            .await
            .unwrap()
            .rows_affected();
        assert_eq!(n, 0, "an unmarked unpinned DELETE emptied {table}");
        tx.rollback().await.unwrap();

        let mut tx = as_app_maintenance(&pool).await;
        let n = sqlx::query(&format!("DELETE FROM {table}"))
            .execute(&mut *tx)
            .await
            .unwrap()
            .rows_affected();
        assert_eq!(n, 1, "a marked DELETE on {table}");
        tx.rollback().await.unwrap();
    }

    // Pinned deletes (the org.deleted purge's loop) need no marker.
    let app_db = otto_app_db(&pool).await;
    let mut tx = app_db.begin(org).await.unwrap();
    for table in tables {
        let n = sqlx::query(&format!("DELETE FROM {table} WHERE org_id = $1"))
            .bind(org)
            .execute(tx.conn())
            .await
            .unwrap()
            .rows_affected();
        assert_eq!(n, 1, "a pinned DELETE on {table}");
    }
    tx.rollback().await.unwrap();
    app_db.pool().close().await;
}
