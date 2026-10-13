# Rollback-safe migrator: tolerate migrations a release does not know

**Status:** DRAFT
**Issue:** #26

## 1. Brief

Make the flags-core migrator tolerate migrations it does not know (`ignore_missing: true`), so
that a release can boot against a database that a newer release has already migrated. Today
`crates/flags-core/src/migrate.rs` builds `MIGRATOR` with sqlx's default `ignore_missing = false`,
so after any new migration is applied, rolling back to the previous image (Fly's automatic
rollback after a failed `/readyz`, an old machine restarting mid-rollout, or a revert deploy)
crash-loops every machine with `VersionMissing`. This must merge and deploy before PR #24
(auto-rollback), which adds the first migrations after the baseline (0002, 0003).

Acceptance criteria (#26, verbatim in intent):

- `MIGRATOR` is built with `ignore_missing: true`, checked to compile with the pinned sqlx 0.8 and
  to still embed the migrations, with a doc comment saying why (rollback safety; checksums of
  known migrations are still verified).
- A test in `crates/flags-core/tests/db.rs` applies the migrations, inserts a fake future row into
  `_sqlx_migrations`, runs `flags_core::MIGRATOR.run(&pool)` again, and asserts it succeeds.
- A test proves a tampered checksum of a known migration still fails.
- `CLAUDE.md`'s Development Workflow section stays accurate (Ship > verify rollback sentence,
  Out-of-band migrations bullet, new Invariant bullet). Minimal edits.
- `docs/deploy/fly.md` and PR #24's files are not edited.

## 2. Premise corrections

- The brief's premise holds against sqlx-core 0.8.6 (`Cargo.lock`):
  `validate_applied_migrations` returns `VersionMissing(v)` for any applied version the migrator
  does not embed, unless `ignore_missing` is set, in which case it skips that check entirely.
  The checksum comparison (`VersionMismatch`) and the dirty check (`Dirty`) in `run_direct` are
  separate and unaffected by `ignore_missing`.
- Nothing is broken on `main` today: only `0001_baseline.sql` exists, so no deployed release can
  meet an unknown version yet.

## 3. Assumptions

- **Construction form.** `Migrator`'s fields are `#[doc(hidden)] pub` and documented by sqlx as
  semver-exempt; they are public precisely so `migrate!()` can build a const. The only public
  setter, `set_ignore_missing(&mut self)`, cannot build a `static`, and `MIGRATOR` must stay a
  `static` path for `#[sqlx::test(migrator = "flags_core::MIGRATOR")]`. So the static uses the
  struct-update form `Migrator { ignore_missing: true, ..sqlx::migrate!("./migrations") }`.
  `Cargo.lock` pins sqlx 0.8.6, so a field change in a later sqlx shows up as a compile error on
  the bump, not silently. The doc comment says so.
- **No new config key.** Tolerance is unconditional; there is no environment switch for it.
- Every Run Profile value was read from the Development Workflow section; none assumed.
- The new Invariant bullet's wording comes from the brief; it sits beside the existing
  append-only bullet, whose "compatible with the previous release during rollout" clause stays.

## 4. Goal and success criteria

A release boots against a database whose `_sqlx_migrations` holds versions it does not embed,
while still refusing a database whose copy of a migration it knows has a different checksum.

- `flags_core::MIGRATOR.ignore_missing` is `true`, and it still embeds `0001_baseline`.
- `MIGRATOR.run` succeeds against a migrated database with an extra, unknown, higher version row.
- `MIGRATOR.run` fails with `MigrateError::VersionMismatch(1)` when version 1's stored checksum is
  altered.
- `CLAUDE.md` states the rollback guarantee and the new invariant.

## 5. Scope

In: `crates/flags-core/src/migrate.rs`, two tests in `crates/flags-core/tests/db.rs`, three
`CLAUDE.md` edits, this spec and its plan.

Out: new migrations or schema; `docs/deploy/fly.md` (PR #24 owns its recovery text); any change
to `flags-server` boot (`FLAGS_RUN_MIGRATIONS` and the boot isolation proof are unchanged);
`undo` or down migrations.

## 6. Architecture

- `crates/flags-core/src/migrate.rs:10` — `MIGRATOR` gains `ignore_missing: true` via struct
  update over `sqlx::migrate!("./migrations")`. `migrate()` (line 14) is unchanged and still the
  boot entry point (`crates/flags-server/src/main.rs:40`).
- Public interface: `flags_core::MIGRATOR` keeps its name and type; only a field value changes.
  Additive (behavioral relaxation of one boot check). No wire, MCP, REST, or config change.

Behavior after the change, on boot of release R against a database last migrated by R' > R:
applied versions R does not know are skipped; every version R embeds must be applied with an
identical checksum (else `VersionMismatch`, boot fails, as today); a failed (`success = false`)
row of any version still yields `Dirty` and fails boot, as today.

## 7. Invariants impact

- **Append-only migrations:** strengthened in meaning. Checksums of known migrations are still
  verified, so editing an applied migration still refuses to boot. A new bullet records the
  consequence: migrations must stay compatible with the previous release after a rollback too,
  because that release now runs against the newer schema rather than refusing to start.
- **Tenant isolation is RLS under `otto_app`, proven:** unaffected. The boot isolation proof runs
  after migration as before; a rolled-back release proves isolation against the newer schema, and
  `tenant_isolation_is_enforced_on_every_tenant_table` is unchanged.
- No other invariant is touched (no SQL outside flags-core: the new tests' SQL lives in
  `crates/flags-core/tests/db.rs`, which is inside flags-core).

## 8. Migration

None. No schema change, no new file under `crates/flags-core/migrations/`.

## 9. Out-of-band

None. No secret, config key, scope, or platform registration. Deploy is `ci-auto` on merge;
migrations apply at boot as today (vacuous: no new migration).

## 10. Error handling and edge cases

- Unknown version lower than a known one (out-of-order history): also ignored; sqlx would apply
  the known one if missing. Not a case this repo produces (versions are monotonic `NNNN`).
- A newer release's failed migration: Postgres runs each migration in a transaction, so a failure
  normally leaves no row; a `success = false` row would still block boot via `Dirty`. Unchanged.
- The rolled-back release running against a newer schema is the remaining risk; the new invariant
  puts it on the author of each migration (additive, nullable or defaulted columns, no drops or
  renames the previous release reads).

## 11. Testing approach

Both tests live in `crates/flags-core/tests/db.rs` with
`#[sqlx::test(migrator = "flags_core::MIGRATOR")]` (fresh migrated database each):

1. `a_release_boots_against_a_database_a_newer_release_migrated`: insert a `_sqlx_migrations` row
   with a version above every embedded one (`max + 1`, computed from `MIGRATOR.iter()`), any
   checksum, `success = true`; assert `flags_core::MIGRATOR.run(&pool)` and
   `flags_core::migrate(&db)` both return `Ok`. Fails today with `VersionMissing`.
2. `a_tampered_known_migration_still_refuses_to_boot`: update version 1's checksum; assert
   `MIGRATOR.run(&pool)` returns `Err(MigrateError::VersionMismatch(1))`.

Gates: the section's test (with `DATABASE_URL`, serialized across worktrees per the section's
Worktree notes), lint, format,
the migrations `extra` check, and the `podman build` gate (changed paths under `crates/**`).

## 12. Risks and open questions

- sqlx's hidden fields could change in a future minor; mitigated by the lockfile pin and a
  compile error on bump.
- A future migration that is not backward compatible would now let a rolled-back release boot
  into a schema it misreads, instead of crash-looping. The new invariant is the mitigation;
  reviewers check it per migration.
