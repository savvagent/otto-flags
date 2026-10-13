# Rollback-safe migrator Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** `flags_core::MIGRATOR` tolerates applied migrations it does not embed, so a rolled-back
release boots against a database a newer release migrated, while known-migration checksums are
still verified.

**Architecture:** Struct-update `ignore_missing: true` over `sqlx::migrate!("./migrations")` in the
existing `static`; two DB tests prove tolerance and integrity; `CLAUDE.md` records the guarantee
and the invariant it creates.

**Tech Stack:** Rust (stable, MSRV 1.88), sqlx 0.8.6, Postgres 16.

**Ticket:** savvagent/otto-flags#26

**Spec:** `docs/specs/2026-10-13-migrator-ignore-missing-design.md`

## Global Constraints

- Worktree only (`/home/robhicks/dev/otto-flags/.claude/worktrees/fix-26-migrator-ignore-missing`);
  `main` changes only through the PR.
- Gates (from `CLAUDE.md` > Gates; every DB test run serialized across worktrees with the run's
  lock):
  - test: `flock /tmp/claude-1000/-home-robhicks-dev-otto-flags/1c1ca6c4-ca67-4c3f-8a81-2816918f1768/scratchpad/dbtest.lock env DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/'` prints nothing
  - when `crates/**`: `podman build -t otto-flags:check .`
- Evidence: pass counts per test binary; a `DATABASE_URL must be set` panic is a missing variable,
  not a pass. `DATABASE_URL` only ever points at the local Postgres on 15434.
- Invariants that apply: append-only migrations (no file under `crates/flags-core/migrations/`
  changes); no SQL outside flags-core (the tests' SQL lives in `crates/flags-core/tests/db.rs`);
  tenant isolation proof unchanged.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #26` (or `Refs #26` on docs-only intermediate commits). No attribution of any kind (no
  Co-Authored-By, no "Generated with", no credit markers).
- Do not edit `docs/deploy/fly.md` or any file PR #24 touches beyond `CLAUDE.md`'s three lines.

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `crates/flags-core/src/migrate.rs` | `MIGRATOR` with `ignore_missing: true` and its why-doc |
| **Modify.** `crates/flags-core/tests/db.rs` | Two tests: unknown newer version tolerated; tampered known checksum refused |
| **Modify.** `CLAUDE.md` | Ship > verify and Out-of-band clauses; new Invariant bullet |

## Task Order & Rationale

Task 1 is the code change, test-first. Task 2 is the docs that describe the new guarantee, and
only makes sense once Task 1 holds.

---

### Task 1: tolerate unknown migrations, keep checksum checks

Model: sonnet

- [ ] **Write the failing tests** in `crates/flags-core/tests/db.rs` (after the existing
  `tenant_isolation_is_enforced_on_every_tenant_table` test). Add
  `use sqlx::migrate::MigrateError;` to the imports.

  ```rust
  /// A rolled-back release must boot against a database that a newer
  /// release already migrated: the applied version it does not know is
  /// ignored (`ignore_missing`), see `flags_core::migrate`.
  #[sqlx::test(migrator = "flags_core::MIGRATOR")]
  async fn a_release_boots_against_a_database_a_newer_release_migrated(pool: PgPool) {
      let newest = flags_core::MIGRATOR
          .iter()
          .map(|m| m.version)
          .max()
          .expect("migrations are embedded");
      sqlx::query(
          "INSERT INTO _sqlx_migrations \
           (version, description, success, checksum, execution_time) \
           VALUES ($1, 'from a newer release', true, '\\xdeadbeef'::bytea, 0)",
      )
      .bind(newest + 1)
      .execute(&pool)
      .await
      .unwrap();

      flags_core::MIGRATOR.run(&pool).await.unwrap();
      flags_core::migrate(&db(pool).await).await.unwrap();
  }

  /// Ignoring unknown versions must not disable integrity: a known
  /// migration whose applied checksum differs still refuses to boot.
  #[sqlx::test(migrator = "flags_core::MIGRATOR")]
  async fn a_tampered_known_migration_still_refuses_to_boot(pool: PgPool) {
      sqlx::query("UPDATE _sqlx_migrations SET checksum = '\\xdeadbeef'::bytea WHERE version = 1")
          .execute(&pool)
          .await
          .unwrap();

      let err = flags_core::MIGRATOR.run(&pool).await.unwrap_err();
      assert!(
          matches!(err, MigrateError::VersionMismatch(1)),
          "expected VersionMismatch(1), got {err:?}"
      );
  }
  ```

  (The `\\x` in a normal Rust string literal yields `\x` in SQL, the bytea hex form; a raw string
  `r"..."` with a single backslash is equivalent. Check `rows_affected() == 1` on the UPDATE if it
  helps make the test self-proving.)

- [ ] **Run, see the first fail** with `VersionMissing` and the second pass:
  `flock /tmp/claude-1000/-home-robhicks-dev-otto-flags/1c1ca6c4-ca67-4c3f-8a81-2816918f1768/scratchpad/dbtest.lock env DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --test db -- a_release_boots a_tampered_known`

- [ ] **Implement** in `crates/flags-core/src/migrate.rs`, replacing the `MIGRATOR` line:

  ```rust
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
  ```

  with `use sqlx::migrate::Migrator;`. If the struct-update form does not compile in a `static`,
  use a const block (`{ let mut m = sqlx::migrate!("./migrations"); m.ignore_missing = true; m }`)
  and record it.

- [ ] **Run, see both pass** (same command), then the full gates: test, lint, format, extra (all
  in Global Constraints).

- [ ] **Commit:**
  ```
  fix(core): let a rolled-back release boot against newer migrations

  <why: VersionMissing crash-loops every machine after a rollback once a
  migration past the baseline is applied; checksums of known migrations
  are still verified, proven by a test.>

  Fixes #26
  ```

### Task 2: record the guarantee in CLAUDE.md

Model: haiku

- [ ] `CLAUDE.md` Ship > verify: after "A machine failing `/readyz` rolls back: merged, not live."
  append: "The rolled-back release boots against the newer schema because `MIGRATOR` ignores
  migration versions it does not know (`crates/flags-core/src/migrate.rs`)."
- [ ] `CLAUDE.md` Out-of-band > Migrations: after "the deploy rolls back." append: "The previous
  release then boots against the migrated database: `MIGRATOR` ignores unknown versions
  (`crates/flags-core/src/migrate.rs`)."
- [ ] `CLAUDE.md` Invariants: a new bullet directly after **Append-only migrations**:
  "- **Migrations must be compatible with the previous release during rollout AND after a
  rollback:** the previous release runs against the newer schema (`ignore_missing`)."
- [ ] Gates: format and the migrations extra check (docs-only; no code changed).
- [ ] **Commit:** `docs: say a rolled-back release tolerates newer migrations` with a why-body,
  ending `Refs #26`. No attribution.
