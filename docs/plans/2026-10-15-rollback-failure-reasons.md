# Auto-Rollback Checker Failure Reasons Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** The auto-rollback checker's three failure events log the text-free fields of
`Error::failure_reason()` (`reason`, `status`, `sqlstate`, `transport`) alongside `code`, and
their `debug` twins carry the same fields plus `error = %e`.

**Architecture:** In `crates/flags-core/src/rollback.rs`, the policy failure in `check_flag`,
the `check_one` failure, and the backstop `due_policies` failure each bind
`let f = e.failure_reason();` and log the fields, as `crates/flags-api/src/platform.rs` and
`crates/flags-core/src/usage.rs` do. Levels, messages, control flow, and
`rollback_policies.last_error_code` are unchanged. A new test binary captures the checker's
output with a closed pool and checks two of the three sites end to end.

**Tech Stack:** Rust (stable, edition 2021), `tracing`, `tracing-subscriber` (test only), sqlx.

**Ticket:** savvagent/otto-flags#54

**Spec:** fast-path — no spec (one source file, log fields only, no interface, migration, or
dependency change).

## Global Constraints

- Gates (from the worktree root, `CARGO_BUILD_JOBS=3`, `TMPDIR=$PWD/target/tmp`):
  - test: `DATABASE_URL=postgres://flags:flags@localhost:<port>/otto_flags cargo test --workspace`
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'`
    prints nothing
  - crates/** changed: `podman build -t otto-flags:check .`
- Evidence: pass counts per test binary; skipped tests are not a pass.
- Invariants: database and platform text never reaches a log above `debug`
  (`crates/flags-server/tests/log_text.rs` passes with no exceptions); no SQL, migration, or
  response change.
- Worktree only; `main` changes only through the PR. No attribution of any kind.

## Task 1: Log failure reasons at the three checker sites

**Model:** sonnet

**Files:**
- Modify: `crates/flags-core/src/rollback.rs` (policy failure in `check_flag`, backstop arm of
  `run`, error arm of `check_one`)
- Create: `crates/flags-server/tests/rollback_logs.rs`

- [x] **Step 1: Write the failing test.** `rollback_logs.rs`, its own binary (callsite-interest
  caching): spawn `Checker` on a `Db` over a lazily connected pool that is then closed, with
  `Entitlement` granting auto-rollback, send one hint, wait until the capture holds both the
  `auto-rollback backstop could not list due policies` and `auto-rollback check failed` lines,
  and assert each `WARN` line carries `code="internal_error"` and `reason="pool_closed"` and
  each `DEBUG` twin carries `reason="pool_closed"` and `error=`.
- [x] **Step 2: Run it and see it fail** on the missing `reason` field:
  `cargo test -p flags-server --test rollback_logs`.
- [x] **Step 3: Implement.** At each site bind `let f = e.failure_reason();` and log
  `code = e.code(), reason = f.reason, status = f.status, sqlstate = f.sqlstate(),
  transport = f.transport` at the existing level, and the same fields plus `error = %e` at
  `debug`.
- [x] **Step 4: Run it and see it pass**, then the full gates above.
- [x] **Step 5: Commit** `feat(core): log failure reasons in the auto-rollback checker`, body
  explaining why, ending `Fixes #54`.
