//! otto-flags' own migrations, in `crates/flags-core/migrations`.
//!
//! **Never call `otto_tenant::Db::migrate` on this database.** It applies
//! otto-platform's migration history, whose version numbers collide with this
//! one's. The platform's schema lives in the platform's database.

use otto_tenant::Db;

/// For tests: `#[sqlx::test(migrator = "flags_core::MIGRATOR")]`.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Apply every migration. Safe from several replicas at once: sqlx holds an
/// advisory lock for the duration.
pub async fn migrate(db: &Db) -> crate::Result<()> {
    MIGRATOR
        .run(db.pool())
        .await
        .map_err(|e| sqlx::Error::Migrate(Box::new(e)))?;
    Ok(())
}
