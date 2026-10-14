//! Background maintenance transactions: the only way to read or sweep tenant
//! tables across every org.
//!
//! The unpinned row-level policies (`rollback_policies_checker`, the
//! `*_retention_read` policies, `rollback_events_retention`,
//! `usage_outbox_maintenance`, `usage_outbox_rejected_maintenance`) grant
//! access only when no org is pinned **and** the transaction carries the
//! marker `app.maintenance = 'on'` (#27). Unpinned alone is not enough: a
//! request path that forgot to pin an org is unpinned too, and must see
//! nothing.
//!
//! **Only background maintenance code may call [`begin`]**: the retention
//! sweeps, the rollback checker's backstop, and the usage shipper. Never a
//! request handler or a tool, which always go through a pinned transaction
//! ([`crate::platform_events::begin_live`]). It is `pub(crate)` so nothing
//! outside this crate can reach it.

use otto_tenant::Db;
use sqlx::{Postgres, Transaction};

/// Open an unpinned transaction on `db`'s pool with the maintenance marker
/// set for its duration (`SET LOCAL`, so it ends with the transaction and
/// never leaks to the next user of the connection).
pub(crate) async fn begin(db: &Db) -> sqlx::Result<Transaction<'static, Postgres>> {
    let mut tx = db.pool().begin().await?;
    sqlx::query("SET LOCAL app.maintenance = 'on'")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}
