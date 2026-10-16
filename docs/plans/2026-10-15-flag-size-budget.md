# Bound a flag's size and an app's SDK flag list Implementation Plan

> Execute task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

> Shipped with the per-flag cap at 128 KiB rather than the 64 KiB first planned (spec section 3).

**Goal:** every flag write that would grow a flag past 128 KiB, or an app's active flags past
1 MiB (each flag's size plus 256 bytes), is refused; disable-only environment patches and writes
that do not grow always land; the SDK guide documents the bound.

**Architecture:** constants and one size-check query in `crates/flags-core/src/flags.rs`, run
before the `INSERT` in `create_flag` and before the `UPDATE` in `commit_change` (skipped for a
disable-only environment patch). Docs in `docs/SDK-DEVELOPER-GUIDE.md` and the MCP argument
docs. No migration.

**Ticket:** savvagent/otto-flags#123 (part 2 of 2; closes it)

**Spec:** `docs/specs/2026-10-15-flag-size-budget-design.md`

## Global Constraints

- Worktree only: `/home/robhicks/dev/otto-flags/.claude/worktrees/feat-123-bound-flag-size`;
  `main` changes only through the PR.
- Every cargo command runs with `CARGO_BUILD_JOBS=3 TMPDIR=$PWD/target/tmp` from the worktree root
  (`mkdir -p target/tmp` first).
- Gates:
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15452/otto_flags cargo test --workspace`
    (dedicated container `otto-flags-pg-15452`; never 15434 or a Fly database)
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'`
    prints nothing
  - `podman build -t otto-flags:check .` (crates changed; once, before the PR)
- Evidence: pass counts per test binary; skipped tests are not a pass; `cargo test` without
  `DATABASE_URL` panics rather than skipping.
- Invariants: no migration or constraint; SDK REST contract unchanged in shape (documented bound
  only); client keys still never see rules; the check is a tenant query in the caller's `Tx`
  with `org_id` in every predicate; refusals built in code (no database error text); no SQL
  outside `flags-core`; runtime `sqlx::query` only; auto-rollback and the kill switch never
  refused.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #123`. No attribution of any kind.
- Shipped: before merge, a final commit sets the spec's `**Status:**` to `IMPLEMENTED (#<PR>)`
  and ticks these boxes.
- Reviews: rust-pro, architect, blind security, database-optimizer (SQL changes).

## Task 1: Size budgets on write

Model: sonnet

Files:
- Modify: `crates/flags-core/src/flags.rs` (constants, `SizeCheck`, `check_size`, calls in
  `create_flag`, `commit_change` and its callers, `apply_env_patch`; unit test for the overhead)
- Modify: `crates/flags-core/tests/db.rs` (new tests at the end)

- [x] **Step 1: Failing tests.** Unit: `the_list_overhead_covers_an_empty_flag` (a `SdkFlag` with
  a 128-character key, `version: i32::MAX`, empty variations, no configuration, `{}`
  environments, serialized plus one comma, is at most `FLAG_OVERHEAD_BYTES`). DB:
  `a_flag_is_capped_at_128_kib`, `an_apps_active_flags_are_capped_at_1_mib`,
  `writes_that_do_not_grow_land_over_budget` per spec section 10, using a test helper that
  measures stored size with the same SQL expression.
- [x] **Step 2: Run, see them fail:**
  `DATABASE_URL=postgres://flags:flags@localhost:15452/otto_flags cargo test -p flags-core --test db capped`
- [x] **Step 3: Implement** per spec section 6.
- [x] **Step 4: Run, see them pass**, then the full `db` binary and `cargo test -p flags-core --lib`.
- [x] **Step 5: Commit** `feat(core): cap a flag at 128 kib and an app's active flags at 1 mib`
  with a why-body ending `Refs #123`.

## Task 2: List bound test and docs

Model: sonnet

Files:
- Modify: `crates/flags-server/tests/e2e.rs` (new test)
- Modify: `docs/SDK-DEVELOPER-GUIDE.md`, `crates/flags-mcp/src/tools/flags.rs` (argument docs)

- [x] **Step 1: E2E test** `the_sdk_flag_list_is_bounded_by_the_app_budget`: an app seeded by SQL
  (superuser pool) with flags carrying non-ASCII strings, a fractional `rollout_percentage`,
  long numbers and empty variations, up to (not over) the budget by the stored measure; `GET
  /api/sdk/flags` with the server key returns a body no longer than the stored total plus 256.
- [x] **Step 2: Run it** (passes on Task 1's code; the bound is a property of storage):
  `DATABASE_URL=postgres://flags:flags@localhost:15452/otto_flags cargo test -p flags-server --test e2e sdk_flag_list`
- [x] **Step 3: Docs.** Guide: rewrite the Flag Evaluation paragraph's last sentence; add the
  bounds and recommended read caps (list 4 MiB, stream event 1 MiB) to the list and SSE
  sections. MCP: `create_flag`/`update_flag` argument docs mention the 128 KiB flag and 1 MiB app
  limits.
- [x] **Step 4: Commit** `docs(sdk): document the flag list bound` (or `test(server): ...`) with
  a why-body ending `Fixes #123`.

## Task 3: Full gates

Model: haiku

- [x] Run test, lint, format, extra, and `podman build`; record per-binary pass counts and
  `git rev-parse HEAD`.
