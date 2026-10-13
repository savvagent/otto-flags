//! otto-flags' own migrations, in `crates/flags-core/migrations`.
//!
//! **Never call `otto_tenant::Db::migrate` on this database.** It applies
//! otto-platform's migration history, whose version numbers collide with this
//! one's. The platform's schema lives in the platform's database.

use otto_tenant::Db;
use sqlx::migrate::Migrator;

/// Every migration in `./migrations`, embedded at compile time. Tests use it
/// as `#[sqlx::test(migrator = "flags_core::MIGRATOR")]`.
///
/// `ignore_missing` lets a release boot against a database that a newer
/// release has already migrated: Fly's automatic rollback after a failed
/// `/readyz`, an old machine restarting mid-rollout, or a revert deploy all
/// run the previous image against the newer schema. sqlx's default would
/// refuse with `VersionMissing` and crash-loop every machine. Only unknown
/// versions are ignored: a known migration whose applied checksum differs
/// still fails with `VersionMismatch`, and a failed one with `Dirty`.
///
/// The struct-update form is needed because the only setter takes
/// `&mut self` and this must be a `static`; the fields are public (and
/// semver-exempt) so `migrate!` can build one. `Cargo.lock` pins sqlx, so a
/// change there fails to compile rather than silently.
pub static MIGRATOR: Migrator = Migrator {
    ignore_missing: true,
    ..sqlx::migrate!("./migrations")
};

/// Apply every migration. Safe from several replicas at once: sqlx holds an
/// advisory lock for the duration.
pub async fn migrate(db: &Db) -> crate::Result<()> {
    MIGRATOR
        .run(db.pool())
        .await
        .map_err(|e| sqlx::Error::Migrate(Box::new(e)))?;
    Ok(())
}
