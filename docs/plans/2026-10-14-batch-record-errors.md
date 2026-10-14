# Batch error telemetry Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** `flags_core::telemetry::record_errors` writes a batch with one `INSERT … SELECT FROM
UNNEST`, with results and stored values unchanged.

**Architecture:** Same key lookup as today; the loop builds parallel vectors (truncated, `sane`
timestamps) instead of executing; one statement binds them with `org_id` and `reporter` as scalars.

**Tech Stack:** Rust (stable, MSRV 1.88), sqlx 0.8.6, Postgres 16.

**Ticket:** savvagent/otto-flags#23

**Spec:** `docs/specs/2026-10-14-batch-record-errors-design.md`

## Global Constraints

- Worktree only (`/home/robhicks/dev/otto-flags/.claude/worktrees/fix-23-batch-record-errors`);
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
- Invariants that apply: no per-row query fan-out (the point of this change); tenant isolation (the
  insert runs in the caller's pinned `Tx`, `org_id` from `tx.org()`); no SQL outside flags-core and
  runtime `sqlx::query` only (no `query!` macros); SDK text bound as parameters, never interpolated;
  truncation caps unchanged.
- Out-of-band: none. No migration.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #23`. No attribution of any kind (no Co-Authored-By, no "Generated with", no credit
  markers).

### Task 1: Write error batches with one statement

**Model:** sonnet

**Files:**
- Modify: `crates/flags-core/src/telemetry.rs` (`record_errors` only)
- Modify: `crates/flags-core/tests/db.rs` (one new test, beside the existing telemetry tests)

- [ ] **Step 1: Write the test** `error_batches_land_one_row_per_known_event` in
  `crates/flags-core/tests/db.rs` with `#[sqlx::test(migrator = "flags_core::MIGRATOR")]`, following
  the setup of the neighbouring telemetry tests (an org, an app, two flags `f` and `g`, a pinned
  `Tx`). Record one batch with `KeyKind::Server`:
  - `f`, enabled, `environment: Some("production")`, `stack_trace: Some(..)`, `at: now`
  - `g`, disabled, `environment: None`, `stack_trace: None`, `at: now - 1 minute`
  - `f`, enabled, an `error_message` of 3,000 characters, `at: now + 1 hour` (future: stored as
    roughly now)
  - `missing` (no such flag)
  Assert: `receipt == Receipt { accepted: 3, unknown_flags: 1 }`; `recorded.flags` is `[f, g]`
  sorted as `FlagId`s sort; `SELECT count(*) FROM flag_errors` is 3; the `g` row has NULL
  `environment` and NULL `stack_trace` and `flag_enabled = false`; the long message is stored at
  2,000 characters; the future-dated row's `occurred_at` is not in the future; every row's
  `reporter` is `server`. Find rows by content (flag and message), never by `id` order.
- [ ] **Step 2: Run it against the current code; it should pass** (this pins today's behaviour
  before the rewrite):
  `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --test db error_batches_land`
- [ ] **Step 3: Rewrite `record_errors`** as the spec's §6 describes: keep the key lookup, the
  receipt counting, and the `flags` dedup/sort; build `Vec<Uuid>` (flag ids via `.as_uuid()`),
  `Vec<Option<String>>` environment (truncate 64), `Vec<bool>`, `Vec<String>` error_type (truncate
  `MAX_ERROR_TYPE`), `Vec<String>` error_message (truncate `MAX_ERROR_MESSAGE`),
  `Vec<Option<String>>` stack_trace (truncate `MAX_STACK`), `Vec<DateTime<Utc>>` (`sane(e.at,
  now)`); return early with no statement if no event was accepted; then one
  `sqlx::query(...)` with the SQL in the spec's §6, binding `org`, the seven vectors, and
  `reporter.as_str()` in that order (`$1`..`$9`), executed on `tx.conn()`.
- [ ] **Step 4: Run the new test and every existing telemetry and rollback test:**
  `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --test db`
- [ ] **Step 5: Run every gate** (Global Constraints) and report pass counts per test binary.
- [ ] **Step 6: Commit** `crates/flags-core/src/telemetry.rs` and `crates/flags-core/tests/db.rs`
  with exactly this message (no trailers of any kind):

  ```
  fix(core): write error telemetry batches with one statement

  record_errors ran one INSERT per event, so a 1,000-event batch held a
  pooled connection for up to 1,000 round trips on the SDK request path.
  It now binds the batch as arrays and inserts it with one UNNEST, as
  record_evaluations does; stored values and the receipt are unchanged.

  Fixes #23
  ```

The spec's `**Status:**` line and this plan's boxes are updated in the PR's final commit by the
orchestrator.
