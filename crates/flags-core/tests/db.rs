//! flags-core against a real Postgres. Each test gets a fresh, migrated
//! database from `#[sqlx::test]`; set `DATABASE_URL` to a superuser on a
//! scratch cluster (`podman compose up -d` gives you one on port 15434).

use chrono::{DurationRound, TimeDelta, Utc};
use flags_core::apps::{AppsExt, Rotate};
use flags_core::audit_purge;
use flags_core::eval::{Context, Operator, Rule};
use flags_core::flags::{ChangeMeta, EnvPatch, FlagPatch, FlagsExt, NewFlag};
use flags_core::keys::KeyKind;
use flags_core::platform_events::{self, begin_live, Outcome};
use flags_core::rollback::{self, Mode, PolicyState, RollbackExt, Thresholds};
use flags_core::telemetry::{
    self, ErrorEvent, EvalEvent, ERROR_RETENTION_DAYS, EVAL_RETENTION_DAYS,
};
use flags_core::usage;
use flags_core::{eval, keys, Error};
use otto_tenant::ids::{OrgId, UserId};
use otto_tenant::Db;
use serde_json::json;
use sqlx::migrate::{MigrateError, Migration, MigrationType, Migrator};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgPool, Postgres};
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

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
    for name in ["rollback_policies", "rollback_events", "usage_meter_hourly"] {
        let table = report
            .tables
            .iter()
            .find(|t| t.name == name)
            .unwrap_or_else(|| panic!("{name} is missing from the isolation report"));
        assert!(table.rls_enabled, "{name}: RLS not enabled");
        assert!(table.rls_forced, "{name}: RLS not forced");
    }
}

/// The usage outbox is drained by the background shipper across every org, so
/// its baseline policy lets an unpinned transaction through. Unpinned alone is
/// not enough (#68, as #27 for the retention tables): a request path that
/// forgot to pin is unpinned too. Without the maintenance marker an unpinned
/// `otto_app` transaction sees, changes, and adds nothing; with it, it sees
/// every org's rows; pinned transactions still see only their own org.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn usage_outbox_tables_need_the_maintenance_marker_unpinned(pool: PgPool) {
    let (a, b) = (OrgId::new(), OrgId::new());
    for org in [a, b] {
        sqlx::query(
            "INSERT INTO usage_outbox (org_id, tool, billable) VALUES ($1, 'create_flag', true)",
        )
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO usage_outbox_rejected (event_id, org_id, tool, billable, occurred_at, reason) \
             VALUES (gen_random_uuid(), $1, 'create_flag', true, now(), 'x')",
        )
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    }

    for table in ["usage_outbox", "usage_outbox_rejected"] {
        // Unpinned, no marker: nothing visible, nothing changed.
        let mut tx = as_app_unpinned(&pool).await;
        let seen: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(seen, 0, "{table}: unpinned read without the marker");
        let updated = sqlx::query(&format!("UPDATE {table} SET tool = 'x'"))
            .execute(&mut *tx)
            .await
            .unwrap()
            .rows_affected();
        assert_eq!(updated, 0, "{table}: unpinned update without the marker");
        let deleted = sqlx::query(&format!("DELETE FROM {table}"))
            .execute(&mut *tx)
            .await
            .unwrap()
            .rows_affected();
        assert_eq!(deleted, 0, "{table}: unpinned delete without the marker");
        tx.rollback().await.unwrap();

        // Unpinned, no marker: an insert is refused.
        let mut tx = as_app_unpinned(&pool).await;
        let insert = if table == "usage_outbox" {
            "INSERT INTO usage_outbox (org_id, tool, billable) VALUES ($1, 'create_flag', true)"
        } else {
            "INSERT INTO usage_outbox_rejected (event_id, org_id, tool, billable, occurred_at, reason) \
             VALUES (gen_random_uuid(), $1, 'create_flag', true, now(), 'x')"
        };
        let err = sqlx::query(insert)
            .bind(a)
            .execute(&mut *tx)
            .await
            .unwrap_err();
        assert_eq!(
            sqlstate(&err),
            "42501",
            "{table}: unpinned insert without the marker"
        );
        drop(tx);

        // Unpinned with the marker: every org's rows.
        let mut tx = as_app_maintenance(&pool).await;
        let seen: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(seen, 2, "{table}: maintenance read");
        tx.rollback().await.unwrap();

        // Pinned: only the org's own rows, and its own insert succeeds.
        let db = db(pool.clone()).await;
        let mut tx = db.begin(a).await.unwrap();
        let seen: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(tx.conn())
            .await
            .unwrap();
        assert_eq!(seen, 1, "{table}: pinned read");
        sqlx::query(insert)
            .bind(a)
            .execute(tx.conn())
            .await
            .unwrap();
        tx.rollback().await.unwrap();
    }
}

/// The hourly evaluation meter is drained and rolled up across every org by a
/// background closer, so like the usage outbox it needs the maintenance marker
/// unpinned, while a pinned transaction reaches only its own org's rows.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn usage_meter_hourly_needs_the_maintenance_marker_unpinned(pool: PgPool) {
    let db = db(pool.clone()).await;
    let (a, b) = (OrgId::new(), OrgId::new());
    let mut apps = vec![];
    for org in [a, b] {
        let mut tx = db.begin(org).await.unwrap();
        let (app, _) = tx.create_app("web", &[], UserId::new()).await.unwrap();
        tx.commit().await.unwrap();
        sqlx::query(
            "INSERT INTO usage_meter_hourly (org_id, app_id, hour, served) \
             VALUES ($1, $2, date_trunc('hour', now()), 1)",
        )
        .bind(org)
        .bind(app.id)
        .execute(&pool)
        .await
        .unwrap();
        apps.push(app.id);
    }
    let insert = "INSERT INTO usage_meter_hourly (org_id, app_id, hour, served) \
                  VALUES ($1, $2, date_trunc('hour', now()) - interval '1 hour', 1)";

    // Unpinned, no marker: nothing visible, nothing changed, no insert.
    let mut tx = as_app_unpinned(&pool).await;
    let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_meter_hourly")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(seen, 0, "unpinned read without the marker");
    let updated = sqlx::query("UPDATE usage_meter_hourly SET served = 2")
        .execute(&mut *tx)
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(updated, 0, "unpinned update without the marker");
    let deleted = sqlx::query("DELETE FROM usage_meter_hourly")
        .execute(&mut *tx)
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(deleted, 0, "unpinned delete without the marker");
    tx.rollback().await.unwrap();

    let mut tx = as_app_unpinned(&pool).await;
    let err = sqlx::query(insert)
        .bind(a)
        .bind(apps[0])
        .execute(&mut *tx)
        .await
        .unwrap_err();
    assert_eq!(
        sqlstate(&err),
        "42501",
        "unpinned insert without the marker"
    );
    drop(tx);

    // Unpinned with the marker: every org's rows.
    let mut tx = as_app_maintenance(&pool).await;
    let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_meter_hourly")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(seen, 2, "maintenance read");
    tx.rollback().await.unwrap();

    // Pinned: only the org's own rows, and its own insert succeeds.
    let mut tx = db.begin(a).await.unwrap();
    let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_meter_hourly")
        .fetch_one(tx.conn())
        .await
        .unwrap();
    assert_eq!(seen, 1, "pinned read");
    sqlx::query(insert)
        .bind(a)
        .bind(apps[0])
        .execute(tx.conn())
        .await
        .unwrap();
    tx.rollback().await.unwrap();
}

/// The meter and quantity columns travel together and a metered event counts
/// at least one: both NULL (an unmetered row, as the previous release writes)
/// or both set and positive, on both outbox tables.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn usage_outbox_meter_and_quantity_are_paired_and_positive(pool: PgPool) {
    let org = OrgId::new();
    for table in ["usage_outbox", "usage_outbox_rejected"] {
        let insert = |meter: &str, quantity: &str| {
            if table == "usage_outbox" {
                format!(
                    "INSERT INTO usage_outbox (org_id, tool, billable, meter, quantity) \
                     VALUES ($1, 'evaluate', true, {meter}, {quantity})"
                )
            } else {
                format!(
                    "INSERT INTO usage_outbox_rejected \
                     (event_id, org_id, tool, billable, occurred_at, reason, meter, quantity) \
                     VALUES (gen_random_uuid(), $1, 'evaluate', true, now(), 'x', {meter}, {quantity})"
                )
            }
        };
        for (meter, quantity) in [("NULL", "NULL"), ("'evaluations'", "5")] {
            sqlx::query(&insert(meter, quantity))
                .bind(org)
                .execute(&pool)
                .await
                .unwrap_or_else(|e| panic!("{table}: ({meter}, {quantity}) refused: {e}"));
        }
        for (meter, quantity) in [
            ("'evaluations'", "NULL"),
            ("NULL", "5"),
            ("'evaluations'", "0"),
        ] {
            let err = sqlx::query(&insert(meter, quantity))
                .bind(org)
                .execute(&pool)
                .await
                .unwrap_err();
            assert_eq!(
                sqlstate(&err),
                "23514",
                "{table}: ({meter}, {quantity}) must violate the CHECK"
            );
        }
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

/// Runs a migrator the way a booting server does: on one session, which then
/// ends. sqlx returns a migration error without releasing its session-level
/// advisory lock; in production the process exits and the session with it.
/// On a pooled connection the lock would outlive the call and block the next
/// migrator that gets a different connection.
async fn boot_migrate(migrator: &Migrator, pool: &PgPool) -> Result<(), MigrateError> {
    let mut conn = pool.acquire().await.unwrap().detach();
    let result = migrator.run(&mut conn).await;
    conn.close().await.unwrap();
    result
}

/// What a migration that fails in production leaves behind, which the
/// recovery runbook in `docs/deploy/fly.md` ("When a migration fails")
/// relies on (#34). sqlx runs each migration and its `_sqlx_migrations` row
/// in one transaction, so a failure leaves neither: no `success = false`
/// row, no `Dirty`, and the previous release boots as if the newer one had
/// never deployed. The newer release retries from scratch on every boot.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_failed_migration_leaves_nothing_and_the_previous_release_boots(pool: PgPool) {
    let newest = flags_core::MIGRATOR
        .iter()
        .map(|m| m.version)
        .max()
        .expect("migrations are embedded");
    let failing = newest + 1;
    // Fails on data, like a constraint production rows violate: the table
    // and its row are created first, so a partial apply would be visible.
    let mut migrations: Vec<Migration> = flags_core::MIGRATOR.iter().cloned().collect();
    migrations.push(Migration::new(
        failing,
        Cow::Borrowed("fails on existing data"),
        MigrationType::Simple,
        Cow::Borrowed(
            "CREATE TABLE half_applied (n int); \
             INSERT INTO half_applied VALUES (1); \
             ALTER TABLE half_applied ADD CONSTRAINT negative CHECK (n < 0);",
        ),
        false,
    ));
    // The corrected change a recovery ships after recording the failed one.
    let next = failing + 1;
    migrations.push(Migration::new(
        next,
        Cow::Borrowed("the corrected change"),
        MigrationType::Simple,
        Cow::Borrowed("CREATE TABLE corrected (n int);"),
        false,
    ));
    // `Migrator::DEFAULT` is doc-hidden in sqlx; `Cargo.lock` pins sqlx, so
    // an upgrade that drops it fails to compile rather than misbehaving.
    let newer_release = Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing: true,
        ..Migrator::DEFAULT
    };

    let assert_fails = |err: MigrateError| match err {
        MigrateError::ExecuteMigration(e, v) => {
            assert_eq!(v, failing);
            assert_eq!(sqlstate(&e), "23514", "{e}");
            // The runbook tells the operator to look for this in the log.
            let shown = MigrateError::ExecuteMigration(e, v).to_string();
            assert!(
                shown.starts_with(&format!(
                    "while executing migration {failing}: error returned from database: "
                )),
                "{shown}"
            );
        }
        other => panic!("expected ExecuteMigration({failing}), got {other:?}"),
    };
    assert_fails(boot_migrate(&newer_release, &pool).await.unwrap_err());

    // Nothing recorded, nothing half applied.
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM _sqlx_migrations WHERE version >= $1 OR NOT success",
    )
    .bind(failing)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rows, 0, "a failed migration must leave no row");
    let table: Option<String> = sqlx::query_scalar("SELECT to_regclass('half_applied')::text")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        table, None,
        "a failed migration must leave none of its changes"
    );

    // The previous release boots.
    boot_migrate(&flags_core::MIGRATOR, &pool).await.unwrap();
    flags_core::migrate(&db(pool.clone()).await).await.unwrap();

    // The newer release retries from scratch and fails the same way, not
    // with `Dirty`.
    assert_fails(boot_migrate(&newer_release, &pool).await.unwrap_err());

    // The runbook's human-only "record as applied" statement, with the
    // checksum as `sha384sum` prints it: the newer release then boots,
    // skips the failed migration, and applies the corrected one. The
    // description differs from sqlx's on purpose: only version and checksum
    // are compared.
    let checksum_hex: String = newer_release
        .iter()
        .find(|m| m.version == failing)
        .unwrap()
        .checksum
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES ($1, 'recorded by hand', true, decode($2, 'hex'), -1)",
    )
    .bind(failing)
    .bind(&checksum_hex)
    .execute(&pool)
    .await
    .unwrap();
    boot_migrate(&newer_release, &pool).await.unwrap();
    let tables: (Option<String>, Option<String>) =
        sqlx::query_as("SELECT to_regclass('half_applied')::text, to_regclass('corrected')::text")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        tables,
        (None, Some("corrected".to_owned())),
        "the recorded migration is skipped and the corrected one applied"
    );
}

/// The runbook's "nothing is half applied" rests on sqlx committing each
/// migration with its row. sqlx runs a `-- no-transaction` migration outside
/// any transaction it opens and records its row separately, so those
/// guarantees would no longer hold. Adding one means revisiting
/// `docs/deploy/fly.md` first.
#[test]
fn no_migration_runs_outside_a_transaction() {
    for m in flags_core::MIGRATOR.iter() {
        assert!(
            !m.no_tx,
            "migration {} is -- no-transaction; update the failed-migration runbook",
            m.version
        );
    }
}

/// The runbook's "record as applied" statement takes the checksum from
/// `sha384sum <file>`. sqlx's checksum is SHA-384 of the file's bytes;
/// Postgres' `sha384` checks that for every embedded migration.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn migration_checksums_are_the_sha384_of_their_files(pool: PgPool) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    for m in flags_core::MIGRATOR.iter() {
        let prefix = format!("{:04}_", m.version);
        let matching: Vec<_> = files
            .iter()
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".sql"))
            })
            .collect();
        assert_eq!(
            matching.len(),
            1,
            "files for migration {}: {matching:?}",
            m.version
        );
        let path = matching[0];
        let bytes = std::fs::read(path).unwrap();
        let digest: Vec<u8> = sqlx::query_scalar("SELECT sha384($1)")
            .bind(&bytes)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(digest, m.checksum.as_ref(), "{}", path.display());
    }
}

/// A migration recorded as failed refuses to boot, even when this release
/// does not know its version. sqlx never writes such a row on Postgres (see
/// `a_failed_migration_leaves_nothing_and_the_previous_release_boots`); only
/// a hand-written row or another tool does. The runbook's human-only
/// `DELETE` clears it.
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

    let err = boot_migrate(&flags_core::MIGRATOR, &pool)
        .await
        .unwrap_err();
    assert!(
        matches!(err, MigrateError::Dirty(v) if v == newest + 1),
        "expected Dirty({}), got {err:?}",
        newest + 1
    );
    // The runbook tells the operator to look for this in the log.
    assert!(
        err.to_string()
            .starts_with(&format!("migration {} is partially applied", newest + 1)),
        "{err}"
    );

    // The runbook's human-only statement clears it.
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = $1 AND success = false")
        .bind(newest + 1)
        .execute(&pool)
        .await
        .unwrap();
    boot_migrate(&flags_core::MIGRATOR, &pool).await.unwrap();
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

// No stored key can contain NUL (Postgres text cannot hold it), so such a key
// finds nothing; binding it would fail the statement and abort the
// transaction instead (#45).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_key_with_nul_finds_no_flag(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let mut tx = db.begin(org).await.unwrap();
    let (app, _) = tx.create_app("web", &[], actor).await.unwrap();
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

    assert!(tx.find_flag(app.id, "f\0").await.unwrap().is_none());
    assert!(tx.find_flag(app.id, " \0").await.unwrap().is_none());
    let err = tx
        .update_flag(
            &app,
            "f\0",
            FlagPatch {
                name: Some("x".into()),
                ..Default::default()
            },
            meta(actor),
        )
        .await
        .unwrap_err();
    assert!(matches!(err, Error::FlagNotFound { .. }), "{err:?}");
    // The transaction is still usable: no statement failed in it.
    assert!(tx.find_flag(app.id, "f").await.unwrap().is_some());
    tx.commit().await.unwrap();
}

/// A write refused because of NUL, not for some other reason.
fn refused_for_nul(err: &Error) {
    assert!(
        matches!(err, Error::Invalid(m) if m.contains("NUL")),
        "{err:?}"
    );
}

// An MCP tool argument with NUL is unstorable: a lookup finds nothing and a
// write is refused as invalid input, and no statement fails in the transaction
// (#61).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn arguments_with_nul_find_nothing_or_are_refused(pool: PgPool) {
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

    for name in ["web\0", " \0"] {
        let err = tx.resolve_app(name).await.unwrap_err();
        assert!(matches!(err, Error::AppNotFound { .. }), "{err:?}");
    }

    let new_flag = |f: fn(&mut NewFlag)| {
        let mut n = NewFlag {
            key: "g".into(),
            ..Default::default()
        };
        f(&mut n);
        n
    };
    let bad: Vec<(NewFlag, Option<String>)> = vec![
        (new_flag(|n| n.name = Some("n\0".into())), None),
        (new_flag(|n| n.description = Some("d\0".into())), None),
        (new_flag(|_| {}), Some("r\0".into())),
        (
            new_flag(|n| n.variations = Some(json!({"on": {"description": "\u{0}"}}))),
            None,
        ),
        (
            new_flag(|n| n.configuration = Some(json!({"k": "\u{0}"}))),
            None,
        ),
    ];
    for (new, reason) in bad {
        let err = tx.create_flag(&app, new, actor, reason).await.unwrap_err();
        refused_for_nul(&err);
    }

    let err = tx
        .update_flag(
            &app,
            "f",
            FlagPatch {
                configuration: Some(json!(["\u{0}"])),
                ..Default::default()
            },
            meta(actor),
        )
        .await
        .unwrap_err();
    refused_for_nul(&err);
    let err = tx
        .update_flag(
            &app,
            "f",
            FlagPatch {
                name: Some("renamed".into()),
                ..Default::default()
            },
            ChangeMeta {
                reason: Some("r\0".into()),
                ..meta(actor)
            },
        )
        .await
        .unwrap_err();
    refused_for_nul(&err);
    // The rollback prefix ("rollback to vN: ") keeps the NUL. `h` has a
    // version to roll back to, so the reason is what refuses it.
    tx.create_flag(
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
    tx.update_flag(
        &app,
        "h",
        FlagPatch {
            name: Some("renamed".into()),
            ..Default::default()
        },
        meta(actor),
    )
    .await
    .unwrap();
    let err = tx
        .rollback_flag(
            &app,
            "h",
            None,
            ChangeMeta {
                reason: Some("r\0".into()),
                ..meta(actor)
            },
        )
        .await
        .unwrap_err();
    refused_for_nul(&err);

    // Restoring an active flag changes nothing, and still refuses the reason.
    let err = tx
        .set_archived(
            &app,
            "f",
            false,
            ChangeMeta {
                reason: Some("r\0".into()),
                ..meta(actor)
            },
        )
        .await
        .unwrap_err();
    refused_for_nul(&err);

    let err = tx
        .set_environment(
            &app,
            "f",
            "production",
            EnvPatch {
                rules: Some(vec![Rule {
                    attribute: "plan".into(),
                    operator: Operator::Equals,
                    values: vec![json!("a\u{0}")],
                    enabled: None,
                    variation: None,
                    description: None,
                }]),
                ..Default::default()
            },
            meta(actor),
        )
        .await
        .unwrap_err();
    refused_for_nul(&err);

    assert!(tx
        .find_policy(flag.id, "production\0")
        .await
        .unwrap()
        .is_none());
    assert!(!tx
        .remove_policy(&flag, "production\0", actor)
        .await
        .unwrap());

    let now = Utc::now();
    let event = |environment: &str| EvalEvent {
        flag_key: "f".into(),
        enabled: true,
        variation: None,
        environment: environment.into(),
        at: now,
    };
    let events = (0..2)
        .map(|_| event("production\0"))
        .chain((0..3).map(|_| event("production")))
        .collect();
    telemetry::record_evaluations(&mut tx, app.id, KeyKind::Server, events)
        .await
        .unwrap();
    let h = telemetry::health(&mut tx, &flag, Some("production\0"), 24)
        .await
        .unwrap();
    assert_eq!(h.evaluations.enabled, 2);
    assert!(!h.environment.unwrap().contains('\0'));
    let h = telemetry::health(&mut tx, &flag, Some("production"), 24)
        .await
        .unwrap();
    assert_eq!(h.evaluations.enabled, 3);

    // Nothing changed, and no statement failed in the transaction.
    assert_eq!(tx.find_flag(app.id, "f").await.unwrap().unwrap().version, 1);
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

    let h = telemetry::health(&mut tx, &flag, Some("production"), 24)
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

// Variation names are SDK-reported (a public client key can send any) and
// flag_health shows them to an agent, so they are cleaned like error text.
// Names that clean alike are counted together; one that cleans to nothing is
// dropped like an empty variation.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn health_shows_variation_names_cleaned(pool: PgPool) {
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
    let events: Vec<EvalEvent> = [
        ("a\u{202E}b", 2),
        ("a\u{200B}b", 3),
        ("\u{200B}", 1),
        ("on", 4),
    ]
    .into_iter()
    .flat_map(|(variation, n)| {
        (0..n).map(move |_| EvalEvent {
            flag_key: "f".into(),
            enabled: true,
            variation: Some(variation.into()),
            environment: "production".into(),
            at: now,
        })
    })
    .collect();
    telemetry::record_evaluations(&mut tx, app.id, KeyKind::Server, events)
        .await
        .unwrap();

    let h = telemetry::health(&mut tx, &flag, Some("production"), 24)
        .await
        .unwrap();
    assert_eq!(h.evaluations.enabled, 10);
    assert_eq!(
        h.variations,
        BTreeMap::from([("a b".to_string(), 5), ("on".to_string(), 4)])
    );
    // The name that cleans to nothing is not shown, but still counted.
    assert_eq!(h.other_variations, 1);
    tx.commit().await.unwrap();
}

// A public client key can report any number of distinct variation names. The
// answer shows the 20 most evaluated, as many as a flag can declare, and sums
// the rest, so no reporter sizes it (#59).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn health_shows_at_most_twenty_variations(pool: PgPool) {
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
    let event = |enabled: bool, variation: Option<String>| EvalEvent {
        flag_key: "f".into(),
        enabled,
        variation,
        environment: "production".into(),
        at: now,
    };
    // vNN is evaluated NN + 1 times.
    let mut events: Vec<EvalEvent> = (0..30)
        .flat_map(|i| (0..=i).map(move |_| format!("v{i:02}")))
        .map(|v| event(true, Some(v)))
        .collect();
    events.extend((0..2).map(|_| event(true, None)));
    events.extend((0..3).map(|_| event(false, None)));
    telemetry::record_evaluations(&mut tx, app.id, KeyKind::Client, events)
        .await
        .unwrap();

    let h = telemetry::health(&mut tx, &flag, Some("production"), 24)
        .await
        .unwrap();
    assert_eq!((h.evaluations.enabled, h.evaluations.disabled), (467, 3));
    let expected: BTreeMap<String, i64> = (10..30).map(|i| (format!("v{i:02}"), i + 1)).collect();
    assert_eq!(h.variations, expected);
    // v00..v09: 1 + 2 + ... + 10.
    assert_eq!(h.other_variations, 55);
    tx.commit().await.unwrap();
}

// The fetch behind the map is bounded too: past the 100 most evaluated names
// the rest are not read, even when they would merge into one large name, and
// they are still counted in other_variations (#59).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn health_reads_a_bounded_number_of_variation_names(pool: PgPool) {
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
    // 100 names evaluated twice fill the fetch. 30 spellings of "zz", once
    // each, are past it: read, they would merge into "zz" (30) and lead.
    let fetched = (0..100).flat_map(|i| [format!("a{i:03}"), format!("a{i:03}")]);
    let past = (1..=30).map(|k| format!("zz{}", "\u{200B}".repeat(k)));
    let events: Vec<EvalEvent> = fetched
        .chain(past)
        .map(|v| EvalEvent {
            flag_key: "f".into(),
            enabled: true,
            variation: Some(v),
            environment: "production".into(),
            at: now,
        })
        .collect();
    telemetry::record_evaluations(&mut tx, app.id, KeyKind::Client, events)
        .await
        .unwrap();

    let h = telemetry::health(&mut tx, &flag, Some("production"), 24)
        .await
        .unwrap();
    assert_eq!(h.evaluations.enabled, 230);
    let expected: BTreeMap<String, i64> = (0..20).map(|i| (format!("a{i:03}"), 2)).collect();
    assert_eq!(h.variations, expected);
    assert_eq!(h.other_variations, 230 - 40);
    tx.commit().await.unwrap();
}

// A flag's declared variations are always shown, even when a public client
// key reports more evaluations under made-up names, and even at zero (#70).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn health_always_shows_declared_variations(pool: PgPool) {
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
                variations: Some(json!({"control": {}, "treatment": {}, "unused": {}})),
                ..Default::default()
            },
            actor,
            None,
        )
        .await
        .unwrap();

    let now = Utc::now();
    let event = |variation: String| EvalEvent {
        flag_key: "f".into(),
        enabled: true,
        variation: Some(variation),
        environment: "production".into(),
        at: now,
    };
    // control 2, treatment 3; junk000..junk119 evaluated 5..=124 times, so
    // every junk name is louder than a declared one and the 120 of them
    // exceed the 100 raw names fetched.
    let mut events: Vec<EvalEvent> = (0..2).map(|_| event("control".into())).collect();
    events.extend((0..3).map(|_| event("treatment".into())));
    for i in 0..120 {
        events.extend((0..(5 + i)).map(|_| event(format!("junk{i:03}"))));
    }
    telemetry::record_evaluations(&mut tx, app.id, KeyKind::Client, events)
        .await
        .unwrap();

    let h = telemetry::health(&mut tx, &flag, Some("production"), 24)
        .await
        .unwrap();
    assert_eq!(h.variations.len(), 20);
    assert_eq!(h.variations.get("control"), Some(&2));
    assert_eq!(h.variations.get("treatment"), Some(&3));
    assert_eq!(h.variations.get("unused"), Some(&0));
    // The 17 most evaluated junk names (junk103..junk119) take the other slots.
    assert!(h.variations.contains_key("junk103"));
    assert!(!h.variations.contains_key("junk102"));
    // The rest, fetched or not: junk000..junk102.
    assert_eq!(h.other_variations, (0..=102).map(|i| 5 + i).sum::<i64>());
    tx.commit().await.unwrap();
}

// Error types are grouped as they read after cleaning, so one error reported
// under spellings that differ only in hidden characters is one row, and cannot
// spread over the ten and push real errors out (#59).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn health_merges_error_types_that_read_alike(pool: PgPool) {
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
    // Unmerged, no TypeError spelling (2 each) would reach the ten (3 each).
    let spellings = ["TypeError", "TypeError\u{200B}", "\u{202E}TypeError"]
        .into_iter()
        .map(|t| (t.to_string(), 2));
    let others = (0..11).map(|i| (format!("E{i:02}"), 3));
    let errors: Vec<ErrorEvent> = spellings
        .chain(others)
        .flat_map(|(error_type, n)| {
            (0..n).map(move |_| ErrorEvent {
                flag_key: "f".into(),
                flag_enabled: true,
                environment: Some("production".into()),
                error_type: error_type.clone(),
                error_message: "boom".into(),
                stack_trace: None,
                at: now,
            })
        })
        .collect();
    telemetry::record_errors(&mut tx, app.id, KeyKind::Client, errors)
        .await
        .unwrap();

    let h = telemetry::health(&mut tx, &flag, Some("production"), 24)
        .await
        .unwrap();
    assert_eq!(h.errors.enabled, 39);
    assert_eq!(h.top_errors.len(), 10);
    assert_eq!(
        (h.top_errors[0].error_type.as_str(), h.top_errors[0].count),
        ("TypeError", 6)
    );
    let rest: BTreeSet<&str> = h.top_errors[1..]
        .iter()
        .inspect(|g| assert_eq!(g.count, 3, "{g:?}"))
        .map(|g| g.error_type.as_str())
        .collect();
    assert_eq!(rest.len(), 9);
    assert!(rest.iter().all(|t| t.starts_with('E')), "{rest:?}");
    tx.commit().await.unwrap();
}

// Each error group shows the message of its latest report: the newest
// occurred_at, the row stored last on a tie, within the environment asked
// about. It is looked up per shown group, not aggregated from every message
// (#69).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn health_shows_the_latest_message_of_each_error_group(pool: PgPool) {
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

    let now = Utc::now().duration_trunc(TimeDelta::seconds(1)).unwrap();
    let ago = |mins: i64| now - TimeDelta::minutes(mins);
    // (environment, error_type, message, occurred_at), in insertion (id) order.
    let rows = [
        (Some("production"), "TypeError", "newest", ago(1)),
        (Some("production"), "TypeError", "oldest", ago(30)),
        (
            Some("production"),
            "TypeError",
            "tied, stored first",
            ago(1),
        ),
        (Some("production"), "TypeError", "tied, stored last", ago(1)),
        (Some("production"), "TypeError", "middle", ago(10)),
        // Newer, but in another environment.
        (Some("staging"), "TypeError", "other environment", ago(0)),
        (
            None,
            "RangeError",
            "no environment, newest",
            ago(2) + TimeDelta::microseconds(1),
        ),
        // A microsecond older but stored later: the time decides, not the id.
        (
            Some("production"),
            "RangeError",
            "a microsecond older",
            ago(2),
        ),
    ];
    for (environment, error_type, message, at) in rows {
        sqlx::query(
            "INSERT INTO flag_errors \
               (org_id, flag_id, environment, flag_enabled, error_type, error_message, occurred_at, reporter) \
             VALUES ($1, $2, $3, true, $4, $5, $6, 'server')",
        )
        .bind(org)
        .bind(flag.id)
        .bind(environment)
        .bind(error_type)
        .bind(message)
        .bind(at)
        .execute(tx.conn())
        .await
        .unwrap();
    }

    let h = telemetry::health(&mut tx, &flag, Some("production"), 24)
        .await
        .unwrap();
    let shown: Vec<(&str, i64, &str)> = h
        .top_errors
        .iter()
        .map(|g| (g.error_type.as_str(), g.count, g.last_message.as_str()))
        .collect();
    assert_eq!(
        shown,
        [
            ("TypeError", 5, "tied, stored last"),
            ("RangeError", 2, "no environment, newest"),
        ]
    );
    assert_eq!(h.top_errors[0].last_seen, ago(1));
    assert_eq!(
        h.top_errors[1].last_seen,
        ago(2) + TimeDelta::microseconds(1)
    );

    // Across every environment, the staging report is the newest.
    let h = telemetry::health(&mut tx, &flag, None, 24).await.unwrap();
    assert_eq!(h.top_errors[0].count, 6);
    assert_eq!(h.top_errors[0].last_message, "other environment");

    // A row stored before the reporter was recorded (NULL) is shown as a
    // client report, its own group beside the server one.
    sqlx::query(
        "INSERT INTO flag_errors \
           (org_id, flag_id, environment, flag_enabled, error_type, error_message, occurred_at) \
         VALUES ($1, $2, 'production', true, 'TypeError', 'legacy', $3)",
    )
    .bind(org)
    .bind(flag.id)
    .bind(ago(3))
    .execute(tx.conn())
    .await
    .unwrap();
    let h = telemetry::health(&mut tx, &flag, Some("production"), 24)
        .await
        .unwrap();
    let legacy: Vec<(KeyKind, i64, &str)> = h
        .top_errors
        .iter()
        .filter(|g| g.error_type == "TypeError")
        .map(|g| (g.reporter, g.count, g.last_message.as_str()))
        .collect();
    assert_eq!(
        legacy,
        [
            (KeyKind::Server, 5, "tied, stored last"),
            (KeyKind::Client, 1, "legacy"),
        ]
    );

    // A flag-off group of the same type is its own group with its own latest
    // message, and a row of another flag in the same org (higher id, later
    // time) is never chosen as a message.
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
    for (flag_id, enabled, message, at) in [
        (flag.id, false, "flag off", ago(5)),
        (other.id, true, "another flag", ago(0)),
    ] {
        sqlx::query(
            "INSERT INTO flag_errors \
               (org_id, flag_id, environment, flag_enabled, error_type, error_message, occurred_at, reporter) \
             VALUES ($1, $2, 'production', $3, 'TypeError', $4, $5, 'server')",
        )
        .bind(org)
        .bind(flag_id)
        .bind(enabled)
        .bind(message)
        .bind(at)
        .execute(tx.conn())
        .await
        .unwrap();
    }
    let h = telemetry::health(&mut tx, &flag, Some("production"), 24)
        .await
        .unwrap();
    let typed: Vec<(bool, KeyKind, i64, &str)> = h
        .top_errors
        .iter()
        .filter(|g| g.error_type == "TypeError")
        .map(|g| (g.flag_enabled, g.reporter, g.count, g.last_message.as_str()))
        .collect();
    assert_eq!(
        typed,
        [
            (true, KeyKind::Server, 5, "tied, stored last"),
            (true, KeyKind::Client, 1, "legacy"),
            (false, KeyKind::Server, 1, "flag off"),
        ]
    );
    tx.commit().await.unwrap();
}

// A public client key can report any number of error types and spellings.
// Server-key groups are fetched apart from client ones and keep slots in
// the answer, so client reports cannot hide what the server reported (#70).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn health_keeps_server_errors_among_client_floods(pool: PgPool) {
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
    let error = |error_type: String| ErrorEvent {
        flag_key: "f".into(),
        flag_enabled: true,
        environment: Some("production".into()),
        error_type,
        error_message: "boom".into(),
        stack_trace: None,
        at: now,
    };
    // 120 spellings of one junk type (3 each) fill a shared fetch of 100,
    // and 20 other client types (5 each) outrank every server type.
    let mut flood: Vec<ErrorEvent> = (1..=120)
        .flat_map(|k| {
            let t = format!("Junk{}", "\u{200B}".repeat(k));
            (0..3).map(move |_| t.clone())
        })
        .map(error)
        .collect();
    flood.extend(
        (0..20)
            .flat_map(|i| (0..5).map(move |_| format!("C{i:02}")))
            .map(error),
    );
    for batch in flood.chunks(telemetry::MAX_BATCH) {
        telemetry::record_errors(&mut tx, app.id, KeyKind::Client, batch.to_vec())
            .await
            .unwrap();
    }
    let server = ["S0", "S1", "S2"].map(|t| error(t.into())).to_vec();
    telemetry::record_errors(&mut tx, app.id, KeyKind::Server, server)
        .await
        .unwrap();

    let h = telemetry::health(&mut tx, &flag, Some("production"), 24)
        .await
        .unwrap();
    assert_eq!(h.top_errors.len(), 10);
    let server: Vec<&str> = h
        .top_errors
        .iter()
        .filter(|g| g.reporter == KeyKind::Server)
        .map(|g| g.error_type.as_str())
        .collect();
    assert_eq!(server, ["S0", "S1", "S2"]);
    assert!(h
        .top_errors
        .iter()
        .filter(|g| g.reporter == KeyKind::Client)
        .all(|g| g.count >= 5));
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

/// Postgres `text` cannot hold NUL, which JSON and Rust strings can: one in
/// any reported field used to fail the whole batch (#38). It is stored as
/// U+FFFD, and a flag key containing one is simply unknown.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn telemetry_text_with_nul_is_stored_replaced(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let mut tx = db.begin(org).await.unwrap();
    let (app, _) = tx.create_app("web", &[], actor).await.unwrap();
    let f = tx
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
        .unwrap()
        .id;

    let error = |key: &str, plain: bool| ErrorEvent {
        flag_key: key.into(),
        flag_enabled: true,
        environment: Some(if plain { "prod" } else { "prod\0x" }.into()),
        error_type: if plain { "T" } else { "Type\0Error" }.into(),
        error_message: if plain { "m" } else { "bad\0message" }.into(),
        stack_trace: Some(if plain { "s" } else { "at\0main" }.into()),
        at: Utc::now(),
    };
    let recorded = telemetry::record_errors(
        &mut tx,
        app.id,
        KeyKind::Server,
        vec![error("f", false), error("f\0", true)],
    )
    .await
    .unwrap();
    assert_eq!(
        recorded.receipt,
        telemetry::Receipt {
            accepted: 1,
            unknown_flags: 1
        }
    );
    assert_eq!(recorded.flags, vec![f]);
    let rows: Vec<(String, String, String, String)> = sqlx::query_as(
        "SELECT environment, error_type, error_message, stack_trace FROM flag_errors",
    )
    .fetch_all(tx.conn())
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![(
            "prod\u{FFFD}x".to_string(),
            "Type\u{FFFD}Error".to_string(),
            "bad\u{FFFD}message".to_string(),
            "at\u{FFFD}main".to_string()
        )]
    );

    // A batch naming only NUL keys is all unknown and writes nothing.
    let recorded =
        telemetry::record_errors(&mut tx, app.id, KeyKind::Server, vec![error("f\0", true)])
            .await
            .unwrap();
    assert_eq!(
        recorded.receipt,
        telemetry::Receipt {
            accepted: 0,
            unknown_flags: 1
        }
    );
    assert!(recorded.flags.is_empty());
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM flag_errors")
        .fetch_one(tx.conn())
        .await
        .unwrap();
    assert_eq!(count, 1);

    let now = Utc::now();
    let eval = |key: &str| EvalEvent {
        flag_key: key.into(),
        enabled: true,
        variation: Some("v\0one".into()),
        environment: "stag\0ing".into(),
        at: now,
    };
    // Replaced, not dropped: a plain "staging" / "vone" event keeps its own
    // bucket rather than merging with the malformed one.
    let plain = EvalEvent {
        environment: "staging".into(),
        variation: Some("vone".into()),
        ..eval("f")
    };
    let recorded = telemetry::record_evaluations(
        &mut tx,
        app.id,
        KeyKind::Server,
        vec![eval("f"), eval("f"), eval("f\0"), plain],
    )
    .await
    .unwrap();
    assert_eq!(
        recorded.receipt,
        telemetry::Receipt {
            accepted: 3,
            unknown_flags: 1
        }
    );
    let rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT environment, variation, count FROM flag_eval_hourly ORDER BY count DESC",
    )
    .fetch_all(tx.conn())
    .await
    .unwrap();
    assert_eq!(
        rows,
        vec![
            ("stag\u{FFFD}ing".to_string(), "v\u{FFFD}one".to_string(), 2),
            ("staging".to_string(), "vone".to_string(), 1),
        ]
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

    // Each org has a rejected usage event; the purge takes only the org's own.
    for o in [org, other] {
        sqlx::query(
            "INSERT INTO usage_outbox_rejected (event_id, org_id, user_id, tool, billable, occurred_at, reason) \
             VALUES (gen_random_uuid(), $1, gen_random_uuid(), 'create_flag', true, now(), 'x')",
        )
        .bind(o)
        .execute(db.pool())
        .await
        .unwrap();
    }

    // The org has an hourly evaluation meter row; the purge must count and take it.
    sqlx::query(
        "INSERT INTO usage_meter_hourly (org_id, app_id, hour, served, reported) \
         SELECT org_id, id, date_trunc('hour', now()), 3, 2 FROM flag_apps WHERE org_id = $1",
    )
    .bind(org)
    .execute(db.pool())
    .await
    .unwrap();

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
    assert_eq!(detail["usage_meter_hourly"], 1, "{detail}");
    assert_eq!(detail["rollback_policies"], 1, "{detail}");
    assert_eq!(detail["rollback_events"], 1, "{detail}");
    assert_eq!(detail["usage_outbox_rejected"], 1, "{detail}");
    assert_eq!(
        platform_events::apply(&db, &event).await.unwrap(),
        Outcome::Duplicate
    );
    for table in [
        "rollback_policies",
        "rollback_events",
        "usage_meter_hourly",
        "usage_outbox_rejected",
    ] {
        let left: i64 =
            sqlx::query_scalar(&format!("SELECT count(*) FROM {table} WHERE org_id = $1"))
                .bind(org)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert_eq!(left, 0, "{table}");
    }
    let others: i64 =
        sqlx::query_scalar("SELECT count(*) FROM usage_outbox_rejected WHERE org_id = $1")
            .bind(other)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(others, 1, "another org's rejected usage stays");

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

/// `org.deleted` purges through the function the startup check vouches for,
/// `public.purge_deleted_org_audit`, even when a same-named function sits
/// earlier on the tenant role's `search_path` (`"$user"`, here `otto_app`):
/// the call names it by schema (#53).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn org_deleted_purges_through_public_whatever_the_search_path(pool: PgPool) {
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

    for statement in [
        "CREATE SCHEMA otto_app",
        "CREATE FUNCTION otto_app.purge_deleted_org_audit(uuid) RETURNS bigint \
         LANGUAGE sql AS 'SELECT -1::bigint'",
        "GRANT USAGE ON SCHEMA otto_app TO otto_app",
    ] {
        sqlx::query(statement).execute(&pool).await.unwrap();
    }

    let app_db = otto_app_db(&pool).await;
    let decoy: i64 = sqlx::query_scalar("SELECT purge_deleted_org_audit($1)")
        .bind(org)
        .fetch_one(app_db.pool())
        .await
        .unwrap();
    assert_eq!(decoy, -1, "the unqualified name must resolve to the decoy");

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
/// nothing in silence, so the function refuses to run for one (#21). All in
/// one transaction that is rolled back: `CREATE ROLE` is transactional, so a
/// failing run leaves no cluster-wide role behind.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn audit_purge_refuses_an_owner_subject_to_rls(pool: PgPool) {
    let org = OrgId::new();
    let role = format!("purge_owner_{}", uuid::Uuid::new_v4().simple());
    let mut tx = pool.begin().await.unwrap();
    for statement in [
        format!("CREATE ROLE {role} NOLOGIN NOSUPERUSER NOBYPASSRLS"),
        format!("ALTER FUNCTION purge_deleted_org_audit(uuid) OWNER TO {role}"),
    ] {
        sqlx::query(&statement).execute(&mut *tx).await.unwrap();
    }
    sqlx::query("SELECT set_config('app.org_id', $1, true)")
        .bind(org.to_string())
        .execute(&mut *tx)
        .await
        .unwrap();
    let err = sqlx::query_scalar::<_, i64>("SELECT purge_deleted_org_audit($1)")
        .bind(org)
        .fetch_one(&mut *tx)
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err), "55000", "{err}");
    tx.rollback().await.unwrap();
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

/// The retention sweeps on the default test pool, whose superuser role RLS
/// does not apply to. Maintenance transactions run there as `otto_app` (#73;
/// `maintenance`'s own tests prove the role), so this guards that the marker
/// policies and grants still let them reach every org's expired rows, and only
/// those. It would pass before #73 too: it guards, it does not reproduce.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn sweeps_reach_every_org_on_the_superuser_pool(pool: PgPool) {
    let db = db(pool.clone()).await;
    let actor = UserId::new();
    let keep_days = 30;
    let orgs = [OrgId::new(), OrgId::new()];
    for org in orgs {
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
        tx.commit().await.unwrap();
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
        // Removed-member tombstones: no RLS, but otto_app needs the DELETE grant.
        for hours in [1, 0] {
            sqlx::query(
                "INSERT INTO removed_members (org_id, user_id, removed_at) \
                 VALUES ($1, $2, now() - make_interval(hours => $3))",
            )
            .bind(org)
            .bind(uuid::Uuid::new_v4())
            .bind(hours)
            .execute(&pool)
            .await
            .unwrap();
        }
    }

    assert_eq!(telemetry::sweep(&db).await.unwrap(), 6);
    assert_eq!(platform_events::sweep(&db, keep_days).await.unwrap(), 4);
    for table in [
        "flag_errors",
        "flag_eval_hourly",
        "rollback_events",
        "platform_events",
        "removed_members",
    ] {
        // Uuid orders bytewise, as Postgres orders uuid; OrgId has no Ord.
        let orgs_left: Vec<uuid::Uuid> =
            sqlx::query_scalar(&format!("SELECT org_id FROM {table} ORDER BY org_id"))
                .fetch_all(&pool)
                .await
                .unwrap();
        let mut expected: Vec<uuid::Uuid> = orgs.iter().map(|o| o.as_uuid()).collect();
        expected.sort();
        assert_eq!(orgs_left, expected, "{table}: each org keeps its fresh row");
    }
}

/// Rejected usage events are kept `usage::REJECTED_RETENTION_DAYS` days for
/// operators, then swept across every org (#74), in a maintenance transaction
/// (as `otto_app` even on this superuser pool; `maintenance`'s tests prove the
/// role). A row a day inside the window pins the cutoff and its unit.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn rejected_usage_is_swept_after_its_retention(pool: PgPool) {
    let db = db(pool.clone()).await;
    let orgs = [OrgId::new(), OrgId::new()];
    for org in orgs {
        for days in [
            usage::REJECTED_RETENTION_DAYS + 1,
            usage::REJECTED_RETENTION_DAYS - 1,
            1,
        ] {
            sqlx::query(
                "INSERT INTO usage_outbox_rejected \
                 (event_id, org_id, user_id, tool, billable, occurred_at, reason, rejected_at) \
                 VALUES (gen_random_uuid(), $1, gen_random_uuid(), 'create_flag', true, now(), 'x', \
                         now() - make_interval(days => $2))",
            )
            .bind(org)
            .bind(days)
            .execute(&pool)
            .await
            .unwrap();
        }
    }

    assert_eq!(usage::sweep_rejected(&db).await.unwrap(), 2);
    for org in orgs {
        let (total, expired): (i64, i64) = sqlx::query_as(
            "SELECT count(*), count(*) FILTER (WHERE rejected_at < now() - make_interval(days => $2)) \
             FROM usage_outbox_rejected WHERE org_id = $1",
        )
        .bind(org)
        .bind(usage::REJECTED_RETENTION_DAYS)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            (total, expired),
            (2, 0),
            "each org keeps its rows inside the window"
        );
    }
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_removed_member_is_refused_but_others_are_not(pool: PgPool) {
    let db = db(pool.clone()).await;
    let org = OrgId::new();
    let (gone, stays) = (UserId::new(), UserId::new());
    let body = otto_resource::webhook::body(
        uuid::Uuid::new_v4(),
        "member.removed",
        Utc::now(),
        &json!({"org_id": org.as_uuid(), "user_id": gone.as_uuid()}),
    );
    let event = otto_resource::webhook::parse(&body).unwrap();
    let audited = || async {
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM audit_events \
             WHERE org_id = $1 AND action = 'platform.member.removed'",
        )
        .bind(org)
        .fetch_one(&pool)
        .await
        .unwrap()
    };
    assert_eq!(
        platform_events::apply(&db, &event).await.unwrap(),
        Outcome::Applied { detail: json!({}) }
    );
    assert_eq!(audited().await, 1);
    // A redelivery is recognised and audits nothing more.
    assert_eq!(
        platform_events::apply(&db, &event).await.unwrap(),
        Outcome::Duplicate
    );
    assert_eq!(audited().await, 1);
    assert!(platform_events::revoked(&db, org, gone).await.unwrap());
    assert!(!platform_events::revoked(&db, org, stays).await.unwrap());
    assert!(begin_live(&db, org, Some(stays)).await.is_ok());
}

/// A `member.removed` delivered after its org's `org.deleted` must not write
/// an audit row into the purged org: nothing would remove it short of a
/// redelivered `org.deleted` (#41). The dedupe marker is still recorded, and
/// so is the short-lived `removed_members` row, as for a live org.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn member_removed_after_org_deleted_writes_no_audit_row(pool: PgPool) {
    let db = db(pool.clone()).await;
    let org = OrgId::new();
    let mut tx = db.begin(org).await.unwrap();
    tx.create_app("web", &[], UserId::new()).await.unwrap();
    tx.commit().await.unwrap();
    assert!(audit_rows(&pool, org).await > 0);

    let app_db = otto_app_db(&pool).await;
    let body = otto_resource::webhook::body(
        uuid::Uuid::new_v4(),
        "org.deleted",
        Utc::now(),
        &json!({"org_id": org.as_uuid()}),
    );
    let deleted = otto_resource::webhook::parse(&body).unwrap();
    assert!(matches!(
        platform_events::apply(&app_db, &deleted).await.unwrap(),
        Outcome::Applied { .. }
    ));
    assert_eq!(audit_rows(&pool, org).await, 0);

    let user = UserId::new();
    let body = otto_resource::webhook::body(
        uuid::Uuid::new_v4(),
        "member.removed",
        Utc::now(),
        &json!({"org_id": org.as_uuid(), "user_id": user.as_uuid()}),
    );
    let event = otto_resource::webhook::parse(&body).unwrap();
    assert_eq!(
        platform_events::apply(&app_db, &event).await.unwrap(),
        Outcome::Ignored
    );
    assert_eq!(audit_rows(&pool, org).await, 0);

    // Superuser pool: `app_db`'s RLS would hide the marker.
    let markers: i64 =
        sqlx::query_scalar("SELECT count(*) FROM platform_events WHERE event_id = $1")
            .bind(event.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(markers, 1);
    let removed: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM removed_members WHERE org_id = $1 AND user_id = $2",
    )
    .bind(org)
    .bind(user)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(removed, 1);

    assert_eq!(
        platform_events::apply(&app_db, &event).await.unwrap(),
        Outcome::Duplicate
    );
    assert_eq!(audit_rows(&pool, org).await, 0);
    app_db.pool().close().await;
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

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn audit_purge_check_passes_on_a_migrated_database(pool: PgPool) {
    let report = audit_purge::check(&Db::from_pool(pool)).await.unwrap();
    assert_eq!(report.problems(), Vec::<String>::new());
    assert_eq!(report.tenant_role, "otto_app");
    assert!(report.owner.is_some());
    assert!(report.owner_is_superuser || report.owner_bypasses_rls);
    assert!(report.executable);
}

/// Runs `inspect` as `otto_app` after making a throwaway role the function's
/// owner. Everything is transactional and rolled back, so a failed run leaves
/// no cluster-wide role behind. Needs the superuser `DATABASE_URL` the suite
/// already assumes: on Postgres 15+ only a superuser may hand a function in
/// `public` to a role without CREATE there.
async fn inspect_with_owner(pool: &PgPool, attributes: &str) -> (String, audit_purge::PurgeReport) {
    let role = format!("purge_owner_{}", uuid::Uuid::new_v4().simple());
    let mut tx = pool.begin().await.unwrap();
    for statement in [
        format!("CREATE ROLE {role} NOLOGIN {attributes}"),
        format!("ALTER FUNCTION purge_deleted_org_audit(uuid) OWNER TO {role}"),
        "SET LOCAL ROLE otto_app".to_string(),
    ] {
        sqlx::query(&statement).execute(&mut *tx).await.unwrap();
    }
    let report = audit_purge::inspect(&mut tx).await.unwrap();
    tx.rollback().await.unwrap();
    (role, report)
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn audit_purge_check_reports_an_owner_subject_to_rls(pool: PgPool) {
    let (role, report) = inspect_with_owner(&pool, "NOSUPERUSER NOBYPASSRLS").await;
    assert_eq!(report.owner.as_deref(), Some(role.as_str()));
    assert!(!report.owner_is_superuser && !report.owner_bypasses_rls);
    let problems = report.problems();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains(&role), "{problems:?}");
}

/// `otto_app`'s grant survives the owner change, so this exercises only the
/// exemption branch.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn audit_purge_check_accepts_a_bypassrls_owner(pool: PgPool) {
    let (_, report) = inspect_with_owner(&pool, "NOSUPERUSER BYPASSRLS").await;
    assert!(!report.owner_is_superuser && report.owner_bypasses_rls);
    assert_eq!(report.problems(), Vec::<String>::new());
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn audit_purge_check_reports_a_tenant_role_without_execute(pool: PgPool) {
    let mut tx = pool.begin().await.unwrap();
    for statement in [
        "REVOKE EXECUTE ON FUNCTION purge_deleted_org_audit(uuid) FROM otto_app",
        "SET LOCAL ROLE otto_app",
    ] {
        sqlx::query(statement).execute(&mut *tx).await.unwrap();
    }
    let report = audit_purge::inspect(&mut tx).await.unwrap();
    tx.rollback().await.unwrap();
    assert!(!report.executable);
    let problems = report.problems();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("GRANT EXECUTE"), "{problems:?}");
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn audit_purge_check_reports_a_missing_function(pool: PgPool) {
    let mut tx = pool.begin().await.unwrap();
    for statement in [
        "DROP FUNCTION purge_deleted_org_audit(uuid)",
        "SET LOCAL ROLE otto_app",
    ] {
        sqlx::query(statement).execute(&mut *tx).await.unwrap();
    }
    let report = audit_purge::inspect(&mut tx).await.unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(report.owner, None);
    let problems = report.problems();
    assert_eq!(problems.len(), 1, "{problems:?}");
    assert!(problems[0].contains("does not exist"), "{problems:?}");
}

/// The missing-function advice works (#98): 0004 creates the function
/// unqualified, so it can land outside `public` while 0004 is recorded as
/// applied and migrations will not re-run it. The check then reports it
/// missing and suggests moving it back, and after that move it passes.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn audit_purge_check_reports_a_function_outside_public(pool: PgPool) {
    let mut tx = pool.begin().await.unwrap();
    for statement in [
        "CREATE SCHEMA stray",
        "ALTER FUNCTION purge_deleted_org_audit(uuid) SET SCHEMA stray",
        "SET LOCAL ROLE otto_app",
    ] {
        sqlx::query(statement).execute(&mut *tx).await.unwrap();
    }
    let report = audit_purge::inspect(&mut tx).await.unwrap();
    assert_eq!(report.owner, None);
    let problems = report.problems();
    assert_eq!(problems.len(), 1, "{problems:?}");
    let advice = "ALTER FUNCTION <schema>.purge_deleted_org_audit(uuid) SET SCHEMA public";
    assert!(problems[0].contains(advice), "{problems:?}");

    for statement in [
        "RESET ROLE".to_string(),
        advice.replace("<schema>", "stray"),
        "SET LOCAL ROLE otto_app".to_string(),
    ] {
        sqlx::query(&statement).execute(&mut *tx).await.unwrap();
    }
    let report = audit_purge::inspect(&mut tx).await.unwrap();
    tx.rollback().await.unwrap();
    assert_eq!(report.problems(), Vec::<String>::new());
}

/// Plant a schema named after the tenant role, owned by it, holding empty
/// copies of `tables` (defaults and unique constraints included). Under the
/// default `search_path` (`"$user", public`) a transaction running as
/// `otto_app` would resolve those names here instead of in `public` (#97).
async fn shadow_schema(pool: &PgPool, tables: &[&str]) {
    sqlx::query("CREATE SCHEMA otto_app AUTHORIZATION otto_app")
        .execute(pool)
        .await
        .unwrap();
    for t in tables {
        for sql in [
            format!("CREATE TABLE otto_app.{t} (LIKE public.{t} INCLUDING ALL)"),
            format!("ALTER TABLE otto_app.{t} OWNER TO otto_app"),
        ] {
            sqlx::query(&sql).execute(pool).await.unwrap();
        }
    }
}

/// An app in `org`, committed, and its client key.
async fn app_with_key(db: &Db, org: OrgId) -> String {
    let mut tx = db.begin(org).await.unwrap();
    let (_, issued) = tx
        .create_app("web", &["production".into()], UserId::new())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    issued.client_key.unwrap()
}

/// The revocation check reads `public`, not a shadowing `otto_app` schema
/// that says the org is deleted.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn revocation_checks_ignore_a_shadowing_schema(pool: PgPool) {
    let db = db(pool.clone()).await;
    let org = OrgId::new();
    shadow_schema(&pool, &["deleted_orgs"]).await;
    sqlx::query("INSERT INTO otto_app.deleted_orgs (org_id) VALUES ($1)")
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    assert!(!platform_events::revoked(&db, org, UserId::new())
        .await
        .unwrap());
}

/// Key resolution reads `public`, not a shadowing `otto_app` schema that
/// holds no keys.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn key_resolution_ignores_a_shadowing_schema(pool: PgPool) {
    let db = db(pool.clone()).await;
    let org = OrgId::new();
    let key = app_with_key(&db, org).await;
    shadow_schema(&pool, &["app_keys", "deleted_orgs"]).await;
    let owner = keys::resolve(&db, &key)
        .await
        .unwrap()
        .expect("resolves in public");
    assert_eq!(owner.org_id, org);
}

/// A request transaction re-checks the tombstones in `public`, not in a
/// shadowing schema that says the org is deleted.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn request_tombstone_checks_ignore_a_shadowing_schema(pool: PgPool) {
    let db = db(pool.clone()).await;
    let org = OrgId::new();
    shadow_schema(&pool, &["deleted_orgs"]).await;
    sqlx::query("INSERT INTO otto_app.deleted_orgs (org_id) VALUES ($1)")
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    begin_live(&db, org, None)
        .await
        .expect("the only tombstone is in the shadow schema");
}

/// A request transaction reads tenant tables in `public`, not a shadowing
/// schema's copy holding another app.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn request_reads_ignore_a_shadowing_schema(pool: PgPool) {
    let db = db(pool.clone()).await;
    let org = OrgId::new();
    app_with_key(&db, org).await;
    shadow_schema(&pool, &["flag_apps"]).await;
    sqlx::query(
        "INSERT INTO otto_app.flag_apps (org_id, name, client_key) VALUES ($1, 'shadow', 'sdk_x')",
    )
    .bind(org)
    .execute(&pool)
    .await
    .unwrap();
    let mut tx = begin_live(&db, org, None).await.unwrap();
    let names: Vec<String> = tx
        .list_apps()
        .await
        .unwrap()
        .into_iter()
        .map(|a| a.name)
        .collect();
    assert_eq!(names, ["web"]);
}

/// A temporary table, searched before `public` for relations under the
/// default path, no longer shadows one: the pinned path lists `pg_temp` last.
/// One connection, so the temporary table is visible to the transactions.
/// `verify_tenant_isolation` first caches whether the tenant role can be
/// assumed: `Db::begin` asks on a second pool connection otherwise, which a
/// one-connection pool cannot give it while the transaction holds the first.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn transactions_ignore_a_shadowing_temporary_table(pool: PgPool) {
    let one = PgPoolOptions::new()
        .max_connections(1)
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let db = Db::from_pool(one);
    db.verify_tenant_isolation().await.unwrap();
    let org = OrgId::new();
    for sql in [
        "CREATE TEMP TABLE deleted_orgs (LIKE public.deleted_orgs INCLUDING ALL)",
        "GRANT ALL ON pg_temp.deleted_orgs TO otto_app",
    ] {
        sqlx::query(sql).execute(db.pool()).await.unwrap();
    }
    sqlx::query("INSERT INTO pg_temp.deleted_orgs (org_id) VALUES ($1)")
        .bind(org)
        .execute(db.pool())
        .await
        .unwrap();
    assert!(!platform_events::revoked(&db, org, UserId::new())
        .await
        .unwrap());
    begin_live(&db, org, None)
        .await
        .expect("the only tombstone is in the temporary table");
    db.pool().close().await;
}

/// A maintenance sweep deletes from `public`, not from a shadowing schema.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn maintenance_transactions_ignore_a_shadowing_schema(pool: PgPool) {
    let db = db(pool.clone()).await;
    shadow_schema(&pool, &["platform_events"]).await;
    for schema in ["public", "otto_app"] {
        sqlx::query(&format!(
            "INSERT INTO {schema}.platform_events (event_id, org_id, kind, received_at) \
             VALUES (gen_random_uuid(), gen_random_uuid(), 'team.deleted', now() - interval '30 days')"
        ))
        .execute(&pool)
        .await
        .unwrap();
    }
    platform_events::sweep(&db, 7).await.unwrap();
    for (schema, left) in [("public", 0), ("otto_app", 1)] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {schema}.platform_events"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, left, "{schema}");
    }
}

/// The rows a webhook writes in `public`, by schema: `(schema, table, rows)`.
async fn assert_counts(pool: &PgPool, expected: &[(&str, &str, i64)]) {
    for (schema, table, n) in expected {
        let got: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {schema}.{table}"))
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(got, *n, "{schema}.{table}");
    }
}

/// A delivery of `kind` for `org` with `data`.
fn delivery(kind: &str, data: serde_json::Value) -> otto_resource::webhook::WebhookEvent {
    let body = otto_resource::webhook::body(uuid::Uuid::new_v4(), kind, Utc::now(), &data);
    otto_resource::webhook::parse(&body).unwrap()
}

/// `team.deleted`'s dedupe marker lands in `public`, so a redelivery is
/// recognised there and the shadow schema stays empty.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn team_deleted_ignores_a_shadowing_schema(pool: PgPool) {
    let db = db(pool.clone()).await;
    shadow_schema(&pool, &["platform_events"]).await;
    let org = OrgId::new();
    let event = delivery(
        "team.deleted",
        json!({"org_id": org.as_uuid(), "team_id": uuid::Uuid::new_v4()}),
    );
    assert_eq!(
        platform_events::apply(&db, &event).await.unwrap(),
        Outcome::Ignored
    );
    assert_eq!(
        platform_events::apply(&db, &event).await.unwrap(),
        Outcome::Duplicate
    );
    assert_counts(
        &pool,
        &[
            ("public", "platform_events", 1),
            ("otto_app", "platform_events", 0),
        ],
    )
    .await;
}

/// `org.deleted`'s tombstone and dedupe marker land in `public`.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn org_deleted_ignores_a_shadowing_schema(pool: PgPool) {
    let db = db(pool.clone()).await;
    shadow_schema(&pool, &["deleted_orgs", "platform_events"]).await;
    let org = OrgId::new();
    let event = delivery("org.deleted", json!({"org_id": org.as_uuid()}));
    assert!(matches!(
        platform_events::apply(&db, &event).await.unwrap(),
        Outcome::Applied { .. }
    ));
    assert_counts(
        &pool,
        &[
            ("public", "deleted_orgs", 1),
            ("public", "platform_events", 1),
            ("otto_app", "deleted_orgs", 0),
            ("otto_app", "platform_events", 0),
        ],
    )
    .await;
}

/// `member.removed`'s tombstone and dedupe marker land in `public`.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn member_removed_ignores_a_shadowing_schema(pool: PgPool) {
    let db = db(pool.clone()).await;
    shadow_schema(&pool, &["removed_members", "platform_events"]).await;
    let (org, user) = (OrgId::new(), UserId::new());
    let event = delivery(
        "member.removed",
        json!({"org_id": org.as_uuid(), "user_id": user.as_uuid()}),
    );
    assert!(matches!(
        platform_events::apply(&db, &event).await.unwrap(),
        Outcome::Applied { .. }
    ));
    assert_counts(
        &pool,
        &[
            ("public", "removed_members", 1),
            ("public", "platform_events", 1),
            ("otto_app", "removed_members", 0),
            ("otto_app", "platform_events", 0),
        ],
    )
    .await;
}

/// Rotating a client key stops it on the very next lookup through the
/// unknown-key cache: a key that resolved is never remembered (#104).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn rotation_stops_a_key_through_the_cache(pool: PgPool) {
    let db = db(pool).await;
    let (org, actor) = (OrgId::new(), UserId::new());
    let unknown = Arc::new(keys::UnknownKeys::new());
    let limit = keys::KeyLookups::new(1, keys::KEY_LOOKUP_WAIT);
    let mut tx = db.begin(org).await.unwrap();
    let (app, issued) = tx
        .create_app("web", &["production".into()], actor)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let old = issued.client_key.unwrap();
    assert!(keys::resolve_cached(&db, &unknown, &limit, &old)
        .await
        .unwrap()
        .is_some());

    let mut tx = db.begin(org).await.unwrap();
    let (_, rotated) = tx.rotate_keys(&app, Rotate::Client, actor).await.unwrap();
    tx.commit().await.unwrap();
    let new = rotated.client_key.unwrap();

    assert!(keys::resolve_cached(&db, &unknown, &limit, &old)
        .await
        .unwrap()
        .is_none());
    assert!(keys::resolve_cached(&db, &unknown, &limit, &new)
        .await
        .unwrap()
        .is_some());
    assert_eq!(unknown.len(), 1, "only the rotated-away key is remembered");
}

/// A deleted org's key stops on the very next lookup through the cache.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_deleted_orgs_key_stops_through_the_cache(pool: PgPool) {
    let db = db(pool.clone()).await;
    let org = OrgId::new();
    let unknown = Arc::new(keys::UnknownKeys::new());
    let limit = keys::KeyLookups::new(1, keys::KEY_LOOKUP_WAIT);
    let mut tx = db.begin(org).await.unwrap();
    let (_, issued) = tx
        .create_app("web", &["production".into()], UserId::new())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let key = issued.client_key.unwrap();
    assert!(keys::resolve_cached(&db, &unknown, &limit, &key)
        .await
        .unwrap()
        .is_some());
    sqlx::query("INSERT INTO deleted_orgs (org_id) VALUES ($1)")
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    assert!(keys::resolve_cached(&db, &unknown, &limit, &key)
        .await
        .unwrap()
        .is_none());
}

/// A string that is not shaped like a key is not remembered (`looks_like_key`
/// refuses it before any lookup).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn malformed_keys_are_not_remembered(pool: PgPool) {
    let db = db(pool).await;
    let unknown = Arc::new(keys::UnknownKeys::new());
    let limit = keys::KeyLookups::new(1, keys::KEY_LOOKUP_WAIT);
    assert!(keys::resolve_cached(&db, &unknown, &limit, "sdk_xyz")
        .await
        .unwrap()
        .is_none());
    assert!(unknown.is_empty());
}

/// A database error is not an answer about the key, so it is never
/// remembered: once the database is back, the key resolves.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn errors_are_not_remembered(pool: PgPool) {
    let db = db(pool).await;
    let unknown = Arc::new(keys::UnknownKeys::new());
    let limit = keys::KeyLookups::new(1, keys::KEY_LOOKUP_WAIT);
    let key = keys::NewKey::mint(KeyKind::Client).key;
    db.pool().close().await;
    assert!(keys::resolve_cached(&db, &unknown, &limit, &key)
        .await
        .is_err());
    assert!(unknown.is_empty());
}

/// A connection outside `pool` holding `app_keys` in ACCESS EXCLUSIVE mode, so
/// every key lookup blocks in the database, holding its pool connection, until
/// the transaction is rolled back (#120). Close it before the test returns so
/// sqlx can drop the database.
async fn lock_app_keys(pool: &PgPool) -> sqlx::PgConnection {
    let mut lock = sqlx::PgConnection::connect_with(&pool.connect_options())
        .await
        .unwrap();
    sqlx::raw_sql("BEGIN; LOCK TABLE public.app_keys IN ACCESS EXCLUSIVE MODE")
        .execute(&mut lock)
        .await
        .unwrap();
    lock
}

/// Release [`lock_app_keys`]' lock and close its connection.
async fn unlock_app_keys(mut lock: sqlx::PgConnection) {
    sqlx::raw_sql("ROLLBACK").execute(&mut lock).await.unwrap();
    lock.close().await.unwrap();
}

/// Poll `check` every 10 ms until it holds; panics with `what` after 5 s.
async fn within_5s(what: &str, mut check: impl FnMut() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while !check() {
        assert!(std::time::Instant::now() < deadline, "timed out: {what}");
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
}

/// With every permit held by a lookup blocked in the database, the next
/// lookup waits out the limit's wait and is refused `Busy`; a remembered
/// unknown key still answers without a permit; and the blocked lookup's
/// permit comes back once it finishes (#120).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_lookup_waits_for_a_permit_and_is_refused_after_the_wait(pool: PgPool) {
    let db = db(pool.clone()).await;
    let unknown = Arc::new(keys::UnknownKeys::new());
    let wait = std::time::Duration::from_millis(200);
    let limit = Arc::new(keys::KeyLookups::new(1, wait));
    let remembered = keys::NewKey::mint(KeyKind::Client).key;
    assert!(keys::resolve_cached(&db, &unknown, &limit, &remembered)
        .await
        .unwrap()
        .is_none());

    let lock = lock_app_keys(&pool).await;
    let blocked = tokio::spawn({
        let (db, unknown, limit) = (db.clone(), unknown.clone(), limit.clone());
        let key = keys::NewKey::mint(KeyKind::Client).key;
        async move { keys::resolve_cached(&db, &unknown, &limit, &key).await }
    });
    within_5s("the blocked lookup takes the permit", || {
        limit.available() == 0
    })
    .await;

    let started = std::time::Instant::now();
    let refused = keys::resolve_cached(
        &db,
        &unknown,
        &limit,
        &keys::NewKey::mint(KeyKind::Client).key,
    )
    .await;
    assert!(matches!(refused, Err(Error::Busy)), "{refused:?}");
    assert!(started.elapsed() >= wait, "refused before the wait was up");
    assert!(keys::resolve_cached(&db, &unknown, &limit, &remembered)
        .await
        .unwrap()
        .is_none());
    assert_eq!(limit.available(), 0, "the remembered key took no permit");

    unlock_app_keys(lock).await;
    assert!(blocked.await.unwrap().unwrap().is_none());
    within_5s("the permit comes back", || limit.available() == 1).await;
    assert_eq!(unknown.len(), 2, "the remembered key and the blocked one");
}

/// A lookup answers only once its connection is back in the pool, release
/// ping included, so the permit it ran under covered the whole checkout
/// (#120). Checked with no wait in between: if the release were left to the
/// task sqlx spawns when a `PoolConnection` drops, a connection would still
/// be out here.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_lookup_returns_its_connection_before_it_answers(pool: PgPool) {
    let db = db(pool.clone()).await;
    let unknown = Arc::new(keys::UnknownKeys::new());
    let limit = keys::KeyLookups::new(1, keys::KEY_LOOKUP_WAIT);
    let all_idle = |db: &otto_tenant::Db| db.pool().num_idle() == db.pool().size() as usize;
    for _ in 0..3 {
        let key = keys::NewKey::mint(KeyKind::Client).key;
        assert!(keys::resolve_hash(&db, &keys::hash(&key))
            .await
            .unwrap()
            .is_none());
        assert!(
            all_idle(&db),
            "resolve_hash answered with its connection out"
        );
        assert!(
            keys::resolve_cached(&db, &unknown, &limit, &key.replace("sdk_", "srv_"))
                .await
                .unwrap()
                .is_none()
        );
        assert!(
            all_idle(&db),
            "resolve_cached answered with its connection out"
        );
    }
}

/// A lookup whose caller is dropped mid-lookup still finishes: its answer is
/// remembered and only then is its permit freed (#120).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_dropped_lookup_still_remembers_its_unknown_key(pool: PgPool) {
    let db = db(pool.clone()).await;
    let unknown = Arc::new(keys::UnknownKeys::new());
    let limit = Arc::new(keys::KeyLookups::new(1, keys::KEY_LOOKUP_WAIT));
    let lock = lock_app_keys(&pool).await;
    let caller = tokio::spawn({
        let (db, unknown, limit) = (db.clone(), unknown.clone(), limit.clone());
        let key = keys::NewKey::mint(KeyKind::Client).key;
        async move { keys::resolve_cached(&db, &unknown, &limit, &key).await }
    });
    within_5s("the lookup takes the permit", || limit.available() == 0).await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    assert_eq!(limit.available(), 0, "still held: the lookup is blocked");

    unlock_app_keys(lock).await;
    within_5s("the permit comes back", || limit.available() == 1).await;
    assert_eq!(
        unknown.len(),
        1,
        "the dropped lookup's answer is remembered"
    );
}

/// Every query on the pool's own role, outside a pinned (`begin_live`),
/// maintenance (`maintenance::begin`), or unpinned (`unpinned::begin`)
/// transaction, runs as whatever role the pool connects as. In production that
/// is the superuser attach role, which row-level security does not apply to,
/// so a raw pool query that named a tenant table would read every org's rows
/// (#80). Only the sites below may reach the pool directly; each says why. A
/// new one fails here: route it through `unpinned::begin` (flags-core), or add
/// it with its reason if it touches no table a tenant role is constrained on.
///
/// Textual, over each workspace crate's non-test `src`: comment lines and the
/// trailing `#[cfg(test)] mod tests` are skipped (it must be the file's last
/// item). It counts the ways to an unconstrained connection: `otto_tenant::Db`'s
/// `.pool()` and `begin_unpinned`, its audit helpers that run on the pool
/// (`audit_global`, `audit_for_org`, `audit_trail_for_user`), any `PgPool`
/// (`PgPoolOptions` included) held or built outside a `Db`, and a connection
/// opened beside the pool. It cannot see an alias or a reformatted call such as
/// `Db::pool(&db)`.
#[test]
fn no_raw_pool_queries_outside_the_allowed_sites() {
    // `Connection::connect` also covers `PgConnection::connect`.
    const PATTERNS: [&str; 8] = [
        ".pool()",
        "begin_unpinned",
        "audit_global(",
        "audit_for_org(",
        "audit_trail_for_user(",
        "PgPool",
        "PgListener::connect(",
        "Connection::connect",
    ];
    let allowed: BTreeMap<&str, usize> = BTreeMap::from([
        // The helper itself: opens the transaction it then drops into the
        // tenant role; acquires the opaque `Held` connection the key lookup
        // keeps across its transaction, so it can await the connection's
        // return to the pool before freeing its permit (#120; only `Held::begin`
        // can run anything on it); and reads the pool's size, which touches no
        // table.
        ("flags-core/src/unpinned.rs", 3),
        // Migrations run as the migrating role: DDL, grants, and the
        // security-definer purge function's owner.
        ("flags-core/src/migrate.rs", 1),
        // The flag-change listener (`use sqlx::PgPool` and `spawn`'s
        // parameter): its dedicated connection only `LISTEN`s.
        ("flags-core/src/notify.rs", 2),
        // `/readyz` runs `SELECT 1`: asks about the connection, touches no
        // table, and must not depend on the tenant role being assumable.
        ("flags-server/src/health.rs", 1),
        // Builds the pool (`PgPoolOptions`) that becomes the `Db`, and hands
        // the notify listener its clone.
        ("flags-server/src/main.rs", 2),
    ]);
    let found = count_in_crate_src(&PATTERNS);
    let found: BTreeMap<&str, usize> = found.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    assert_eq!(
        found, allowed,
        "raw pool access outside the allowed sites; route it through `unpinned::begin`"
    );
}

/// How often any of `patterns` occurs in each workspace crate's non-test
/// `src`, by path relative to `crates/`: comment lines and the trailing
/// `#[cfg(test)] mod tests` (which must be the file's last item) are skipped.
fn count_in_crate_src(patterns: &[&str]) -> BTreeMap<String, usize> {
    let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap();
    let mut found: BTreeMap<String, usize> = BTreeMap::new();
    let mut dirs = vec![crates.to_path_buf()];
    while let Some(dir) = dirs.pop() {
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let rel = path.strip_prefix(crates).unwrap();
            let name = rel.to_string_lossy().replace('\\', "/");
            if path.is_dir() {
                // Every crate's `src`, at any depth below it; never `tests`,
                // `target`, or `migrations`.
                let in_src = rel
                    .components()
                    .nth(1)
                    .is_none_or(|c| c.as_os_str() == "src");
                if in_src {
                    dirs.push(path);
                }
                continue;
            }
            if path.extension().is_none_or(|e| e != "rs") {
                continue;
            }
            let text = std::fs::read_to_string(&path)
                .unwrap()
                .replace("\r\n", "\n");
            let code = match text
                .find("#[cfg(test)]\nmod tests {")
                .or_else(|| text.find("#[cfg(test)]\npub(crate) mod tests {"))
            {
                Some(start) => {
                    // The test module must be the file's last item, so
                    // nothing after it escapes the scan. rustfmt (a gate)
                    // indents its body, so the only line at column 0 after
                    // its header is its closing brace, at the end.
                    let tail: Vec<&str> = text[start..]
                        .lines()
                        .skip(2)
                        .filter(|l| !l.is_empty() && !l.starts_with(char::is_whitespace))
                        .collect();
                    assert_eq!(
                        tail,
                        ["}"],
                        "{name}: code after the test module would escape this check"
                    );
                    &text[..start]
                }
                None => text.as_str(),
            };
            let n: usize = code
                .lines()
                .filter(|l| !l.trim_start().starts_with("//"))
                .map(|l| patterns.iter().map(|p| l.matches(p).count()).sum::<usize>())
                .sum();
            if n > 0 {
                found.insert(name, n);
            }
        }
    }
    found
}

/// Every pinned transaction in the crates' source opens through
/// `platform_events::begin_live` or `platform_events::begin_pinned`, which pin
/// the search path to `public` (#97); a bare `Db::begin` would run with the
/// role's default path, where a schema named after the role comes first.
/// Textual, like the scan above: it matches `db.begin(` and `Db::begin(`, and
/// every `Db` in the workspace is held as `db` (or `self.db`, `state.db`), so
/// a method call on one held under another name would escape it. If the
/// helpers move out of `platform_events.rs`, move the allowed site with them.
#[test]
fn no_pinned_begin_outside_the_search_path_helpers() {
    let found = count_in_crate_src(&["db.begin(", "Db::begin("]);
    let found: BTreeMap<&str, usize> = found.iter().map(|(k, v)| (k.as_str(), *v)).collect();
    assert_eq!(
        found,
        BTreeMap::from([("flags-core/src/platform_events.rs", 2)]),
        "open pinned transactions with `begin_live` or `begin_pinned`"
    );
}

/// `.` and `..` are URL dot segments, so an SDK evaluating them over
/// `POST /api/flags/{key}/evaluate` or `POST /api/evaluate/{key}` reaches
/// another route (#105). They are refused before anything is written.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn create_flag_refuses_dot_segment_keys(pool: PgPool) {
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let mut tx = db.begin(org).await.unwrap();
    let (app, _) = tx
        .create_app("web", &["production".into()], actor)
        .await
        .unwrap();
    for key in [".", ".."] {
        let err = tx
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
            .unwrap_err();
        assert_eq!(err.code(), "invalid_argument", "{key:?}: {err}");
    }
    assert!(tx.list_flags(&app, true).await.unwrap().is_empty());
}

/// A JSON string whose compact serialization is exactly `n` bytes.
fn sized(n: usize) -> serde_json::Value {
    json!("x".repeat(n - 2))
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn configuration_size_is_capped_on_write(pool: PgPool) {
    let max = eval::MAX_CONFIGURATION_BYTES;
    let db = db(pool).await;
    let org = OrgId::new();
    let actor = UserId::new();
    let mut tx = db.begin(org).await.unwrap();
    let (app, _) = tx
        .create_app("web", &["production".into()], actor)
        .await
        .unwrap();
    let new = |key: &str, configuration, variations| NewFlag {
        key: key.into(),
        configuration,
        variations,
        ..Default::default()
    };

    let err = tx
        .create_flag(&app, new("big", Some(sized(max + 1)), None), actor, None)
        .await
        .unwrap_err();
    assert_eq!(err.code(), "invalid_argument", "{err}");
    assert!(err.to_string().contains(&max.to_string()), "{err}");
    let over_variation = json!({"control": {}, "treatment": {"configuration": sized(max + 1)}});
    let err = tx
        .create_flag(
            &app,
            new("big-variation", None, Some(over_variation.clone())),
            actor,
            None,
        )
        .await
        .unwrap_err();
    assert_eq!(err.code(), "invalid_argument", "{err}");
    assert!(err.to_string().contains("\"treatment\""), "{err}");
    assert!(tx.list_flags(&app, true).await.unwrap().is_empty());

    let flag = tx
        .create_flag(
            &app,
            new(
                "at-limit",
                Some(sized(max)),
                Some(json!({"treatment": {"configuration": sized(max)}})),
            ),
            actor,
            None,
        )
        .await
        .unwrap();
    assert_eq!(flag.configuration, Some(sized(max)));

    for patch in [
        FlagPatch {
            configuration: Some(sized(max + 1)),
            ..Default::default()
        },
        FlagPatch {
            variations: Some(over_variation),
            ..Default::default()
        },
    ] {
        let err = tx
            .update_flag(&app, "at-limit", patch, meta(actor))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "invalid_argument", "{err}");
    }
    let updated = tx
        .update_flag(
            &app,
            "at-limit",
            FlagPatch {
                variations: Some(json!({"control": {"configuration": sized(max)}})),
                configuration: Some(sized(max)),
                ..Default::default()
            },
            meta(actor),
        )
        .await
        .unwrap();
    assert_eq!(updated.version, flag.version + 1);
    assert_eq!(
        updated.variations,
        json!({"control": {"configuration": sized(max)}})
    );
    // JSON null clears the configuration; it is never measured against the cap.
    let cleared = tx
        .update_flag(
            &app,
            "at-limit",
            FlagPatch {
                configuration: Some(serde_json::Value::Null),
                ..Default::default()
            },
            meta(actor),
        )
        .await
        .unwrap();
    assert_eq!(cleared.configuration, None);
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn stored_oversized_configuration_still_evaluates(pool: PgPool) {
    let max = eval::MAX_CONFIGURATION_BYTES;
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
                key: "legacy".into(),
                variations: Some(json!({"on": {}})),
                ..Default::default()
            },
            actor,
            None,
        )
        .await
        .unwrap();
    // Stored before the cap existed: written around the validation.
    let big = sized(max + 10_000);
    let big_variations = json!({"on": {"configuration": sized(max + 20_000)}});
    sqlx::query(
        "UPDATE feature_flags SET configuration = $3, variations = $4 \
         WHERE org_id = $1 AND id = $2",
    )
    .bind(org)
    .bind(flag.id)
    .bind(&big)
    .bind(&big_variations)
    .execute(tx.conn())
    .await
    .unwrap();
    // Neither the kill switch nor a targeting change re-checks the stored
    // configurations.
    tx.set_environment(
        &app,
        "legacy",
        "production",
        EnvPatch {
            enabled: Some(true),
            ..Default::default()
        },
        meta(actor),
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let mut tx = db.begin(org).await.unwrap();
    let stored = tx.find_flag(app.id, "legacy").await.unwrap().unwrap();
    // Off (no state for the environment): the flag's own configuration.
    let e = eval::evaluate(stored.view(), "staging", &Context::default());
    assert!(!e.enabled);
    assert_eq!(e.configuration, Some(big.clone()));
    let with_variation = tx
        .set_environment(
            &app,
            "legacy",
            "production",
            EnvPatch {
                default_variation: Some(Some("on".into())),
                ..Default::default()
            },
            meta(actor),
        )
        .await
        .unwrap();
    let e = eval::evaluate(with_variation.view(), "production", &Context::default());
    assert_eq!(e.variation.as_deref(), Some("on"));
    assert_eq!(e.configuration, Some(sized(max + 20_000)));

    // A write that does not carry the configurations leaves them alone.
    let renamed = tx
        .update_flag(
            &app,
            "legacy",
            FlagPatch {
                name: Some("Legacy".into()),
                ..Default::default()
            },
            meta(actor),
        )
        .await
        .unwrap();
    assert_eq!(renamed.name, "Legacy");
    assert_eq!(renamed.configuration, Some(big.clone()));
    assert_eq!(renamed.variations, big_variations);

    // Resubmitting either is a new write, so it is refused.
    for patch in [
        FlagPatch {
            configuration: Some(big.clone()),
            ..Default::default()
        },
        FlagPatch {
            variations: Some(big_variations.clone()),
            ..Default::default()
        },
    ] {
        let err = tx
            .update_flag(&app, "legacy", patch, meta(actor))
            .await
            .unwrap_err();
        assert_eq!(err.code(), "invalid_argument", "{err}");
    }

    // Shrinking it is allowed, and a rollback restores the stored state
    // unchecked: the incident path never fails on pre-cap data.
    let shrunk = tx
        .update_flag(
            &app,
            "legacy",
            FlagPatch {
                configuration: Some(json!({"small": true})),
                ..Default::default()
            },
            meta(actor),
        )
        .await
        .unwrap();
    assert_eq!(shrunk.configuration, Some(json!({"small": true})));
    let (back, to) = tx
        .rollback_flag(&app, "legacy", Some(renamed.version), meta(actor))
        .await
        .unwrap();
    assert_eq!(to, renamed.version);
    assert_eq!(back.configuration, Some(big));
    assert_eq!(back.variations, big_variations);
}

/// Make `n` changes to flag `key` (renames `{key}{i}`), so it gains `n` versions.
async fn bump(
    tx: &mut otto_tenant::Tx<'_>,
    app: &flags_core::apps::FlagApp,
    key: &str,
    n: usize,
    actor: UserId,
) {
    for i in 1..=n {
        tx.update_flag(
            app,
            key,
            FlagPatch {
                name: Some(format!("{key}{i}")),
                ..Default::default()
            },
            meta(actor),
        )
        .await
        .unwrap();
    }
}

/// (rows, min version, max version) of one flag's history, read as the
/// superuser pool (RLS-exempt) so it sees every org.
async fn versions_of(
    pool: &PgPool,
    flag: flags_core::ids::FlagId,
) -> (i64, Option<i32>, Option<i32>) {
    sqlx::query_as(
        "SELECT count(*), min(version), max(version) FROM flag_versions WHERE flag_id = $1",
    )
    .bind(flag)
    .fetch_one(pool)
    .await
    .unwrap()
}

/// #123: history is removed only by the background sweep, and only for
/// versions both older than `VERSION_RETENTION_DAYS` and outside the flag's
/// newest `MIN_RETAINED_VERSIONS`. However many writes follow a change, it
/// stays on record for 90 days: writes cannot push recent history out.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn flag_versions_are_swept_by_age_beyond_the_newest_100(pool: PgPool) {
    use flags_core::flags::{sweep_versions, MIN_RETAINED_VERSIONS, VERSION_RETENTION_DAYS};
    assert_eq!((VERSION_RETENTION_DAYS, MIN_RETAINED_VERSIONS), (90, 100));
    let db = db(pool.clone()).await;
    let (a, b) = (OrgId::new(), OrgId::new());
    let actor = UserId::new();

    let mut tx = db.begin(a).await.unwrap();
    let (app_a, _) = tx
        .create_app("web", &["production".into()], actor)
        .await
        .unwrap();
    let mut ids = vec![];
    for key in ["f", "recent"] {
        let flag = tx
            .create_flag(
                &app_a,
                NewFlag {
                    key: key.into(),
                    ..Default::default()
                },
                actor,
                None,
            )
            .await
            .unwrap();
        bump(&mut tx, &app_a, key, 104, actor).await;
        ids.push(flag.id);
    }
    tx.commit().await.unwrap();
    let (f, recent) = (ids[0], ids[1]);

    let mut tx = db.begin(b).await.unwrap();
    let (app_b, _) = tx
        .create_app("web", &["production".into()], actor)
        .await
        .unwrap();
    let flag_b = tx
        .create_flag(
            &app_b,
            NewFlag {
                key: "b".into(),
                ..Default::default()
            },
            actor,
            None,
        )
        .await
        .unwrap();
    bump(&mut tx, &app_b, "b", 2, actor).await;
    tx.commit().await.unwrap();

    // Age f's versions 1..=103 and all of b's past the retention window.
    sqlx::query(
        "UPDATE flag_versions SET created_at = now() - interval '91 days' \
         WHERE (flag_id = $1 AND version <= 103) OR flag_id = $2",
    )
    .bind(f)
    .bind(flag_b.id)
    .execute(&pool)
    .await
    .unwrap();

    assert_eq!(sweep_versions(&db).await.unwrap(), 5);
    // Version 6 is old but among f's newest 100, so it stays.
    assert_eq!(versions_of(&pool, f).await, (100, Some(6), Some(105)));
    // 105 recent versions: none is old, so none goes.
    assert_eq!(versions_of(&pool, recent).await, (105, Some(1), Some(105)));
    // Old, but fewer than 100 versions.
    assert_eq!(versions_of(&pool, flag_b.id).await, (3, Some(1), Some(3)));
    assert_eq!(sweep_versions(&db).await.unwrap(), 0);

    let mut tx = db.begin(a).await.unwrap();
    let err = tx
        .rollback_flag(&app_a, "f", Some(5), meta(actor))
        .await
        .unwrap_err();
    assert_eq!(err.code(), "version_not_found", "{err}");
    let msg = err.to_string();
    assert!(msg.contains("90 days") && msg.contains("100"), "{msg}");
    let (back, to) = tx
        .rollback_flag(&app_a, "f", Some(6), meta(actor))
        .await
        .unwrap();
    assert_eq!((to, back.version, back.name.as_str()), (6, 106, "f5"));
    // Recent history of a heavily written flag is all still restorable.
    let (back, _) = tx
        .rollback_flag(&app_a, "recent", Some(1), meta(actor))
        .await
        .unwrap();
    assert_eq!(back.name, "recent");
    tx.commit().await.unwrap();
}

/// The sweep's unpinned access to `flag_versions` (migration 0006) needs the
/// maintenance marker: a request path that forgot to pin sees and deletes
/// nothing; pinned transactions still see only their own org (#27, #123).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn flag_versions_need_the_maintenance_marker_unpinned(pool: PgPool) {
    let db = db(pool.clone()).await;
    let actor = UserId::new();
    for org in [OrgId::new(), OrgId::new()] {
        let mut tx = db.begin(org).await.unwrap();
        let (app, _) = tx
            .create_app("web", &["production".into()], actor)
            .await
            .unwrap();
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
        let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM flag_versions")
            .fetch_one(tx.conn())
            .await
            .unwrap();
        assert_eq!(seen, 1, "pinned sees only its own org");
        tx.commit().await.unwrap();
    }

    let mut tx = as_app_unpinned(&pool).await;
    let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM flag_versions")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(seen, 0);
    let deleted = sqlx::query("DELETE FROM flag_versions")
        .execute(&mut *tx)
        .await
        .unwrap()
        .rows_affected();
    assert_eq!(deleted, 0);
    tx.rollback().await.unwrap();

    let mut tx = as_app_maintenance(&pool).await;
    let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM flag_versions")
        .fetch_one(&mut *tx)
        .await
        .unwrap();
    assert_eq!(seen, 2);
    tx.rollback().await.unwrap();
}

/// The sweep drains a backlog in batches across every org: 2 100 eligible
/// rows in org A and 1 900 in org B, exactly four full batches, so the loop
/// also ends on an empty one. A row outside the newest 100 but only 89 days
/// old stays.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn flag_version_sweep_drains_a_backlog_in_batches(pool: PgPool) {
    use flags_core::flags::sweep_versions;
    let db = db(pool.clone()).await;
    let actor = UserId::new();
    let mut flags = vec![];
    for org in [OrgId::new(), OrgId::new()] {
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
        tx.commit().await.unwrap();
        flags.push((org, flag.id));
    }
    // History seeded directly (as the superuser pool): versions 2..=top, all
    // 91 days old, on top of the real version 1.
    for (&(org, flag), top) in flags.iter().zip([2_200, 2_001]) {
        sqlx::query(
            "INSERT INTO flag_versions (org_id, flag_id, version, change, snapshot, created_at) \
             SELECT $1, $2, v, 'update', '{}'::jsonb, now() - interval '91 days' \
               FROM generate_series(2, $3) AS v",
        )
        .bind(org)
        .bind(flag)
        .bind(top)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE flag_versions SET created_at = now() - interval '91 days' \
             WHERE flag_id = $1 AND version = 1",
        )
        .bind(flag)
        .execute(&pool)
        .await
        .unwrap();
    }
    // One of B's eligible-by-count rows is only 89 days old.
    sqlx::query(
        "UPDATE flag_versions SET created_at = now() - interval '89 days' \
         WHERE flag_id = $1 AND version = 7",
    )
    .bind(flags[1].1)
    .execute(&pool)
    .await
    .unwrap();

    // A: 2 200 versions, 2 100 swept. B: 2 001 versions, 1 901 past the
    // count, one of them too young, so 1 900 swept.
    assert_eq!(sweep_versions(&db).await.unwrap(), 4_000);
    assert_eq!(
        versions_of(&pool, flags[0].1).await,
        (100, Some(2_101), Some(2_200))
    );
    assert_eq!(
        versions_of(&pool, flags[1].1).await,
        (101, Some(7), Some(2_001))
    );
    assert_eq!(sweep_versions(&db).await.unwrap(), 0);
}

/// The sweep chooses rows by `created_at`, so tenant transactions cannot
/// rewrite history: `otto_app` has no UPDATE on `flag_versions` (0006).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn flag_versions_cannot_be_updated_by_the_app_role(pool: PgPool) {
    let db = db(pool.clone()).await;
    let (org, actor) = (OrgId::new(), UserId::new());
    let mut tx = db.begin(org).await.unwrap();
    let (app, _) = tx
        .create_app("web", &["production".into()], actor)
        .await
        .unwrap();
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

    let mut tx = db.begin(org).await.unwrap();
    let err = sqlx::query("UPDATE flag_versions SET created_at = now() - interval '1 year'")
        .execute(tx.conn())
        .await
        .unwrap_err();
    assert_eq!(sqlstate(&err), "42501", "{err}");
}

// ---- metering: the evaluation counter's flush (#140) ----

mod metering_flush {
    use super::*;
    use flags_core::ids::FlagAppId;
    use flags_core::metering::{self, Counts, EvalCounter};
    use std::time::Duration;

    async fn app_for(db: &Db, org: OrgId, name: &str) -> FlagAppId {
        let mut tx = db.begin(org).await.unwrap();
        let (app, _) = tx.create_app(name, &[], UserId::new()).await.unwrap();
        tx.commit().await.unwrap();
        app.id
    }

    async fn row(pool: &PgPool, org: OrgId, app: FlagAppId) -> Option<(i64, i64, bool)> {
        sqlx::query_as(
            "SELECT served, reported, hour = date_trunc('hour', now(), 'UTC') \
             FROM usage_meter_hourly WHERE org_id = $1 AND app_id = $2",
        )
        .bind(org)
        .bind(app)
        .fetch_optional(pool)
        .await
        .unwrap()
    }

    #[sqlx::test(migrator = "flags_core::MIGRATOR")]
    async fn flush_upserts_into_the_current_hour_and_adds_to_an_existing_row(pool: PgPool) {
        let db = db(pool.clone()).await;
        let org = OrgId::new();
        let app = app_for(&db, org, "web").await;
        let counter = EvalCounter::default();

        counter.served(org, app, 5);
        counter.reported(org, app, 2);
        assert_eq!(metering::flush(&db, &counter).await.unwrap(), 1);
        assert_eq!(row(&pool, org, app).await, Some((5, 2, true)));
        assert!(counter.take().is_empty(), "a flush empties the counter");

        counter.served(org, app, 1);
        counter.reported(org, app, 10);
        assert_eq!(metering::flush(&db, &counter).await.unwrap(), 1);
        assert_eq!(row(&pool, org, app).await, Some((6, 12, true)));

        // Nothing to write: no statement, no row change.
        assert_eq!(metering::flush(&db, &counter).await.unwrap(), 0);
        assert_eq!(row(&pool, org, app).await, Some((6, 12, true)));
    }

    #[sqlx::test(migrator = "flags_core::MIGRATOR")]
    async fn flush_skips_zero_pairs(pool: PgPool) {
        let db = db(pool.clone()).await;
        let org = OrgId::new();
        let app = app_for(&db, org, "web").await;
        let counter = EvalCounter::default();
        counter.served(org, app, 0);
        assert_eq!(metering::flush(&db, &counter).await.unwrap(), 0);
        assert_eq!(row(&pool, org, app).await, None);
    }

    #[sqlx::test(migrator = "flags_core::MIGRATOR")]
    async fn flush_skips_deleted_orgs_and_missing_apps_without_failing_the_batch(pool: PgPool) {
        let db = db(pool.clone()).await;
        let (good, gone) = (OrgId::new(), OrgId::new());
        let good_app = app_for(&db, good, "web").await;
        let gone_app = app_for(&db, gone, "web").await;
        let ghost = FlagAppId::new();
        sqlx::query("INSERT INTO deleted_orgs (org_id) VALUES ($1)")
            .bind(gone)
            .execute(&pool)
            .await
            .unwrap();

        let counter = EvalCounter::default();
        counter.served(good, good_app, 3);
        counter.served(gone, gone_app, 4);
        counter.served(good, ghost, 5);
        assert_eq!(metering::flush(&db, &counter).await.unwrap(), 1);

        assert_eq!(row(&pool, good, good_app).await, Some((3, 0, true)));
        assert_eq!(row(&pool, gone, gone_app).await, None);
        assert_eq!(row(&pool, good, ghost).await, None);
    }

    #[sqlx::test(migrator = "flags_core::MIGRATOR")]
    async fn flush_clamps_values_above_i64_max(pool: PgPool) {
        let db = db(pool.clone()).await;
        let org = OrgId::new();
        let app = app_for(&db, org, "web").await;
        let counter = EvalCounter::default();

        counter.served(org, app, u64::MAX);
        counter.reported(org, app, i64::MAX as u64);
        metering::flush(&db, &counter).await.unwrap();
        assert_eq!(row(&pool, org, app).await, Some((i64::MAX, i64::MAX, true)));

        // Adding to a saturated row clamps instead of overflowing the bigint.
        counter.served(org, app, 10);
        counter.reported(org, app, u64::MAX);
        metering::flush(&db, &counter).await.unwrap();
        assert_eq!(row(&pool, org, app).await, Some((i64::MAX, i64::MAX, true)));
    }

    #[sqlx::test(migrator = "flags_core::MIGRATOR")]
    async fn a_failed_flush_restores_the_taken_counts(pool: PgPool) {
        let db = db(pool.clone()).await;
        let org = OrgId::new();
        let app = app_for(&db, org, "web").await;
        sqlx::query("ALTER TABLE usage_meter_hourly RENAME TO usage_meter_hourly_away")
            .execute(&pool)
            .await
            .unwrap();
        let counter = EvalCounter::default();
        counter.served(org, app, 7);
        counter.reported(org, app, 3);

        metering::flush(&db, &counter).await.unwrap_err();
        // A concurrent add after the restore is kept alongside it.
        counter.served(org, app, 1);
        let restored = counter.take();
        assert_eq!(
            restored.get(&(org, app)),
            Some(&Counts {
                served: 8,
                reported: 3
            })
        );

        sqlx::query("ALTER TABLE usage_meter_hourly_away RENAME TO usage_meter_hourly")
            .execute(&pool)
            .await
            .unwrap();
        counter.restore(restored);
        metering::flush(&db, &counter).await.unwrap();
        assert_eq!(row(&pool, org, app).await, Some((8, 3, true)));
    }

    /// The server aborts a flush transaction left idle past its timeout, so a
    /// suspended machine cannot commit into a closed hour.
    #[sqlx::test(migrator = "flags_core::MIGRATOR")]
    async fn a_flush_transaction_left_idle_is_aborted_and_its_counts_restored(pool: PgPool) {
        let db = db(pool.clone()).await;
        let org = OrgId::new();
        let app = app_for(&db, org, "web").await;
        let counter = EvalCounter::default();
        counter.served(org, app, 9);

        metering::flush_with(&db, &counter, "200ms", Duration::from_secs(1))
            .await
            .unwrap_err();
        assert_eq!(row(&pool, org, app).await, None);
        assert_eq!(
            counter.take().get(&(org, app)),
            Some(&Counts {
                served: 9,
                reported: 0
            })
        );
    }
}

// ---- metering: closing finished hours into the usage outbox (#140) ----

mod metering_close {
    use super::*;
    use flags_core::ids::FlagAppId;
    use flags_core::metering::{self, CLOSE_BATCH_ORGS, EVALUATIONS_TOOL, METER_EVALUATIONS};
    use flags_core::usage::BlockedLookup;
    use std::future::{ready, Ready};
    use std::sync::Mutex;

    async fn app_for(db: &Db, org: OrgId, name: &str) -> FlagAppId {
        let mut tx = db.begin(org).await.unwrap();
        let (app, _) = tx.create_app(name, &[], UserId::new()).await.unwrap();
        tx.commit().await.unwrap();
        app.id
    }

    /// Insert a row for the hour holding `now() - ago` (any SQL interval),
    /// truncated as a flush writes it, returning its hour.
    async fn hour_row(
        pool: &PgPool,
        org: OrgId,
        app: FlagAppId,
        ago: &str,
        served: i64,
        reported: i64,
    ) -> chrono::DateTime<Utc> {
        insert_row(pool, org, app, ago, true, (served, reported)).await
    }

    /// Insert a row at exactly `now() - ago`, not truncated, to probe the due
    /// boundary to the second.
    async fn exact_row(
        pool: &PgPool,
        org: OrgId,
        app: FlagAppId,
        ago: &str,
        served: i64,
    ) -> chrono::DateTime<Utc> {
        insert_row(pool, org, app, ago, false, (served, 0)).await
    }

    async fn insert_row(
        pool: &PgPool,
        org: OrgId,
        app: FlagAppId,
        ago: &str,
        truncate: bool,
        (served, reported): (i64, i64),
    ) -> chrono::DateTime<Utc> {
        sqlx::query_scalar(
            "INSERT INTO usage_meter_hourly (org_id, app_id, hour, served, reported) \
             VALUES ($1, $2, CASE WHEN $6 THEN date_trunc('hour', now() - $3::interval, 'UTC') \
                                  ELSE now() - $3::interval END, $4, $5) RETURNING hour",
        )
        .bind(org)
        .bind(app)
        .bind(ago)
        .bind(served)
        .bind(reported)
        .bind(truncate)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    /// Two hours back: always due.
    const DUE: &str = "2 hours";

    async fn hourly_left(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM usage_meter_hourly")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    #[derive(Debug, PartialEq, Eq, sqlx::FromRow)]
    struct Out {
        org: OrgId,
        tool: String,
        billable: bool,
        meter: Option<String>,
        quantity: Option<i64>,
        occurred_at: chrono::DateTime<Utc>,
        user_id: Option<uuid::Uuid>,
    }

    async fn outbox(pool: &PgPool) -> Vec<Out> {
        sqlx::query_as(
            "SELECT org_id AS org, tool, billable, meter, quantity, occurred_at, user_id \
             FROM usage_outbox ORDER BY org_id, occurred_at, quantity",
        )
        .fetch_all(pool)
        .await
        .unwrap()
    }

    fn not_blocked(_: OrgId) -> Ready<BlockedLookup> {
        ready(BlockedLookup::NotBlocked)
    }

    fn billed(org: OrgId, quantity: i64, occurred_at: chrono::DateTime<Utc>) -> Out {
        Out {
            org,
            tool: EVALUATIONS_TOOL.to_string(),
            billable: true,
            meter: Some(METER_EVALUATIONS.to_string()),
            quantity: Some(quantity),
            occurred_at,
            user_id: None,
        }
    }

    fn unbilled(org: OrgId, occurred_at: chrono::DateTime<Utc>) -> Out {
        Out {
            org,
            tool: EVALUATIONS_TOOL.to_string(),
            billable: false,
            meter: None,
            quantity: None,
            occurred_at,
            user_id: None,
        }
    }

    #[sqlx::test(migrator = "flags_core::MIGRATOR")]
    async fn only_due_hours_close(pool: PgPool) {
        let db = db(pool.clone()).await;
        let org = OrgId::new();
        let app = app_for(&db, org, "web").await;
        // Due: the hour ended at least CLOSE_GRACE (5 min) ago.
        let old = hour_row(&pool, org, app, DUE, 4, 1).await;
        let edge = exact_row(&pool, org, app, "65 minutes 1 second", 2).await;
        // Not due: still inside the grace, or the current, open hour.
        exact_row(&pool, org, app, "64 minutes", 3).await;
        hour_row(&pool, org, app, "0 seconds", 5, 0).await;

        assert_eq!(metering::close_due(&db, not_blocked).await.unwrap(), 2);
        assert_eq!(hourly_left(&pool).await, 2);
        let mut want = vec![billed(org, 4, old), billed(org, 2, edge)];
        want.sort_by_key(|o| o.occurred_at);
        assert_eq!(outbox(&pool).await, want);

        // Nothing more is due: a second pass closes nothing.
        assert_eq!(metering::close_due(&db, not_blocked).await.unwrap(), 0);
        assert_eq!(outbox(&pool).await.len(), 2);
    }

    #[sqlx::test(migrator = "flags_core::MIGRATOR")]
    async fn a_billed_org_gets_the_larger_count_metered_at_its_hour(pool: PgPool) {
        let db = db(pool.clone()).await;
        let org = OrgId::new();
        let (web, api) = (
            app_for(&db, org, "web").await,
            app_for(&db, org, "api").await,
        );
        let h_web = hour_row(&pool, org, web, DUE, 3, 7).await;
        let h_api = hour_row(&pool, org, api, DUE, 9, 2).await;
        assert_eq!(h_web, h_api);

        assert_eq!(metering::close_due(&db, not_blocked).await.unwrap(), 2);
        // GREATEST per (org, app, hour), never the sum of the two sources.
        assert_eq!(
            outbox(&pool).await,
            vec![billed(org, 7, h_web), billed(org, 9, h_api)]
        );
        assert_eq!(hourly_left(&pool).await, 0);
    }

    #[sqlx::test(migrator = "flags_core::MIGRATOR")]
    async fn a_blocked_org_ships_unmetered(pool: PgPool) {
        let db = db(pool.clone()).await;
        let (blocked, paying) = (OrgId::new(), OrgId::new());
        let b_app = app_for(&db, blocked, "web").await;
        let p_app = app_for(&db, paying, "web").await;
        let b1 = hour_row(&pool, blocked, b_app, DUE, 5, 6).await;
        let b2 = hour_row(&pool, blocked, b_app, "3 hours", 1, 0).await;
        let p1 = hour_row(&pool, paying, p_app, DUE, 8, 0).await;

        let lookup = |org: OrgId| {
            ready(if org == blocked {
                BlockedLookup::Blocked
            } else {
                BlockedLookup::NotBlocked
            })
        };
        assert_eq!(metering::close_due(&db, lookup).await.unwrap(), 3);
        let mut want = vec![
            unbilled(blocked, b1),
            unbilled(blocked, b2),
            billed(paying, 8, p1),
        ];
        want.sort_by(|a, b| {
            (a.org.as_uuid(), a.occurred_at).cmp(&(b.org.as_uuid(), b.occurred_at))
        });
        assert_eq!(outbox(&pool).await, want);
        assert_eq!(hourly_left(&pool).await, 0);
    }

    #[sqlx::test(migrator = "flags_core::MIGRATOR")]
    async fn concurrent_passes_close_each_row_exactly_once(pool: PgPool) {
        let db = db(pool.clone()).await;
        let mut total = 0i64;
        let mut rows = 0usize;
        for i in 0..8 {
            let org = OrgId::new();
            let app = app_for(&db, org, "web").await;
            for (h, ago) in ["2 hours", "3 hours", "4 hours"].into_iter().enumerate() {
                let (served, reported) = (i * 10 + h as i64 + 1, i);
                hour_row(&pool, org, app, ago, served, reported).await;
                total += served.max(reported);
                rows += 1;
            }
        }
        // A lookup that yields, so the two passes interleave.
        let slow = |_: OrgId| async {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            BlockedLookup::NotBlocked
        };
        let (a, b) = tokio::join!(
            metering::close_due(&db, slow),
            metering::close_due(&db, slow)
        );
        assert_eq!(a.unwrap() + b.unwrap(), rows);
        let out = outbox(&pool).await;
        assert_eq!(out.len(), rows);
        assert_eq!(out.iter().filter_map(|o| o.quantity).sum::<i64>(), total);
        assert_eq!(hourly_left(&pool).await, 0);
    }

    #[sqlx::test(migrator = "flags_core::MIGRATOR")]
    async fn a_quantity_above_the_cap_is_split_into_rows_that_sum_to_it(pool: PgPool) {
        let db = db(pool.clone()).await;
        let org = OrgId::new();
        let (a, b, c) = (
            app_for(&db, org, "a").await,
            app_for(&db, org, "b").await,
            app_for(&db, org, "c").await,
        );
        let h = hour_row(&pool, org, a, DUE, 25, 3).await;
        hour_row(&pool, org, b, DUE, 0, 20).await;
        hour_row(&pool, org, c, DUE, 10, 0).await;

        assert_eq!(
            metering::close_due_with(&db, not_blocked, 10)
                .await
                .unwrap(),
            3
        );
        let mut q: Vec<i64> = outbox(&pool)
            .await
            .into_iter()
            .inspect(|o| assert_eq!(*o, billed(org, o.quantity.unwrap(), h)))
            .map(|o| o.quantity.unwrap())
            .collect();
        q.sort_unstable();
        // 25 → 10 + 10 + 5; 20 → 10 + 10; 10 → 10.
        assert_eq!(q, vec![5, 10, 10, 10, 10, 10]);
    }

    #[sqlx::test(migrator = "flags_core::MIGRATOR")]
    async fn a_failed_lookup_skips_the_rest_and_bills_every_org(pool: PgPool) {
        let db = db(pool.clone()).await;
        let mut orgs = Vec::new();
        for _ in 0..3 {
            let org = OrgId::new();
            let app = app_for(&db, org, "web").await;
            hour_row(&pool, org, app, DUE, 4, 0).await;
            orgs.push(org);
        }
        let calls = Mutex::new(Vec::new());
        let lookup = |org: OrgId| {
            let mut calls = calls.lock().unwrap();
            calls.push(org);
            // Fails for the first org asked; would block every later one.
            ready(if calls.len() == 1 {
                BlockedLookup::Failed
            } else {
                BlockedLookup::Blocked
            })
        };
        assert_eq!(metering::close_due(&db, lookup).await.unwrap(), 3);
        assert_eq!(calls.lock().unwrap().len(), 1, "no lookups after a failure");
        let out = outbox(&pool).await;
        assert_eq!(out.len(), 3);
        assert!(
            out.iter().all(|o| o.billable && o.quantity == Some(4)),
            "{out:?}"
        );
        let billed_orgs: BTreeSet<_> = out.iter().map(|o| o.org.as_uuid()).collect();
        let all: BTreeSet<_> = orgs.iter().map(|o| o.as_uuid()).collect();
        assert_eq!(billed_orgs, all);
    }

    #[sqlx::test(migrator = "flags_core::MIGRATOR")]
    async fn rows_with_nothing_to_bill_are_deleted_without_an_outbox_row(pool: PgPool) {
        let db = db(pool.clone()).await;
        let (org, blocked) = (OrgId::new(), OrgId::new());
        let app = app_for(&db, org, "web").await;
        let b_app = app_for(&db, blocked, "web").await;
        hour_row(&pool, org, app, DUE, 0, 0).await;
        hour_row(&pool, blocked, b_app, DUE, 0, 0).await;
        let lookup = |o: OrgId| {
            ready(if o == blocked {
                BlockedLookup::Blocked
            } else {
                BlockedLookup::NotBlocked
            })
        };
        assert_eq!(metering::close_due(&db, lookup).await.unwrap(), 2);
        assert_eq!(hourly_left(&pool).await, 0);
        assert!(outbox(&pool).await.is_empty());
    }

    #[sqlx::test(migrator = "flags_core::MIGRATOR")]
    async fn a_pass_closes_at_most_the_batch_of_orgs(pool: PgPool) {
        let db = db(pool.clone()).await;
        let n = usize::try_from(CLOSE_BATCH_ORGS).unwrap() + 3;
        for _ in 0..n {
            let org = OrgId::new();
            let app = app_for(&db, org, "web").await;
            hour_row(&pool, org, app, DUE, 1, 0).await;
            hour_row(&pool, org, app, "3 hours", 1, 0).await;
        }
        let calls = Mutex::new(0usize);
        let counting = |_: OrgId| {
            *calls.lock().unwrap() += 1;
            ready(BlockedLookup::NotBlocked)
        };
        let batch = usize::try_from(CLOSE_BATCH_ORGS).unwrap();
        assert_eq!(
            metering::close_due(&db, &counting).await.unwrap(),
            2 * batch
        );
        assert_eq!(*calls.lock().unwrap(), batch);
        let orgs: BTreeSet<_> = outbox(&pool)
            .await
            .iter()
            .map(|o| o.org.as_uuid())
            .collect();
        assert_eq!(orgs.len(), batch);
        assert_eq!(metering::close_due(&db, &counting).await.unwrap(), 6);
        assert_eq!(hourly_left(&pool).await, 0);
    }
}
