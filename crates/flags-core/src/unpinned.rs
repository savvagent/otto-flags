//! Unpinned transactions that still run under row-level security.
//!
//! Some queries cannot pin an org because the org is what they are looking
//! for, or what they are about to tombstone: SDK key resolution
//! ([`crate::keys::resolve_hash`]), the revocation check
//! ([`crate::platform_events::revoked`]), and the lifecycle tombstones. They
//! touch only the tables deliberately left outside row-level security
//! (`app_keys`, `deleted_orgs`, `removed_members`).
//!
//! They still go through [`begin`] (or [`Held::begin`], on a connection the
//! key lookup holds), never a bare query on the pool: the pool
//! may connect as a role row-level security does not apply to (production's
//! attach role is a superuser), and a later query on that connection that
//! named a tenant table by mistake would read every org's rows (#80). Inside
//! [`begin`] the transaction runs as the tenant role with no org pinned and no
//! maintenance marker, so every tenant table's policies return nothing.
//!
//! `tests/db.rs`' `no_raw_pool_queries_outside_the_allowed_sites` refuses a new
//! raw pool query anywhere else.

use std::time::Duration;

use otto_tenant::Db;
use sqlx::pool::PoolConnection;
use sqlx::{Connection, PgConnection, Postgres, Transaction};

/// The `search_path` every tenant, unpinned, and maintenance transaction runs
/// with (#97): unqualified names resolve in `public` after the built-ins,
/// never in a schema named after the current role (`"$user"`, first in the
/// default path), and a temporary table (`pg_temp`, otherwise searched first
/// for relations) can no longer shadow a `public` one. The same value
/// `0004_audit_purge.sql` gives the purge function. Set transaction-locally,
/// so it ends with the transaction like the role. Pinned transactions get it
/// from `platform_events::begin_live` or `platform_events::begin_pinned`.
pub(crate) const SEARCH_PATH: &str = "pg_catalog, public, pg_temp";

/// Open an unpinned transaction on `db`'s pool as the tenant role, with no
/// maintenance marker. See [`begin_as_tenant_role`] for the role rules.
pub(crate) async fn begin(db: &Db) -> sqlx::Result<Transaction<'static, Postgres>> {
    begin_as_tenant_role(db, false).await
}

/// Open an unpinned transaction on `db`'s pool, as the tenant role
/// (`db.tenant_role()`) when the pool's role can assume it, setting the
/// maintenance marker `app.maintenance = 'on'` only when `maintenance` is
/// true. Only [`crate::maintenance::begin`] passes true; everything else
/// calls [`begin`]. Role, marker, and search path ([`SEARCH_PATH`]) are all
/// transaction-local (`set_config(…, true)`, which for `role` is
/// `SET LOCAL ROLE`), so they end with the transaction and never leak to the
/// next user of the connection.
///
/// Whether the role can be assumed is the question `otto_tenant::Db::begin`
/// asks before its own `SET LOCAL ROLE`, with the same catalog query: its
/// answer is private to otto-tenant, so it is asked again here, once per
/// transaction (#79 will use otto-tenant's own). Where it cannot be assumed
/// the role is left alone, as `Db::begin` leaves it, which is only safe when
/// the pool's own role is subject to row-level security (a managed Postgres
/// role with no CREATEROLE). `Db::verify_tenant_isolation` checks that once at
/// startup; this checks it on every transaction, for the role the transaction
/// will actually run as: a pool role exempt from row-level security
/// (superuser or BYPASSRLS) that cannot assume the tenant role, say after its
/// membership was revoked at runtime, or a tenant role that is itself exempt,
/// fails the transaction instead of quietly running unconstrained. The failure
/// is `sqlx::Error::Configuration`, logged with reason `configuration`.
pub(crate) async fn begin_as_tenant_role(
    db: &Db,
    maintenance: bool,
) -> sqlx::Result<Transaction<'static, Postgres>> {
    enter(db.pool().begin().await?, db, maintenance, None).await
}

/// Server-side timeouts for one transaction (#130), set by its probe and,
/// like everything the probe sets, transaction-local. Only the key lookup
/// passes them ([`Held::begin`]); every other transaction runs with the
/// server's defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Timeouts {
    /// `lock_timeout`: how long a statement waits for a lock.
    pub lock: Duration,
    /// `statement_timeout`: how long a statement runs.
    pub statement: Duration,
    /// `idle_in_transaction_session_timeout`: how long the backend waits for
    /// the client's next statement before ending the session, so a backend
    /// whose client gave up does not linger holding its locks.
    pub idle: Duration,
}

/// A setting's value in whole milliseconds, as Postgres reads it, rounded up
/// and never below 1 ms: Postgres reads `0` as "no timeout", so a bound under a
/// millisecond must not round down to switching the timeout off.
fn millis(d: Duration) -> String {
    format!("{}ms", d.as_nanos().div_ceil(1_000_000).max(1))
}

/// A pool connection held across one unpinned transaction, for the key
/// lookup ([`crate::keys::resolve_hash`]), which must await the connection's
/// return to the pool itself so its permit covers the whole checkout (#120).
///
/// Opaque on purpose: the raw connection never leaves this module, so the
/// only thing a holder can run on it is [`Held::begin`]'s probed transaction,
/// never a bare query as the pool's role (#80).
pub(crate) struct Held(PoolConnection<Postgres>);

impl Held {
    /// Take a connection from `db`'s pool.
    pub(crate) async fn acquire(db: &Db) -> sqlx::Result<Self> {
        Ok(Self(db.pool().acquire().await?))
    }

    /// [`begin`] on the held connection: the same probe, role switch, search
    /// path, and refusal, with no maintenance marker, and with `timeouts`
    /// set by the probe when given (#130).
    pub(crate) async fn begin<'c>(
        &'c mut self,
        db: &Db,
        timeouts: Option<Timeouts>,
    ) -> sqlx::Result<Transaction<'c, Postgres>> {
        let conn: &'c mut PgConnection = &mut self.0;
        enter(conn.begin().await?, db, false, timeouts).await
    }

    /// Return the connection to the pool, and wait until it is back: the
    /// rollback of any transaction left open and sqlx's release ping run
    /// inside this call, not in the task sqlx spawns when a `PoolConnection`
    /// is dropped. A connection that fails the ping is closed instead.
    ///
    /// Waits at most `within` (#130): a half-open connection never answers
    /// the ping, and the kernel can take many minutes to notice. True if
    /// sqlx's return finished in time, whether it pooled the connection or
    /// closed it; false if it was cut short. Cutting it short never leaves
    /// the connection checked out once this returns: `return_to_pool` takes
    /// the connection out of the `PoolConnection` before it returns its
    /// future, and the future owns it, so dropping the future drops the
    /// connection, closing its socket, and its pool slot, decrementing the
    /// pool's size (sqlx-core 0.8.6 `pool/connection.rs`: `return_to_pool`,
    /// `Floating`, `DecrementSizeGuard`'s `Drop`). The emptied
    /// `PoolConnection` then drops with nothing to return.
    pub(crate) async fn release(mut self, within: Duration) -> bool {
        tokio::time::timeout(within, self.0.return_to_pool())
            .await
            .is_ok()
    }

    /// Close the connection instead of returning it (#130), for one that
    /// missed a deadline and may be mid-protocol. Detaching it frees its pool
    /// slot at once, so a replacement may open, and dropping it closes its
    /// socket, both before this returns (sqlx-core 0.8.6
    /// `pool/connection.rs`: `PoolConnection::detach`, `Floating::detach`).
    /// A backend left mid-transaction ends itself when it sees the socket
    /// close, or at its `idle_in_transaction_session_timeout`.
    pub(crate) fn discard(self) {
        drop(self.0.detach());
    }
}

/// The most connections `db`'s pool opens. Reads the pool's options; touches
/// no table.
pub(crate) fn max_connections(db: &Db) -> u32 {
    db.pool().options().get_max_connections()
}

/// The probe's columns, in order: `assumable`, `pool_exempt`,
/// `tenant_exempt`, then what each `set_config` returned (the marker, the
/// role, the search path, and the three timeouts), read only to be
/// discarded.
type ProbeRow = (
    bool,
    bool,
    bool,
    Option<String>,
    Option<String>,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
);

/// Run the role probe in the freshly opened `tx` and switch it into the
/// tenant role, or refuse it; see [`begin_as_tenant_role`]. `timeouts`, when
/// given, are set by the same statement ([`Timeouts`]).
async fn enter<'c>(
    mut tx: Transaction<'c, Postgres>,
    db: &Db,
    maintenance: bool,
    timeouts: Option<Timeouts>,
) -> sqlx::Result<Transaction<'c, Postgres>> {
    // One round trip for the probes, the marker, the role switch, and the
    // search path (these run on every SDK key lookup). The probes are a
    // MATERIALIZED CTE so they read `current_user` before the outer select
    // list can switch the role. The marker and role `set_config`s each sit in
    // their own select-list column inside a CASE: a volatile function in a
    // CASE arm runs only when that arm is taken, and a column of its own is
    // never skipped the way one operand of a boolean expression can be. Exemption is a role attribute, never inherited
    // through membership, so each role is checked by name. If the result is
    // refused below, the switch is rolled back with the transaction. The
    // search path is set unconditionally, in a column of its own: the pool's
    // own role, kept when the tenant role cannot be assumed, has the same
    // `"$user"`-first default path.
    //
    // The timeouts (#130) are set the same way, each in a CASE column of its
    // own, and only when given: every other caller binds NULL and runs
    // unchanged, with one statement text for all of them. They are set here
    // because they take effect from the next statement, the key lookup; set
    // in the lookup itself they would be too late, since its lock on
    // `app_keys` is taken while the statement is bound, before its select
    // list runs, and its statement timer is armed when it starts.
    let (assumable, pool_exempt, tenant_exempt, ..): ProbeRow = sqlx::query_as(
        "WITH probe AS MATERIALIZED ( \
           SELECT EXISTS ( \
                    SELECT 1 FROM pg_catalog.pg_roles r \
                    WHERE r.rolname = $1::text \
                      AND pg_catalog.pg_has_role(current_user, r.oid, 'USAGE')) AS assumable, \
                  EXISTS ( \
                    SELECT 1 FROM pg_catalog.pg_roles r \
                    WHERE r.rolname = current_user AND (r.rolsuper OR r.rolbypassrls)) \
                    AS pool_exempt, \
                  EXISTS ( \
                    SELECT 1 FROM pg_catalog.pg_roles r \
                    WHERE r.rolname = $1::text AND (r.rolsuper OR r.rolbypassrls)) \
                    AS tenant_exempt) \
         SELECT assumable, pool_exempt, tenant_exempt, \
                CASE WHEN $2 THEN set_config('app.maintenance', 'on', true) END, \
                CASE WHEN assumable THEN set_config('role', $1::text, true) END, \
                set_config('search_path', $3::text, true), \
                CASE WHEN $4::text IS NOT NULL \
                  THEN set_config('lock_timeout', $4::text, true) END, \
                CASE WHEN $5::text IS NOT NULL \
                  THEN set_config('statement_timeout', $5::text, true) END, \
                CASE WHEN $6::text IS NOT NULL \
                  THEN set_config('idle_in_transaction_session_timeout', $6::text, true) END \
         FROM probe",
    )
    .bind(db.tenant_role())
    .bind(maintenance)
    .bind(SEARCH_PATH)
    .bind(timeouts.map(|t| millis(t.lock)))
    .bind(timeouts.map(|t| millis(t.statement)))
    .bind(timeouts.map(|t| millis(t.idle)))
    .fetch_one(&mut *tx)
    .await?;
    let runs_exempt = if assumable {
        tenant_exempt
    } else {
        pool_exempt
    };
    if runs_exempt {
        // Roll back now rather than on drop, so the role switch (taken when
        // the tenant role itself is exempt) visibly never outlives the refusal.
        tx.rollback().await?;
        let kind = if maintenance {
            "maintenance"
        } else {
            "unpinned"
        };
        return Err(sqlx::Error::Configuration(
            format!(
                "{kind} transactions must be subject to row-level security, but they \
                 would run exempt from it (tenant role {:?}, assumable: {assumable})",
                db.tenant_role()
            )
            .into(),
        ));
    }
    Ok(tx)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use sqlx::PgPool;

    /// The current role and the maintenance marker inside `tx`.
    async fn role_and_marker(tx: &mut Transaction<'_, Postgres>) -> (String, String) {
        sqlx::query_as(
            "SELECT current_user::text, coalesce(current_setting('app.maintenance', true), '')",
        )
        .fetch_one(&mut **tx)
        .await
        .unwrap()
    }

    /// The test pool connects as a superuser, as production's attach role
    /// does. An unpinned transaction runs as the tenant role, without the
    /// maintenance marker.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn begin_runs_as_the_tenant_role_without_the_marker(pool: PgPool) {
        let db = Db::from_pool(pool);
        let mut tx = begin(&db).await.unwrap();
        assert_eq!(
            role_and_marker(&mut tx).await,
            (db.tenant_role().to_string(), String::new())
        );
    }

    /// The same on a held connection (`Held::begin`, #120): the
    /// tenant role, no marker, the pinned search path.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_held_connection_begins_as_the_tenant_role_without_the_marker(pool: PgPool) {
        let db = Db::from_pool(pool);
        let mut conn = Held::acquire(&db).await.unwrap();
        let mut tx = conn.begin(&db, Some(LOOKUP_TIMEOUTS)).await.unwrap();
        assert_eq!(
            role_and_marker(&mut tx).await,
            (db.tenant_role().to_string(), String::new())
        );
        let path: String = sqlx::query_scalar("SELECT current_setting('search_path')")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(path, SEARCH_PATH);
    }

    /// Timeouts distinct from each other and from any default, so a mix-up
    /// between the three shows.
    const LOOKUP_TIMEOUTS: Timeouts = Timeouts {
        lock: Duration::from_millis(250),
        statement: Duration::from_millis(750),
        idle: Duration::from_millis(1500),
    };

    /// `lock_timeout`, `statement_timeout`, and
    /// `idle_in_transaction_session_timeout` inside `tx`.
    async fn timeouts(tx: &mut Transaction<'_, Postgres>) -> (String, String, String) {
        sqlx::query_as(
            "SELECT current_setting('lock_timeout'), current_setting('statement_timeout'), \
                    current_setting('idle_in_transaction_session_timeout')",
        )
        .fetch_one(&mut **tx)
        .await
        .unwrap()
    }

    /// A bound under a millisecond rounds up rather than down to `0`, which
    /// Postgres reads as "no timeout".
    #[test]
    fn a_sub_millisecond_bound_never_switches_the_timeout_off() {
        assert_eq!(millis(Duration::ZERO), "1ms");
        assert_eq!(millis(Duration::from_micros(1)), "1ms");
        assert_eq!(millis(Duration::from_micros(1500)), "2ms");
        assert_eq!(millis(Duration::from_millis(500)), "500ms");
    }

    fn unset() -> (String, String, String) {
        ("0".into(), "0".into(), "0".into())
    }

    /// A held transaction begun with timeouts runs under them, and they end
    /// with it: the next transaction on the same connection is back to the
    /// defaults (#130).
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn a_held_connection_sets_the_lookup_timeouts(pool: PgPool) {
        // One connection, so the plain `begin` below reuses the held one.
        let one = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(pool.connect_options().as_ref().clone())
            .await
            .unwrap();
        let db = Db::from_pool(one);
        let mut conn = Held::acquire(&db).await.unwrap();
        let mut tx = conn.begin(&db, Some(LOOKUP_TIMEOUTS)).await.unwrap();
        assert_eq!(
            timeouts(&mut tx).await,
            ("250ms".into(), "750ms".into(), "1500ms".into())
        );
        tx.commit().await.unwrap();
        assert!(conn.release(Duration::from_secs(5)).await);
        let mut tx = begin(&db).await.unwrap();
        assert_eq!(timeouts(&mut tx).await, unset());
        tx.rollback().await.unwrap();
        db.pool().close().await;
    }

    /// Every other unpinned and maintenance transaction leaves the timeouts
    /// alone (#130).
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn begin_leaves_the_timeouts_alone(pool: PgPool) {
        let db = Db::from_pool(pool);
        let mut tx = begin(&db).await.unwrap();
        assert_eq!(timeouts(&mut tx).await, unset());
        let mut tx = begin_as_tenant_role(&db, true).await.unwrap();
        assert_eq!(timeouts(&mut tx).await, unset());
        let mut conn = Held::acquire(&db).await.unwrap();
        let mut tx = conn.begin(&db, None).await.unwrap();
        assert_eq!(timeouts(&mut tx).await, unset());
    }

    /// Every unpinned transaction resolves names in `public` only (#97).
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn begin_pins_the_search_path(pool: PgPool) {
        let db = Db::from_pool(pool);
        for maintenance in [false, true] {
            let mut tx = begin_as_tenant_role(&db, maintenance).await.unwrap();
            let path: String = sqlx::query_scalar("SELECT current_setting('search_path')")
                .fetch_one(&mut *tx)
                .await
                .unwrap();
            assert_eq!(path, SEARCH_PATH, "maintenance: {maintenance}");
        }
    }

    /// No org pinned and no marker: a tenant table's policies return nothing,
    /// though the pool's superuser would see every org's rows.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn begin_sees_no_tenant_rows(pool: PgPool) {
        sqlx::query(
            "INSERT INTO flag_apps (org_id, name, client_key) VALUES ($1, 'checkout', 'sdk_x')",
        )
        .bind(uuid::Uuid::new_v4())
        .execute(&pool)
        .await
        .unwrap();
        let db = Db::from_pool(pool);
        let mut tx = begin(&db).await.unwrap();
        let seen: i64 = sqlx::query_scalar("SELECT count(*) FROM flag_apps")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(seen, 0);
    }

    /// The tables deliberately outside row-level security stay readable and
    /// writable by the tenant role, which is what the callers need.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn begin_reads_and_writes_the_pre_org_tables(pool: PgPool) {
        let (org, app) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        sqlx::query(
            "INSERT INTO flag_apps (id, org_id, name, client_key) VALUES ($1, $2, 'checkout', 'sdk_x')",
        )
        .bind(app)
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO app_keys (key_hash, org_id, app_id, kind, prefix) \
             VALUES ('\\x01', $1, $2, 'server', 'srv_')",
        )
        .bind(org)
        .bind(app)
        .execute(&pool)
        .await
        .unwrap();
        let db = Db::from_pool(pool);
        let user = uuid::Uuid::new_v4();
        let mut tx = begin(&db).await.unwrap();
        let owner: Option<uuid::Uuid> =
            sqlx::query_scalar("SELECT app_id FROM app_keys WHERE key_hash = '\\x01'")
                .fetch_optional(&mut *tx)
                .await
                .unwrap();
        assert_eq!(owner, Some(app));
        sqlx::query("INSERT INTO deleted_orgs (org_id) VALUES ($1)")
            .bind(org)
            .execute(&mut *tx)
            .await
            .unwrap();
        // Twice: the second takes `member_removed`'s ON CONFLICT … DO UPDATE.
        for _ in 0..2 {
            sqlx::query(
                "INSERT INTO removed_members (org_id, user_id) VALUES ($1, $2) \
                 ON CONFLICT (org_id, user_id) DO UPDATE SET removed_at = now()",
            )
            .bind(org)
            .bind(user)
            .execute(&mut *tx)
            .await
            .unwrap();
        }
        tx.commit().await.unwrap();
        let tombstoned: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM deleted_orgs WHERE org_id = $1)")
                .bind(org)
                .fetch_one(db.pool())
                .await
                .unwrap();
        assert!(tombstoned);
    }

    /// A role exempt from row-level security that cannot assume the tenant
    /// role would run the transaction unconstrained: refused.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn begin_refuses_an_exempt_role_that_cannot_assume_the_tenant_role(pool: PgPool) {
        let db = Db::from_pool(pool)
            .with_tenant_role("flags_no_such_role")
            .unwrap();
        let err = begin(&db).await.unwrap_err();
        assert!(matches!(err, sqlx::Error::Configuration(_)), "{err}");
        assert!(err.to_string().contains("unpinned transactions"), "{err}");
    }

    /// A pool whose sessions already run as `otto_app`: a role row-level
    /// security applies to, like a managed Postgres role with no CREATEROLE.
    /// Close `.pool()` before the test returns so sqlx can drop the database.
    pub(crate) async fn non_exempt_db(pool: &PgPool) -> Db {
        let app_pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
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

    /// Where the tenant role cannot be assumed but the pool's role is subject
    /// to row-level security, `begin` keeps that role and sets no marker.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn begin_keeps_a_non_exempt_role_that_cannot_assume_the_tenant_role(pool: PgPool) {
        let db = non_exempt_db(&pool)
            .await
            .with_tenant_role("flags_no_such_role")
            .unwrap();
        let mut tx = begin(&db).await.unwrap();
        assert_eq!(
            role_and_marker(&mut tx).await,
            ("otto_app".to_string(), String::new())
        );
        let path: String = sqlx::query_scalar("SELECT current_setting('search_path')")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(path, SEARCH_PATH);
        tx.rollback().await.unwrap();
        db.pool().close().await;
    }

    /// A tenant role that is itself exempt from row-level security: refused.
    /// The pool's own superuser role stands in for one (a role can always
    /// assume itself).
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn begin_refuses_a_tenant_role_exempt_from_rls(pool: PgPool) {
        let pool_role: String = sqlx::query_scalar("SELECT current_user::text")
            .fetch_one(&pool)
            .await
            .unwrap();
        let db = Db::from_pool(pool)
            .with_tenant_role(Box::leak(pool_role.into_boxed_str()))
            .unwrap();
        let err = begin(&db).await.unwrap_err();
        assert!(matches!(err, sqlx::Error::Configuration(_)), "{err}");
        assert!(err.to_string().contains("unpinned transactions"), "{err}");
    }
}
