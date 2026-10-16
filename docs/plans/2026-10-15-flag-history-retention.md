# Retain flag history for 90 days and at least 100 versions Implementation Plan

> Execute task-by-task. Steps use checkbox (`- [x]`) syntax for tracking.

**Goal:** an hourly background sweep removes flag versions that are both older than 90 days and
outside the flag's newest 100; no request path deletes history.

**Architecture:** Migration 0006 adds marker-gated unpinned SELECT and DELETE policies on
`flag_versions`; `flags_core::flags::sweep_versions` deletes in batches through
`maintenance::begin`; `flags-server`'s `sweep_loop` calls it. Message, doc, and description
updates.

**Ticket:** savvagent/otto-flags#123 (part 1 of 2)

**Spec:** `docs/specs/2026-10-15-flag-history-retention-design.md`

## Global Constraints

- Worktree only:
  `/home/robhicks/dev/otto-flags/.claude/worktrees/feat-123-flag-history-retention`; `main`
  changes only through the PR.
- Every cargo command runs with `CARGO_BUILD_JOBS=3 TMPDIR=$PWD/target/tmp` from the worktree root
  (`mkdir -p target/tmp` first).
- Gates:
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15452/otto_flags cargo test --workspace`
    (this run's dedicated container `otto-flags-pg-15452`; never 15434 or a Fly database)
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'`
    prints nothing (0006 is an `A`)
  - `podman build -t otto-flags:check .` (crates changed; once, before the PR)
- Evidence: pass counts per test binary; skipped tests are not a pass; `cargo test` without
  `DATABASE_URL` panics rather than skipping.
- Invariants: append-only migrations (new 0006 only; never edit 0001-0005, not even the stale
  baseline comment); policies admit only unpinned + marker; deletion only in background code via
  `maintenance::begin`; no SQL outside `flags-core`; runtime `sqlx::query` only; database error
  text never reaches a tool response.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Refs #123` (part 2 closes the issue; the PR body says `Refs #123`). No attribution of any
  kind.
- Shipped: before merge, a final commit sets the spec's `**Status:**` to `IMPLEMENTED (#<PR>)`
  and ticks these boxes.
- Reviews: rust-pro, architect, blind security (focus includes caller-driven erasure of
  history), plus database-optimizer (SQL and a migration).

## Task 1: Migration, sweep, and wiring

Model: sonnet

Files:
- Create: `crates/flags-core/migrations/0006_flag_versions_retention.sql`
- Modify: `crates/flags-core/src/flags.rs` (module docs, constants, `sweep_versions`)
- Modify: `crates/flags-core/src/error.rs`, `crates/flags-core/src/audit.rs`,
  `crates/flags-core/src/maintenance.rs` (docs and message)
- Modify: `crates/flags-server/src/main.rs` (`sweep_loop`)
- Modify: `crates/flags-mcp/src/tools/insight.rs`, `crates/flags-mcp/src/tools/flags.rs`
- Modify: `crates/flags-core/tests/db.rs`

- [x] **Step 1: Failing DB tests** `flag_versions_are_swept_by_age_beyond_the_newest_100` and
  `flag_versions_need_the_maintenance_marker_unpinned`, as in spec section 10.
- [x] **Step 2: Run, see them fail** (compile error on `sweep_versions`, or zero rows swept):
  `DATABASE_URL=postgres://flags:flags@localhost:15452/otto_flags cargo test -p flags-core --test db flag_versions_`
- [x] **Step 3: Implement** per spec section 6.
- [x] **Step 4: Run, see them pass**, then the full flags-core `db` binary.
- [x] **Step 5: Commit** `feat(core): sweep flag versions older than 90 days beyond the newest 100`
  with a why-body ending `Refs #123`.

## Task 2: Full gates

Model: haiku

- [x] Run test, lint, format, extra, and `podman build`; record per-binary pass counts and
  `git rev-parse HEAD`. The change touches `crates/` and a migration, so post-merge verification
  checks the boot log for `applying migrations` and exercises `flag_history` read-only.
