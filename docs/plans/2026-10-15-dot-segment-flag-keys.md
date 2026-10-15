# Refuse dot-segment flag keys Implementation Plan

> Execute task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `create_flag` refuses the keys `.` and `..` with `invalid_argument`, pinned by tests.

**Architecture:** One extra check in `validate_key` (`crates/flags-core/src/flags.rs`), a unit
test beside it, a DB test through `create_flag`, and one doc line on the `create_flag` MCP tool's
`key` argument. No SQL, no migration, no new types.

**Ticket:** savvagent/otto-flags#105

**Spec:** `docs/specs/2026-10-15-dot-segment-flag-keys-design.md`

## Global Constraints

- Worktree only:
  `/home/robhicks/dev/otto-flags/.claude/worktrees/fix-105-reject-dot-flag-keys`; `main` changes
  only through the PR.
- Every cargo command runs with `CARGO_BUILD_JOBS=3 TMPDIR=$PWD/target/tmp` from the worktree root
  (`mkdir -p target/tmp` first).
- Gates:
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15449/otto_flags cargo test --workspace`
    (this run's dedicated Postgres on 15449, never 15434 or a Fly database)
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'`
    prints nothing
  - `podman build -t otto-flags:check .` (crates changed; once, before the PR)
- Evidence: pass counts per test binary; skipped tests are not a pass.
- Invariants: no migration; SDK REST contract unchanged; validation happens before any SQL.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #105`. No attribution of any kind.

## Task 1: Refuse `.` and `..` in `validate_key`

Model: sonnet

Files:
- Modify: `crates/flags-core/src/flags.rs` (`validate_key`, `mod tests`)
- Modify: `crates/flags-core/tests/db.rs` (new test)
- Modify: `crates/flags-mcp/src/tools/flags.rs` (`CreateFlagArgs::key` doc)

- [ ] **Step 1: Write the failing unit test** in `flags.rs` `mod tests`:
  `dot_segment_keys_are_refused`: `validate_key(".")`, `validate_key("..")`,
  `validate_key(" .. ")` are `Err(Error::Invalid(_))` whose message contains "dot segment";
  `validate_key("...")`, `"a.b"`, `".a"`, `"a."` are `Ok` and unchanged.
- [ ] **Step 2: Write the failing DB test** in `tests/db.rs`:
  `create_flag_refuses_dot_segment_keys`: create an app, then for `"."` and `".."` call
  `create_flag` and assert `err.code() == "invalid_argument"`; then `list_flags(&app, true)` is
  empty.
- [ ] **Step 3: Run** `cargo test -p flags-core dot_segment` (with `DATABASE_URL`) and see both
  fail.
- [ ] **Step 4: Implement**: in `validate_key`, after the charset check, return
  `Error::Invalid(format!("flag key {key:?} is a URL dot segment, which SDKs cannot evaluate over REST; choose another key"))`
  when `key == "." || key == ".."`. Add a short doc comment on `validate_key` giving the reason
  (path segment in `POST /api/flags/{key}/evaluate`, #105).
- [ ] **Step 5: Doc**: `CreateFlagArgs::key` doc becomes "Letters, digits, '-', '_', '.', but not
  '.' or '..' alone; at most 128 characters. Cannot be changed later."
- [ ] **Step 6: Run** the targeted tests (pass), then all gates above.
- [ ] **Step 7: Commit** `fix(core): refuse the flag keys "." and ".."` with a why-body ending
  `Fixes #105`.
