//! Unpinned transactions that still run under row-level security.
//!
//! Some queries cannot pin an org because the org is what they are looking
//! for, or what they are about to tombstone: SDK key resolution
//! ([`crate::keys::resolve_hash`]), the revocation check
//! ([`crate::platform_events::revoked`]), and the lifecycle tombstones. They
//! touch only the tables deliberately left outside row-level security
//! (`app_keys`, `deleted_orgs`, `removed_members`).
//!
//! They still go through [`begin`], never a bare query on the pool: the pool
//! may connect as a role row-level security does not apply to (production's
//! attach role is a superuser), and a later query on that connection that
//! named a tenant table by mistake would read every org's rows (#80). Inside
//! [`begin`] the transaction runs as the tenant role with no org pinned and no
//! maintenance marker, so every tenant table's policies return nothing.
//!
//! `tests/db.rs`' `no_raw_pool_queries_outside_the_allowed_sites` refuses a new
//! raw pool query anywhere else.

use otto_tenant::Db;
use sqlx::{Postgres, Transaction};

/// Open an unpinned transaction on `db`'s pool as the tenant role, with no
/// maintenance marker. See [`begin_as_tenant_role`] for the role rules.
pub(crate) async fn begin(db: &Db) -> sqlx::Result<Transaction<'static, Postgres>> {
    begin_as_tenant_role(db, false).await
}

/// Open an unpinned transaction on `db`'s pool, as the tenant role
/// (`db.tenant_role()`) when the pool's role can assume it, setting the
/// maintenance marker `app.maintenance = 'on'` only when `maintenance` is
/// true. Only [`crate::maintenance::begin`] passes true; everything else
/// calls [`begin`]. Role and marker are both transaction-local
/// (`set_config(…, true)`, which for `role` is `SET LOCAL ROLE`), so they end
/// with the transaction and never leak to the next user of the connection.
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
    let mut tx = db.pool().begin().await?;
    // One round trip for the probes, the marker, and the role switch (these
    // run on every SDK key lookup). The probes are a MATERIALIZED CTE so they
    // read `current_user` before the outer select list can switch the role.
    // Each `set_config` sits in its own select-list column inside a CASE: a
    // volatile function in a CASE arm runs only when that arm is taken, and
    // a column of its own is never skipped the way one operand of a boolean
    // expression can be. Exemption is a role attribute, never inherited
    // through membership, so each role is checked by name. If the result is
    // refused below, the switch is rolled back with the transaction.
    let (assumable, pool_exempt, tenant_exempt, _, _): (
        bool,
        bool,
        bool,
        Option<String>,
        Option<String>,
    ) = sqlx::query_as(
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
                CASE WHEN assumable THEN set_config('role', $1::text, true) END \
         FROM probe",
    )
    .bind(db.tenant_role())
    .bind(maintenance)
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
    async fn role_and_marker(tx: &mut Transaction<'static, Postgres>) -> (String, String) {
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
