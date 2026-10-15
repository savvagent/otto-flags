//! Background maintenance transactions: the only way to read or sweep tenant
//! tables across every org.
//!
//! The unpinned row-level policies (`rollback_policies_checker`, the
//! `*_retention_read` policies, `rollback_events_retention`) grant access only
//! when no org is pinned **and** the transaction carries the marker
//! `app.maintenance = 'on'` (#27). Unpinned alone is not enough: a request
//! path that forgot to pin an org is unpinned too, and must see nothing. The
//! restrictive policies (0003's `*_maintenance_delete`, 0005's
//! `usage_outbox_maintenance` and `usage_outbox_rejected_maintenance`) leave
//! pinned access alone and require the marker only when unpinned.
//!
//! **Only background maintenance code may call [`begin`]**: the retention
//! sweeps, the rollback checker's backstop, and the usage shipper. Never a
//! request handler or a tool, which always go through a pinned transaction
//! ([`crate::platform_events::begin_live`]). It is `pub(crate)` so nothing
//! outside this crate can reach it.
//!
//! The policies bind only a role row-level security applies to, and the pool
//! may connect as one it does not (production's attach role is a superuser).
//! So [`begin`] drops into the tenant role with `SET LOCAL ROLE`, as
//! `otto_tenant::Db::begin` does for pinned transactions (#73): the unpinned
//! policies are then what let maintenance code see across orgs.

use otto_tenant::Db;
use sqlx::{Postgres, Transaction};

/// Open an unpinned transaction on `db`'s pool, as the tenant role
/// (`db.tenant_role()`) when the pool's role can assume it, with the
/// maintenance marker set. Both are `SET LOCAL`, so they end with the
/// transaction and never leak to the next user of the connection.
///
/// Whether the role can be assumed is the question `otto_tenant::Db::begin`
/// asks before its own `SET LOCAL ROLE`, with the same catalog query: its
/// answer is private to otto-tenant, so it is asked again here, once per
/// transaction. Where it cannot be assumed the role is left alone, as
/// `Db::begin` leaves it; the server only starts when
/// `Db::verify_tenant_isolation` has shown the pool's own role is then subject
/// to row-level security.
pub(crate) async fn begin(db: &Db) -> sqlx::Result<Transaction<'static, Postgres>> {
    let mut tx = db.pool().begin().await?;
    // Two columns, not one `AND`: Postgres may skip either side of an `AND`,
    // and the marker must be set either way.
    let (_, assumable): (String, bool) = sqlx::query_as(
        "SELECT set_config('app.maintenance', 'on', true), EXISTS ( \
           SELECT 1 FROM pg_catalog.pg_roles r \
           WHERE r.rolname = $1 \
             AND pg_catalog.pg_has_role(current_user, r.oid, 'USAGE'))",
    )
    .bind(db.tenant_role())
    .fetch_one(&mut *tx)
    .await?;
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
    /// does, which row-level security does not apply to. A maintenance
    /// transaction must drop into the tenant role so the marker policies are
    /// what let it through (#73).
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn begin_runs_as_the_tenant_role_with_the_marker(pool: PgPool) {
        let db = Db::from_pool(pool);
        let mut tx = begin(&db).await.unwrap();
        assert_eq!(
            role_and_marker(&mut tx).await,
            (db.tenant_role().to_string(), "on".to_string())
        );
        // Refused what the tenant role is refused: it has no DELETE on the
        // append-only audit trail, which the pool's superuser would have.
        let err = sqlx::query("DELETE FROM audit_events")
            .execute(&mut *tx)
            .await
            .unwrap_err();
        let code = err
            .as_database_error()
            .and_then(|e| e.code())
            .map(|c| c.into_owned());
        assert_eq!(code.as_deref(), Some("42501"), "{err}");
    }

    /// Where the tenant role cannot be assumed, `begin` skips the role change,
    /// as `Db::begin` does, instead of failing every maintenance transaction.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn begin_keeps_the_pool_role_when_the_tenant_role_is_not_assumable(pool: PgPool) {
        let pool_role: String = sqlx::query_scalar("SELECT current_user::text")
            .fetch_one(&pool)
            .await
            .unwrap();
        let db = Db::from_pool(pool)
            .with_tenant_role("flags_no_such_role")
            .unwrap();
        let mut tx = begin(&db).await.unwrap();
        assert_eq!(
            role_and_marker(&mut tx).await,
            (pool_role, "on".to_string())
        );
    }
}
