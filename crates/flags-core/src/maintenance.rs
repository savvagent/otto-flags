//! Background maintenance transactions: the only way for background code to
//! read or sweep tenant tables across every org.
//!
//! The unpinned row-level policies (`rollback_policies_checker`, the
//! `*_retention_read` policies, `rollback_events_retention`, 0006's
//! `flag_versions_retention`) grant access only when no org is pinned **and**
//! the transaction carries the marker `app.maintenance = 'on'` (#27). Unpinned alone is not enough: a request
//! path that forgot to pin an org is unpinned too, and must see nothing. The
//! restrictive policies (0003's `*_maintenance_delete`, 0005's
//! `usage_outbox_maintenance` and `usage_outbox_rejected_maintenance`) leave
//! pinned access alone and require the marker only when unpinned.
//!
//! **Only background maintenance code may call [`begin`]**: the retention
//! sweeps, the rollback checker's backstop, the usage shipper, and the
//! evaluation meter's flush and hour close ([`crate::metering`]). Never a
//! request handler or a tool, which always go through a pinned transaction
//! ([`crate::platform_events::begin_live`]). It is `pub(crate)` so nothing
//! outside this crate can reach it.
//!
//! The policies bind only a role row-level security applies to, and the pool
//! may connect as one it does not (production's attach role is a superuser).
//! So [`begin`] drops into the tenant role for the transaction
//! (`crate::unpinned::begin_as_tenant_role`), as `otto_tenant::Db::begin` does
//! for pinned transactions, and refuses to run
//! as a role row-level security does not apply to (#73): the unpinned policies
//! are then what let maintenance code see across orgs.

use otto_tenant::Db;
use sqlx::{Postgres, Transaction};

/// Open an unpinned transaction on `db`'s pool, as the tenant role
/// (`db.tenant_role()`) when the pool's role can assume it, with the
/// maintenance marker set. The role rules, and the refusal to run exempt from
/// row-level security, are [`crate::unpinned::begin_as_tenant_role`]'s, shared
/// with the unpinned lookups that set no marker.
pub(crate) async fn begin(db: &Db) -> sqlx::Result<Transaction<'static, Postgres>> {
    crate::unpinned::begin_as_tenant_role(db, true).await
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
    /// transaction runs as the tenant role and carries the marker (#73).
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

    /// Where the tenant role cannot be assumed but the pool's role is subject
    /// to row-level security, `begin` keeps that role, as `Db::begin` does,
    /// instead of failing every maintenance transaction.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn begin_keeps_a_non_exempt_role_that_cannot_assume_the_tenant_role(pool: PgPool) {
        let db = crate::unpinned::tests::non_exempt_db(&pool)
            .await
            .with_tenant_role("flags_no_such_role")
            .unwrap();
        let mut tx = begin(&db).await.unwrap();
        assert_eq!(
            role_and_marker(&mut tx).await,
            ("otto_app".to_string(), "on".to_string())
        );
        tx.rollback().await.unwrap();
        db.pool().close().await;
    }

    /// A role exempt from row-level security that cannot assume the tenant
    /// role would run the maintenance transaction unconstrained: refused.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn begin_refuses_an_exempt_role_that_cannot_assume_the_tenant_role(pool: PgPool) {
        let db = Db::from_pool(pool)
            .with_tenant_role("flags_no_such_role")
            .unwrap();
        let err = begin(&db).await.unwrap_err();
        assert!(matches!(err, sqlx::Error::Configuration(_)), "{err}");
        assert!(
            err.to_string().contains("maintenance transactions"),
            "{err}"
        );
    }

    /// A tenant role that is itself exempt from row-level security would let
    /// the maintenance transaction run unconstrained after the switch:
    /// refused. The pool's own superuser role stands in for one here (a role
    /// can always assume itself).
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
        assert!(
            err.to_string().contains("maintenance transactions"),
            "{err}"
        );
    }

    /// The role, the marker, and the search path end with the transaction,
    /// committed or rolled back, so the next user of the pooled connection
    /// gets none of them.
    #[sqlx::test(migrator = "crate::MIGRATOR")]
    async fn the_role_marker_and_search_path_end_with_the_transaction(pool: PgPool) {
        let one = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect_with(pool.connect_options().as_ref().clone())
            .await
            .unwrap();
        let pool_role: String = sqlx::query_scalar("SELECT current_user::text")
            .fetch_one(&one)
            .await
            .unwrap();
        let pool_path: String = sqlx::query_scalar("SELECT current_setting('search_path')")
            .fetch_one(&one)
            .await
            .unwrap();
        let db = Db::from_pool(one);
        for commit in [true, false] {
            let tx = begin(&db).await.unwrap();
            if commit {
                tx.commit().await.unwrap();
            } else {
                tx.rollback().await.unwrap();
            }
            let after: (String, String, String) = sqlx::query_as(
                "SELECT current_user::text, coalesce(current_setting('app.maintenance', true), ''), \
                        current_setting('search_path')",
            )
            .fetch_one(db.pool())
            .await
            .unwrap();
            assert_eq!(
                after,
                (pool_role.clone(), String::new(), pool_path.clone()),
                "commit: {commit}"
            );
        }
        db.pool().close().await;
    }
}
