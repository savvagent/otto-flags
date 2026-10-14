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
/// still fails with `VersionMismatch`, and a migration recorded as failed,
/// known or not, with `Dirty`. sqlx never records a failed migration itself
/// on Postgres: a migration and its row commit in one transaction, so a
/// failure leaves neither and the previous release still boots. Recovery is
/// in `docs/deploy/fly.md`, "When a migration fails". Each migration must
/// stay readable and writable by the previous release (the rollback
/// invariant in `CLAUDE.md`).
///
/// The cost: sqlx no longer notices an applied migration whose file was
/// deleted or renamed. CI's `migrations-append-only` job catches any delete
/// or rename; `versions_are_contiguous_from_one` below also catches a gap or
/// a duplicate number.
///
/// The struct-update form is needed because the only setter takes
/// `&mut self` and this must be a `static`; the fields are public (and
/// semver-exempt) so `migrate!` can build one. `Cargo.lock` pins sqlx;
/// removing or renaming the field fails to compile, and
/// `a_release_boots_against_a_database_a_newer_release_migrated` in
/// `tests/db.rs` pins the behavior.
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

#[cfg(test)]
mod tests {
    use super::MIGRATOR;

    /// sqlx ignores applied versions it does not embed, so it no longer
    /// notices a deleted or renumbered migration. Versions must run 1..=N
    /// with no gap: a missing number means a migration file went away.
    #[test]
    fn versions_are_contiguous_from_one() {
        let versions: Vec<i64> = MIGRATOR
            .iter()
            .filter(|m| !m.migration_type.is_down_migration())
            .map(|m| m.version)
            .collect();
        let expected: Vec<i64> = (1..).take(versions.len()).collect();
        assert!(!versions.is_empty(), "no migrations embedded");
        assert_eq!(versions, expected, "migration versions must be 1..=N");
    }
}
