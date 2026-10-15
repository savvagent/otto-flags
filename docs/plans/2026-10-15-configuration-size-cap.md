# Cap configuration size on write Implementation Plan

> Execute task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `create_flag` and `update_flag` refuse a flag or variation configuration whose JSON
exceeds 32 KiB, with `invalid_argument`; stored configurations keep evaluating; the limit is
documented.

**Architecture:** A constant and a byte-counting size check in `crates/flags-core/src/eval.rs`,
called from `validate_variations` and from `validate_configuration` in
`crates/flags-core/src/flags.rs`. Docs on the MCP tool arguments and in the SDK guide. No SQL,
no migration.

**Ticket:** savvagent/otto-flags#115

**Spec:** `docs/specs/2026-10-15-configuration-size-cap-design.md`

## Global Constraints

- Worktree only:
  `/home/robhicks/dev/otto-flags/.claude/worktrees/feat-115-cap-configuration-size`; `main`
  changes only through the PR.
- Every cargo command runs with `CARGO_BUILD_JOBS=3 TMPDIR=$PWD/target/tmp` from the worktree root
  (`mkdir -p target/tmp` first).
- Gates:
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15450/otto_flags cargo test --workspace`
    (this run's dedicated container `otto-flags-pg-15450`; never 15434 or a Fly database)
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'`
    prints nothing
  - `podman build -t otto-flags:check .` (crates changed; once, before the PR)
- Evidence: pass counts per test binary; skipped tests are not a pass.
- Invariants: no migration or constraint; SDK REST contract unchanged; refusal built in code
  before any SQL; rollback is not checked.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #115`. No attribution of any kind.

## Task 1: Size check and its callers

Model: sonnet

Files:
- Modify: `crates/flags-core/src/eval.rs` (constant, `json_len`, `check_configuration_size`,
  `validate_variations`, `mod tests`)
- Modify: `crates/flags-core/src/flags.rs` (`validate_configuration`)
- Modify: `crates/flags-core/tests/db.rs` (new tests at the end)

- [ ] **Step 1: Failing unit tests** in `eval.rs` `mod tests`:
  `json_len_matches_serialized_length` (for `null`, `"é\n\u{1}"`, nested object/array, numbers:
  `json_len(&v) == serde_json::to_vec(&v).unwrap().len()`);
  `variation_configuration_size_is_capped`: a string value `"x".repeat(MAX_CONFIGURATION_BYTES - 2)`
  (serializes to exactly the limit) as `{"on": {"configuration": s}}` is accepted; one more
  character is refused with a message containing `"on"` and `32768`.
- [ ] **Step 2: Failing DB tests** in `tests/db.rs` (helper `sized(n)` = JSON string of `n` total
  serialized bytes):
  - `configuration_size_is_capped_on_write`: `create_flag` with `configuration: sized(32768)`
    succeeds; `sized(32769)` fails `invalid_argument`; `update_flag` with
    `configuration: sized(32769)` fails, `sized(32768)` succeeds; `update_flag` with
    `variations: {"on": {"configuration": sized(32769)}}` fails, and `create_flag` with the same
    variations fails.
  - `stored_oversized_configuration_still_evaluates`: create a flag, then on the pool
    `UPDATE feature_flags SET configuration = $1 WHERE id = $2` with `sized(40_000)`;
    `find_flag` + `eval::evaluate` in an enabled environment returns that configuration;
    `update_flag` with only `name` succeeds and keeps it.
- [ ] **Step 3: Run** `cargo test -p flags-core size` (with `DATABASE_URL`) and see them fail
  (compile failure counts for the unit tests).
- [ ] **Step 4: Implement** per spec section 6 (`MAX_CONFIGURATION_BYTES`, `json_len` via a
  counting writer, failing closed to `usize::MAX`, `check_configuration_size`, the loop in
  `validate_variations` after `MAX_VARIATIONS`, the call in `validate_configuration` after the NUL
  check). Doc comments give the reason (#115: every evaluate response carries it; SDKs cap reads).
- [ ] **Step 5: Run** the targeted tests (pass), then lint and format.
- [ ] **Step 6: Commit** `feat(core): cap a flag configuration at 32 KiB on write` with a
  why-body ending `Fixes #115`.

## Task 2: Document the limit

Model: haiku

Files:
- Modify: `crates/flags-mcp/src/tools/flags.rs` (`CreateFlagArgs`/`UpdateFlagArgs`
  `variations` and `configuration` doc comments)
- Modify: `docs/SDK-DEVELOPER-GUIDE.md` (Flag Evaluation, after the key paragraph)

- [ ] **Step 1: Docs**: each configuration doc says "At most 32 KiB as JSON."; each variations doc
  says "Each variation's configuration is at most 32 KiB as JSON." The guide gains: a flag's or
  variation's `configuration` is at most 32 KiB (32 768 bytes) of JSON when written; flags
  stored before the limit may carry more; size the response read cap well above it (the Rust SDK
  reads up to 1 MiB).
- [ ] **Step 2: Run** all gates above.
- [ ] **Step 3: Commit** `docs: document the 32 KiB configuration limit` ending `Fixes #115`.
