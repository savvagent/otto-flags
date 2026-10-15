# evaluate_flag Description Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** `evaluate_flag`'s tool description matches what its handler records: it changes no
flag, and it writes one unbilled `usage_outbox` row (through `Flags::charge`), so it must not say
"records nothing".

**Architecture:** Reword the `description` in the `#[tool]` attribute of `evaluate_flag` in
`crates/flags-mcp/src/tools/insight.rs`. The handler, its metering, and its `read_only_hint`
annotation are unchanged (the row is internal metering, not a change the caller makes). The
`tools_list_carries_annotations` e2e test gains an assertion on the description served by
`tools/list`.

**Tech Stack:** Rust (stable, edition 2021), rmcp.

**Ticket:** savvagent/otto-flags#89

**Spec:** fast-path — no spec (one source file, tool description text only; no new interface,
migration, dependency, or behavior change).

## Global Constraints

- Gates (from the worktree root, `CARGO_BUILD_JOBS=3`, `TMPDIR=$PWD/target/tmp`):
  - test: `DATABASE_URL=postgres://flags:flags@localhost:<port>/otto_flags cargo test --workspace`
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'`
    prints nothing
  - crates/** changed: `podman build -t otto-flags:check .`
- Evidence: pass counts per test binary; skipped tests are not a pass.
- Invariants: metering is unchanged (`evaluate_flag` still records one non-billable row); the
  read-only annotation is unchanged.
- Worktree only; `main` changes only through the PR. No attribution of any kind.

## Task 1: Reword evaluate_flag's description

**Model:** haiku

**Files:**
- Modify: `crates/flags-mcp/src/tools/insight.rs` (`evaluate_flag`'s `#[tool]` description)
- Modify: `crates/flags-server/tests/e2e.rs` (`tools_list_carries_annotations`)

- [ ] **Step 1: Write the failing test.** In `tools_list_carries_annotations`, assert that the
  `description` `tools/list` serves for `evaluate_flag` does not contain `records nothing` and
  does contain `not billed`.
- [ ] **Step 2: Run it and see it fail:**
  `cargo test -p flags-server --test e2e tools_list_carries_annotations`.
- [ ] **Step 3: Implement.** Replace "Changes nothing and records nothing." with wording that
  says it changes no flag and that the call is recorded as usage but not billed.
- [ ] **Step 4: Run it and see it pass**, then the full gates above.
- [ ] **Step 5: Commit** `docs(mcp): say evaluate_flag records unbilled usage`, body explaining
  why, ending `Fixes #89`.
