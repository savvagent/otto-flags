# Recovering from a migration that fails in production

**Status:** IMPLEMENTED (#57)
**Issue:** #34

## 1. Brief

From #34 (found in review of #31):

> Migrations apply at boot under an advisory lock. If one fails in production (a lock timeout, a
> constraint the data violates), sqlx records it as failed (`success = false`), and every later
> boot refuses with `Dirty` until that row is dealt with. The fix cannot be an edit to the failed
> file (append-only, checksummed), and `docs/deploy/fly.md` gives no recovery procedure.
>
> **Acceptance criteria**
> - [ ] `docs/deploy/fly.md` documents recovery from a failed migration: how to see the failed row
>   (a SELECT on `_sqlx_migrations`), the human-only step that clears it (exact statement, for
>   Rob), and that the corrected change ships as a new migration, never an edit.
> - [ ] A test or doc note confirms what the previous release does when the newest migration
>   failed (it refuses with `Dirty`, per #31's test), so the runbook matches the code.

## 2. Premise corrections

Read against sqlx 0.8.6 (`Cargo.lock`), `sqlx-postgres/src/migrate.rs` and
`sqlx-core/src/migrate/migrator.rs`:

1. **sqlx never records a failed migration on Postgres.** `PgConnection::apply` runs the
   migration's SQL and the `INSERT INTO _sqlx_migrations (…, success, …) VALUES (…, TRUE, …)` in
   one transaction (unless the file starts with `-- no-transaction`; none here does). A failure
   returns `MigrateError::ExecuteMigration(err, version)` and the transaction rolls back: no row,
   and none of the migration's changes. The only write of `success` anywhere in the driver is
   `TRUE`. So a real failure does not leave a `success = false` row, and does not cause `Dirty`.
2. **The previous release therefore boots normally after the newest migration failed.** It does not
   know that version and finds no row for it. It refuses with `Dirty` only when a
   `success = false` row exists, which only a hand-written row or another tool produces. #31's test
   (`a_failed_unknown_migration_still_refuses_to_boot`) inserts such a row by hand; it pins what
   `Dirty` does, not what a failure leaves. The AC's parenthetical is corrected accordingly; the
   AC itself ("a test confirms what the previous release does … so the runbook matches the code")
   is met by a new test of a real failure.
3. **The new release retries the failed migration from scratch on every boot**, so it crash-loops
   until the cause is gone, and every later push to `main` fails to deploy the same way.
4. **"The corrected change ships as a new migration" does not unblock on its own.** The failed
   migration stays in the tree (append-only) and sqlx applies versions in order, so `N+1` never
   runs while `N` keeps failing. Recovery is either making `N` succeed as written (wait out a
   transient cause, or fix the data, a human-only production write), or a human recording `N` as
   applied without running it (exact statement below), with the correction shipped as `N+1`. The
   AC's "clear the failed row" maps to the `Dirty` case, which the runbook still covers with an
   exact statement.

## 3. Assumptions

- **Placement:** a new `## When a migration fails` section in `docs/deploy/fly.md`, after
  `## Deploys`, which is where operators look after a failed deploy. `CLAUDE.md`'s Out-of-band >
  Migrations line gains a one-line pointer to it.
- **Who runs SQL in production:** Rob, through `fly postgres connect -a otto-db -d otto_flags`
  (interactive; it hangs when run non-interactively, so an agent cannot use it). Every write in
  the runbook is marked human-only, per the Human-only list. Agents diagnose from `fly logs`,
  `fly releases`, and `fly status`, which need no database session.
- **Re-running the CI `deploy` job of the newest `main` CI run** (`gh run rerun <id> --failed`) is
  the retry path. The `deploy` job deploys its own run's SHA, so re-running an older run would
  deploy an older commit; every later push failed the same way, so the newest run is the one to
  re-run. It runs migrations (DDL) against production, so after review it is **Rob's call**, run
  by an agent only when he asks, like a hand `fly deploy`; `CLAUDE.md` Out-of-band says so.
- **The description column is cosmetic:** sqlx validates only version and checksum
  (`validate_applied_migrations` and the checksum compare in `run_direct`). The runbook still
  gives the value sqlx would write (the file name without version and `.sql`, `_` as spaces).
- **The checksum is SHA-384 of the file's bytes** (`Migration::new`: `Sha384::digest(sql)`, with
  `sql` from `fs::read_to_string`, unmodified). A test proves `sha384sum <file>` equals the
  embedded checksum for every migration, so the human's command is right.
- Every Run Profile value was read from the Development Workflow section; none assumed.

## 4. Goal and success criteria

An operator whose deploy failed on a migration can tell from `docs/deploy/fly.md` what state the
database is in, what is still serving, and the exact human-only statement for each recovery path;
and a test pins the behavior the runbook describes.

- The runbook states, and a test proves: a failing migration leaves no `_sqlx_migrations` row and
  none of its changes; the previous release's `MIGRATOR` still runs; the new one fails the same
  way again (not `Dirty`).
- The runbook's "record as applied" statement, run as written, lets the new release boot without
  executing the migration; a test runs that statement shape.
- The runbook's `Dirty` statement (`DELETE … WHERE version = N AND success = false`) lets every
  release boot again; the existing `Dirty` test is extended to run it.
- A test proves `sha384sum` of each migration file equals the checksum sqlx embeds.
- No production action is performed or tested; every production write is handed to Rob.

## 5. Scope

**In:** `docs/deploy/fly.md` (new section); tests in `crates/flags-core/tests/db.rs`; the doc
comment on `MIGRATOR` (`crates/flags-core/src/migrate.rs`) gains one sentence saying sqlx never
writes a failed row itself; one pointer line in `CLAUDE.md` Out-of-band.

**Out:** any migration or schema change; changing how migrations run (`lock_timeout` defaults,
`no-transaction`, a separate migration job); automating recovery; running any recovery step
against production; the `fly postgres connect` non-interactive hang (a flyctl behavior).

## 6. Architecture

No code path changes. The runbook section covers, in order:

1. **What a failure looks like.** Boot log: `applying migrations`, then the process exits with
   `migrations failed`, whose cause chain includes
   `while executing migration <N>: error returned from database: <message>` (sqlx's displays; the
   message, not the SQLSTATE). The new machine never binds a port and `/readyz` fails; the deploy
   fails; whether Fly rolled it back is not assumed: `fly status` is the check for any machine
   left on the failed release (moving one back is Rob's hand deploy). The previous release keeps
   serving on machines Fly did not move: no row was written. Later pushes to `main` fail the same way until
   recovery; they are merged, not live.
2. **Diagnose (read-only).** `fly logs -a otto-flags --no-tail` (the version and the database's message; causes are told apart by message text, never SQLSTATE, which the log does not carry),
   `fly releases -a otto-flags`, `fly status -a otto-flags`; then Rob, in `fly postgres connect`:
   ```sql
   SELECT version, description, success, encode(checksum, 'hex') AS checksum, installed_on
   FROM _sqlx_migrations ORDER BY version;
   ```
   and, for a lock wait, a `pg_stat_activity` SELECT for long transactions, filtered to
   `datname = 'otto_flags'` (the instance also hosts `otto_platform` and `otto_factory`).
3. **Recover by cause.**
   - *Transient* (`canceling statement due to lock timeout`: migrations set `SET LOCAL lock_timeout`, as `0002` and
     `0003` do; a dropped connection; a stopped machine): nothing to clear. Re-run the CI deploy.
     Ending a blocking session (`pg_terminate_backend`) is human-only.
   - *The SQL fails on production data* (`violates check constraint`, `could not create unique index`, …): preferred, Rob fixes the data
     so `N` applies as written (the statement is the fix PR author's, with the SELECT that shows
     the offending rows; run in a transaction), then re-run the newest deploy. When `N` must never run as
     written: merge `N+1`, written to converge both states (databases where `N` ran, such as CI and
     local, and production where it did not), and only then Rob records `N` as applied:
     ```sql
     INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
     VALUES (<N>, '<description>', true, decode('<hex>', 'hex'), -1);
     ```
     with `<hex>` from `git show <sha>:crates/flags-core/migrations/<file> | sha384sum`. Then
     re-run the deploy. Recording `N` before `N+1` is deployable would let a restart boot the new
     code against a schema without `N`.
   - *A `success = false` row (`Dirty`)*: every release, old and new, refuses to boot; machines
     already running keep serving only until they restart (resuming from suspend does not re-run migrations). Confirm nothing of `N` is half applied, then Rob runs
     `DELETE FROM _sqlx_migrations WHERE version = <N> AND success = false;`.
4. **Never:** edit, rename, delete, or renumber the failed file, or reuse its number (checksums,
   CI's `migrations-append-only`, the contiguity test, and databases that already applied it);
   run a migration's SQL by hand (objects it creates are owned by whoever runs it: `0004`'s
   function must be owned by an RLS-exempt role, and the row's checksum still has to match);
   boot the new release with `FLAGS_RUN_MIGRATIONS=false` against a schema missing `N`.
5. **A boot that fails after migrations** (`refusing to serve: …`) is not a migration failure:
   see Infrastructure (isolation proof, audit purge owner).
6. **Afterwards:** the verify steps in `CLAUDE.md` Ship > verify.

Tests in `crates/flags-core/tests/db.rs` (`#[sqlx::test(migrator = "flags_core::MIGRATOR")]`):

- `a_failed_migration_leaves_nothing_and_the_previous_release_boots`: a `Migrator` of
  `MIGRATOR`'s migrations plus a version-`N+1` migration that creates a table, inserts a row, and
  adds a `CHECK` that row violates. Run it: `ExecuteMigration(_, N+1)` with SQLSTATE `23514`; no
  `_sqlx_migrations` row for `N+1` and no `success = false` row; the table does not exist;
  `flags_core::MIGRATOR.run` and `flags_core::migrate` succeed; running the newer migrator again
  fails the same way. Then the runbook's record-as-applied `INSERT` (checksum bound as hex);
  the newer migrator succeeds and the table still does not exist.
- `migration_checksums_are_the_sha384_of_their_files`: for every embedded migration, Postgres'
  `sha384` of the file's bytes (read from `CARGO_MANIFEST_DIR/migrations`) equals the embedded
  checksum.
- `a_failed_unknown_migration_still_refuses_to_boot` gains the runbook's `DELETE`, after which
  `MIGRATOR.run` succeeds.
- Every migrator in these tests runs through a `boot_migrate` helper on one detached connection
  that is closed afterwards: `run_direct` returns a migration error without `unlock()`, so on a
  pooled connection the session-level advisory lock would outlive the call and block the next
  migrator on another connection. In production the process exits, which ends the session.

## 7. Invariants impact

- **Append-only migrations / rollback compatibility:** no migration added or changed. The runbook
  restates both rules and never proposes an edit.
- **Human-only:** every production write in the runbook (data fix, `INSERT`, `DELETE`,
  `pg_terminate_backend`, a hand deploy, and re-running a deploy) names Rob; agents get read-only
  diagnostics only, and re-run a deploy only when Rob asks.
- **No SQL outside flags-core:** the new SQL is in `flags-core`'s tests.
- Tenant isolation, MCP auth, metering, SDK contract: untouched.

## 8. Migration

None.

## 9. Out-of-band

None. The change is docs and tests; the deploy redeploys unchanged server code.

## 10. Error handling and edge cases

- A `-- no-transaction` migration (none exists) runs outside any sqlx transaction with its row
  recorded separately, so the runbook's guarantees would not hold; after review,
  `no_migration_runs_outside_a_transaction` refuses one and the runbook's Never list says why.
- Two machines booting at once: the advisory lock serializes them; the loser waits, then sees the
  same failure. Nothing extra to clear.
- A machine killed after commit but before the `execution_time` update leaves a `success = true`
  row with `execution_time = -1`: applied, harmless.

## 11. Testing approach

Gates per `CLAUDE.md`: test (`DATABASE_URL=… cargo test --workspace`), clippy, fmt, the
migrations `extra` check, and `podman build` (the change touches `crates/**`). The pnpm and SDK
gates do not apply. DB tests run once, serialized (no other worktree runs them).

## 12. Risks and open questions

- Whether Fly rolls a failed deploy back is not re-tested here (that would need a failing
  production deploy). After review, the runbook and `CLAUDE.md` Ship > verify both say not to
  assume it: `fly status` must show no machine left on the failed release, and moving one back
  is Rob's hand deploy.
- `fly postgres connect` is the only documented production SQL path and is interactive; giving
  agents a read-only non-interactive path is a separate concern, not this issue.
