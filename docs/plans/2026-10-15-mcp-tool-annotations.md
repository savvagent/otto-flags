# MCP Tool Annotations Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [x]`) syntax for tracking.

**Goal:** Every tool on `/mcp` declares `readOnlyHint` (and, for writes, `destructiveHint` and
`idempotentHint`), pinned by tests that fail on an unannotated or misclassified tool.

**Architecture:** `annotations(...)` inside each `#[tool(...)]` attribute in
`crates/flags-mcp/src/tools/{org,apps,flags,insight,rollback}.rs`; pinned sets and tests in
`crates/flags-mcp/src/tools/mod.rs`.

**Tech Stack:** Rust (stable, MSRV 1.88), rmcp =2.0.0.

**Ticket:** savvagent/otto-flags#77

**Spec:** `docs/specs/2026-10-15-mcp-tool-annotations-design.md`

## Global Constraints

- Worktree only (`/home/robhicks/dev/otto-flags/.claude/worktrees/feat-77-mcp-tool-annotations`);
  `main` changes only through the PR.
- Every cargo command runs with `CARGO_BUILD_JOBS=3 TMPDIR=$PWD/target/tmp` (from the worktree
  root; `target/tmp` exists).
- Gates (from `CLAUDE.md` > Gates), from the worktree root. This worktree has its own Postgres on
  port 15444 (other agents run DB tests on the shared one at 15434 concurrently):
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15444/otto_flags cargo test --workspace`
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'` prints nothing
  - when `crates/**`: `podman build -t otto-flags:check .` (run by the orchestrator before the PR)
- Evidence: pass counts per test binary; a `DATABASE_URL must be set` panic is a missing variable,
  not a pass. sqlx test-db setup panics inside sqlx-core under load are environmental: re-run.
- Invariants that apply: MCP auth (every tool still checks its scope; annotations are hints, not
  authorization); metering unchanged (`flags_core::usage::BILLABLE` is read, never edited). No SQL,
  no migration, nothing in `flags-core` changes.
- Out-of-band: none.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #77`. No attribution of any kind (no Co-Authored-By, no "Generated with", no credit
  markers).

## Task 1: Annotate every tool, with tests that pin the classification

**Model:** sonnet

**Files:**
- Modify: `crates/flags-mcp/src/tools/mod.rs` (constants + tests)
- Modify: `crates/flags-mcp/src/tools/org.rs`, `apps.rs`, `flags.rs`, `insight.rs`, `rollback.rs`
  (the `#[tool(...)]` attributes only)

- [x] **Step 1: Write the failing tests.** In `crates/flags-mcp/src/tools/mod.rs`, after `NAMES`,
  add three `pub const` slices with a doc comment each explaining the rule (reads: no write scope,
  not billed; destructive: can switch off or alter live evaluation, or delete; idempotent: a
  repeat has no further effect, which excludes every billable tool since each call records usage):
  - `READ_ONLY`: `whoami`, `usage`, `list_apps`, `list_flags`, `get_flag`, `flag_history`,
    `evaluate_flag`, `flag_health`, `list_rollback_policies`, `propose_rollback`,
    `rollback_events`.
  - `DESTRUCTIVE`: `update_app`, `rotate_app_keys`, `update_flag`, `set_flag_environment`,
    `archive_flag`, `restore_flag`, `rollback_flag`, `set_rollback_policy`,
    `remove_rollback_policy`.
  - `IDEMPOTENT`: `remove_rollback_policy`.

  In the `tests` module add:
  1. `every_tool_is_annotated`: for each `router().list_all()` tool, `annotations` is `Some`
     (`panic!("{name} has no annotations")`; never a bare `unwrap()`, here or in test 2); `read_only_hint` is `Some`; if `Some(false)`,
     `destructive_hint.is_some()` and `idempotent_hint.is_some()` with the tool name in the
     message.
  2. `annotations_match_the_pinned_sets`: for each tool, `read_only_hint == Some(READ_ONLY.contains(name))`;
     for writes, `destructive_hint == Some(DESTRUCTIVE.contains(name))` and
     `idempotent_hint == Some(IDEMPOTENT.contains(name))`; for reads, `destructive_hint` and
     `idempotent_hint` are `None`. Also assert every name in the three constants is in `NAMES`
     and `DESTRUCTIVE`/`IDEMPOTENT` are disjoint from `READ_ONLY`.
  3. `read_only_tools_change_nothing`: no `READ_ONLY` name is in `flags_core::usage::BILLABLE`.
     Then a source scan over `include_str!("org.rs")`, `"apps.rs"`, `"flags.rs"`, `"insight.rs"`,
     `"rollback.rs"`; for each name in `NAMES`, take the body from
     `format!("pub async fn {name}(")` to the first `"\n    }\n"` after it (the method's closing
     brace at impl indentation; rustfmt guarantees the shape), panicking with the tool and file
     names if the function or its end is not found, or if the body is empty or contains another method or helper at impl
     indentation (`\n    pub `, `\n    async fn `, `\n    fn `), a layout change that would let
     one body run into the next. A `READ_ONLY` body must contain none of `FLAGS_WRITE`, `APPS_ADMIN`, `require_admin`,
     `set_archived` (reads do call `self.charge`: every tool meters a non-billable usage row); every other body must contain at least one of `FLAGS_WRITE`,
     `APPS_ADMIN`, `set_archived` (archive/restore delegate their `FLAGS_WRITE` check to
     `Flags::set_archived`).
- [x] **Step 2: Run and see them fail.** `CARGO_BUILD_JOBS=3 TMPDIR=$PWD/target/tmp cargo test -p flags-mcp --lib tools::tests`
  — `every_tool_is_annotated` and `annotations_match_the_pinned_sets` fail ("whoami has no
  annotations" or similar); `read_only_tools_change_nothing` passes (it checks existing code).
- [x] **Step 3: Annotate.** Add to each `#[tool(...)]`, after `description`:
  - read tools: `annotations(read_only_hint = true)`
  - `create_app`, `create_flag`: `annotations(read_only_hint = false, destructive_hint = false, idempotent_hint = false)`
  - `remove_rollback_policy`: `annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = true)`
  - every other write: `annotations(read_only_hint = false, destructive_hint = true, idempotent_hint = false)`
  Do not change descriptions, arguments, or bodies.
- [x] **Step 4: Run and see them pass.** Same command as Step 2: all `tools::tests` pass. Then the
  full gates: test (with `DATABASE_URL` on 15444), lint, format, extra — report pass counts per
  test binary.
- [x] **Step 5: Commit.**
  `feat(mcp): annotate every tool as read-only or destructive` with a body explaining that
  connector clients (Claude, ChatGPT) use the hints to auto-run reads and confirm writes, that the
  tests pin the classification and fail on an unannotated tool, and ending `Fixes #77`.
