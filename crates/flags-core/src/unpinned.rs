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
/// maintenance marker. See [`begin_with_marker`] for the role rules.
pub(crate) async fn begin(db: &Db) -> sqlx::Result<Transaction<'static, Postgres>> {
    begin_with_marker(db, false).await
}

/// Open an unpinned transaction on `db`'s pool, as the tenant role
/// (`db.tenant_role()`) when the pool's role can assume it, setting the
/// maintenance marker `app.maintenance = 'on'` only when `maintenance` is
/// true ([`crate::maintenance::begin`]). Role and marker are both
/// transaction-local (`SET LOCAL ROLE`, `set_config(…, true)`), so they end
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
pub(crate) async fn begin_with_marker(
    db: &Db,
    maintenance: bool,
) -> sqlx::Result<Transaction<'static, Postgres>> {
    let mut tx = db.pool().begin().await?;
    // One round trip for the marker and the probes. `set_config` runs as its
    // own select-list column, so the marker is set whatever the probes return;
    // inside one boolean expression Postgres could skip it. A volatile
    // function in a CASE arm runs only when that arm is taken, so without
    // `maintenance` no marker is set. Exemption is a role attribute, never
    // inherited through membership, so each role is checked by name.
    let (_, assumable, pool_exempt, tenant_exempt): (Option<String>, bool, bool, bool) =
        sqlx::query_as(
            "SELECT CASE WHEN $2 THEN set_config('app.maintenance', 'on', true) END, \
                    EXISTS ( \
                      SELECT 1 FROM pg_catalog.pg_roles r \
                      WHERE r.rolname = $1 \
                        AND pg_catalog.pg_has_role(current_user, r.oid, 'USAGE')), \
                    EXISTS ( \
                      SELECT 1 FROM pg_catalog.pg_roles r \
                      WHERE r.rolname = current_user AND (r.rolsuper OR r.rolbypassrls)), \
                    EXISTS ( \
                      SELECT 1 FROM pg_catalog.pg_roles r \
                      WHERE r.rolname = $1 AND (r.rolsuper OR r.rolbypassrls))",
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
    if assumable {
        // `tenant_role` is validated by otto-tenant (`^[a-z_][a-z0-9_]*$`)
        // before it can be set; `SET ROLE` takes no bind parameter.
        sqlx::query(&format!("SET LOCAL ROLE {}", db.tenant_role()))
            .execute(&mut *tx)
            .await?;
    }
    Ok(tx)
}

#[cfg(test)]
mod tests {
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
        let db = Db::from_pool(pool);
        let org = uuid::Uuid::new_v4();
        let mut tx = begin(&db).await.unwrap();
        sqlx::query("INSERT INTO deleted_orgs (org_id) VALUES ($1)")
            .bind(org)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query("INSERT INTO removed_members (org_id, user_id) VALUES ($1, $2)")
            .bind(org)
            .bind(uuid::Uuid::new_v4())
            .execute(&mut *tx)
            .await
            .unwrap();
        let keys: i64 = sqlx::query_scalar("SELECT count(*) FROM app_keys")
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(keys, 0);
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
}
