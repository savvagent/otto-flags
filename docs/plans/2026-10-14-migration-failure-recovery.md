# Migration Failure Recovery Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** `docs/deploy/fly.md` tells an operator how to recover from a migration that failed in
production, and tests in `flags-core` pin every behavior and statement the runbook relies on.

**Architecture:** No runtime change. Task 1 adds database tests proving what a failed migration
leaves behind (nothing), that the previous release still boots, that the runbook's two
human-only statements work, and that `sha384sum` gives sqlx's checksum. Task 2 writes the runbook
section and points `CLAUDE.md` and the `MIGRATOR` doc comment at it.

**Tech Stack:** Rust (stable, MSRV 1.88), sqlx 0.8.6, Postgres 16.

**Ticket:** savvagent/otto-flags#34

**Spec:** `docs/specs/2026-10-14-migration-failure-recovery-design.md`

## Global Constraints

- Worktree only
  (`/home/robhicks/dev/otto-flags/.claude/worktrees/docs-34-migration-failure-recovery`);
  `main` changes only through the PR.
- Gates (from `CLAUDE.md` > Gates), from the worktree root:
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'` prints nothing
  - when `crates/**`: `podman build -t otto-flags:check .`
- Evidence: pass counts per test binary; a `DATABASE_URL must be set` panic is a missing variable,
  not a pass. `DATABASE_URL` only ever points at the local Postgres on 15434. Only one DB test run
  at a time against that Postgres.
- Invariants that apply: append-only migrations (no file under `crates/flags-core/migrations`
  is added, edited, renamed, or deleted); no SQL outside flags-core (the new SQL is in
  `crates/flags-core/tests/db.rs`), runtime `sqlx::query` only; human-only production writes are
  documented as hand-offs to Rob, never performed. Nothing is run against production.
- Out-of-band: none.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #34`. No attribution of any kind (no Co-Authored-By, no "Generated with", no credit
  markers).

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `crates/flags-core/tests/db.rs` | two new tests; the existing `Dirty` test runs the runbook's `DELETE` |
| **Modify.** `crates/flags-core/src/migrate.rs` | `MIGRATOR` doc comment: sqlx never writes a failed row; pointer to the runbook |
| **Modify.** `docs/deploy/fly.md` | new `## When a migration fails` section after `## Deploys` |
| **Modify.** `CLAUDE.md` | one pointer sentence on the Out-of-band > Migrations line |

## Task Order & Rationale

Task 1 first: the tests establish the facts the runbook states (they pass against today's code,
since they pin existing sqlx behavior rather than new code; the "failing test first" step is
replaced by checking each assertion fails when the asserted fact is inverted). Task 2 writes the
runbook from those facts.

---

### Task 1: Tests that pin what a failed migration leaves behind

**Files:**
- Modify: `crates/flags-core/tests/db.rs` (imports at the top; the migration tests after
  `a_release_boots_against_a_database_a_newer_release_migrated`, around line 327-385)

**Interfaces:** produces the test names Task 2's runbook cites:
`a_failed_migration_leaves_nothing_and_the_previous_release_boots`,
`migration_checksums_are_the_sha384_of_their_files`, `a_failed_unknown_migration_still_refuses_to_boot`.

**Out-of-band / invariants:** none; no migration file touched.

**Model:** sonnet — test code fully specified below.

- [ ] **Step 1: Imports.** Change `use sqlx::migrate::MigrateError;` to
  `use sqlx::migrate::{MigrateError, Migration, MigrationType, Migrator};` and add
  `use std::borrow::Cow;`.

- [ ] **Step 2: Add the failed-migration test** after
  `a_release_boots_against_a_database_a_newer_release_migrated`:

```rust
/// What a migration that fails in production leaves behind, which the
/// recovery runbook in `docs/deploy/fly.md` ("When a migration fails")
/// relies on (#34). sqlx runs each migration and its `_sqlx_migrations` row
/// in one transaction, so a failure leaves neither: no `success = false`
/// row, no `Dirty`, and the previous release boots as if the newer one had
/// never deployed. The newer release retries from scratch on every boot.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_failed_migration_leaves_nothing_and_the_previous_release_boots(pool: PgPool) {
    let newest = flags_core::MIGRATOR
        .iter()
        .map(|m| m.version)
        .max()
        .expect("migrations are embedded");
    let failing = newest + 1;
    // Fails on data, like a constraint production rows violate: the table
    // and its row are created first, so a partial apply would be visible.
    let mut migrations: Vec<Migration> = flags_core::MIGRATOR.iter().cloned().collect();
    migrations.push(Migration::new(
        failing,
        Cow::Borrowed("fails on existing data"),
        MigrationType::Simple,
        Cow::Borrowed(
            "CREATE TABLE half_applied (n int); \
             INSERT INTO half_applied VALUES (1); \
             ALTER TABLE half_applied ADD CONSTRAINT negative CHECK (n < 0);",
        ),
        false,
    ));
    let newer_release = Migrator {
        migrations: Cow::Owned(migrations),
        ignore_missing: true,
        ..Migrator::DEFAULT
    };

    let assert_fails = |err: MigrateError| match err {
        MigrateError::ExecuteMigration(e, v) => {
            assert_eq!(v, failing);
            assert_eq!(sqlstate(&e), "23514", "{e}");
        }
        other => panic!("expected ExecuteMigration({failing}), got {other:?}"),
    };
    assert_fails(newer_release.run(&pool).await.unwrap_err());

    // Nothing recorded, nothing half applied.
    let rows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM _sqlx_migrations WHERE version = $1 OR NOT success",
    )
    .bind(failing)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rows, 0, "a failed migration must leave no row");
    let table: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('half_applied')::text")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(table, None, "a failed migration must leave none of its changes");

    // The previous release boots.
    flags_core::MIGRATOR.run(&pool).await.unwrap();
    flags_core::migrate(&db(pool.clone()).await).await.unwrap();

    // The newer release retries from scratch and fails the same way, not
    // with `Dirty`.
    assert_fails(newer_release.run(&pool).await.unwrap_err());

    // The runbook's human-only "record as applied" statement, with the
    // checksum as `sha384sum` prints it: the newer release then boots
    // without running the migration.
    let checksum_hex: String = newer_release
        .iter()
        .find(|m| m.version == failing)
        .unwrap()
        .checksum
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    sqlx::query(
        "INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time) \
         VALUES ($1, 'fails on existing data', true, decode($2, 'hex'), -1)",
    )
    .bind(failing)
    .bind(&checksum_hex)
    .execute(&pool)
    .await
    .unwrap();
    newer_release.run(&pool).await.unwrap();
    let table: Option<String> =
        sqlx::query_scalar("SELECT to_regclass('half_applied')::text")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(table, None, "a migration recorded as applied is not run");
}
```

- [ ] **Step 3: Add the checksum test** right after it:

```rust
/// The runbook's "record as applied" statement takes the checksum from
/// `sha384sum <file>`. sqlx's checksum is SHA-384 of the file's bytes;
/// Postgres' `sha384` checks that for every embedded migration.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn migration_checksums_are_the_sha384_of_their_files(pool: PgPool) {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let files: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect();
    for m in flags_core::MIGRATOR.iter() {
        let prefix = format!("{:04}_", m.version);
        let path = files
            .iter()
            .find(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix))
            })
            .unwrap_or_else(|| panic!("no file for migration {}", m.version));
        let bytes = std::fs::read(path).unwrap();
        let digest: Vec<u8> = sqlx::query_scalar("SELECT sha384($1)")
            .bind(&bytes)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(digest, m.checksum.as_ref(), "{}", path.display());
    }
}
```

- [ ] **Step 4: Extend the `Dirty` test.** In `a_failed_unknown_migration_still_refuses_to_boot`,
  change the doc comment to:

```rust
/// A migration recorded as failed refuses to boot, even when this release
/// does not know its version. sqlx never writes such a row on Postgres (see
/// `a_failed_migration_leaves_nothing_and_the_previous_release_boots`); only
/// a hand-written row or another tool does. The runbook's human-only
/// `DELETE` clears it.
```

  and append after the existing `assert!(matches!(err, MigrateError::Dirty(..)))`:

```rust
    // The runbook's human-only statement clears it.
    sqlx::query("DELETE FROM _sqlx_migrations WHERE version = $1 AND success = false")
        .bind(newest + 1)
        .execute(&pool)
        .await
        .unwrap();
    flags_core::MIGRATOR.run(&pool).await.unwrap();
```

- [ ] **Step 4b: Run migrators on one closed session.** (Spec critique.) Add a helper
  `async fn boot_migrate(migrator: &Migrator, pool: &PgPool) -> Result<(), MigrateError>` that
  acquires a connection, `detach()`es it, runs the migrator on it, and `close()`s it (import
  `sqlx::Connection`); replace every `newer_release.run(&pool)` and
  `flags_core::MIGRATOR.run(&pool)` in the two tests above with it. `run_direct` returns on a
  migration error without releasing its advisory lock, which would otherwise block the next
  migrator on another pooled connection.

- [ ] **Step 5: Prove each test can fail.** Temporarily invert one assertion per test (e.g.
  `assert_eq!(rows, 1, …)`, `sqlstate "23505"`, compare `digest` against an empty slice, drop the
  `DELETE`), run, see each FAIL, then restore. Run:
  `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --test db -- migration`
  Expected: the inverted test fails.

- [ ] **Step 6: Run the tests.**
  `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --test db -- migration`
  Expected: the migration tests pass, none skipped. Then `cargo fmt --all` and
  `cargo clippy --workspace --all-targets -- -D warnings`.

- [ ] **Step 7: Commit.**

```bash
git add crates/flags-core/tests/db.rs
git commit -m "test(core): pin what a failed migration leaves behind" \
  -m "The recovery runbook for #34 depends on sqlx's behavior when a migration fails in production: no row and none of its changes, the previous release still boots, the newer one retries. These tests pin that, plus the two human-only statements the runbook hands to Rob and the sha384sum checksum it tells him to use." \
  -m "Fixes #34"
```

### Task 2: The runbook

**Files:**
- Modify: `docs/deploy/fly.md` (new section between `## Deploys` and `## Auto-rollback`)
- Modify: `crates/flags-core/src/migrate.rs:18-21` (the `MIGRATOR` doc comment)
- Modify: `CLAUDE.md` (the `- Migrations (agent):` bullet under `### Out-of-band`)

**Interfaces:** cites Task 1's test names.

**Out-of-band / invariants:** every production write is written as a hand-off to Rob; the
runbook never proposes editing a migration.

**Model:** opus — accuracy against the code matters most here.

- [ ] **Step 1: Write the section** in `docs/deploy/fly.md`, following the spec's section 6,
  items 1-6, in the doc's existing style (prose plus `bash`/`sql` blocks). It must contain:
  the boot-log signature (`applying migrations`, `migrations failed`,
  `while executing migration <N>: error returned from database: <message>`, which carries the
  message but not the SQLSTATE); a pointer to `fly status` for machines left on the failed
  release; that nothing is recorded and the previous release keeps
  serving, citing `a_failed_migration_leaves_nothing_and_the_previous_release_boots`; that later
  pushes to `main` also fail to deploy until recovery; the read-only diagnostics (`fly logs`,
  `fly releases`, `fly status`, then Rob's `SELECT … FROM _sqlx_migrations ORDER BY version` and a
  `pg_stat_activity` SELECT filtered to `datname = 'otto_flags'`, via `fly postgres connect -a otto-db -d otto_flags`, which is
  interactive and hangs when run non-interactively); the three recovery paths with their exact
  human-only statements and ordering (transient: re-run the `deploy` job of the newest `main` CI
  run, since each run deploys its own SHA; data: fix the data, or
  merge a converging `N+1` and only then record `N` with `sha384sum` from `git show`; `Dirty`:
  the `DELETE`); the "never" list; the note distinguishing `refusing to serve: …`; the
  `-- no-transaction` sentence; and "afterwards, verify per `CLAUDE.md` Ship > verify".

- [ ] **Step 2: `MIGRATOR` doc comment.** After "…and a migration recorded as failed, known or
  not, with `Dirty`." add: "sqlx never records a failed migration itself on Postgres: each
  migration and its row commit in one transaction, so a failure leaves neither and the previous
  release boots. Recovery: `docs/deploy/fly.md`, \"When a migration fails\"."

- [ ] **Step 3: `CLAUDE.md` pointer.** Append to the `- Migrations (agent):` bullet:
  " Recovery from a failed one: `docs/deploy/fly.md` (When a migration fails)."

- [ ] **Step 4: Gates.** Run every gate in Global Constraints (all four plus `podman build`);
  the docs must not break `cargo doc`-free gates, and the test gate re-proves Task 1.

- [ ] **Step 5: Commit.**

```bash
git add docs/deploy/fly.md crates/flags-core/src/migrate.rs CLAUDE.md
git commit -m "docs(deploy): document recovery from a failed production migration" \
  -m "A migration that fails at boot leaves no row, so the previous release keeps serving while every new deploy crash-loops on it, and the append-only rule rules out the obvious fix of editing the file. The runbook gives the boot-log signature, read-only diagnostics, and each recovery path with the exact human-only statement for Rob." \
  -m "Fixes #34"
```
