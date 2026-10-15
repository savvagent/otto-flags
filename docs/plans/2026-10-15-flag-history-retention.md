# Keep a flag's newest 100 versions Implementation Plan

> Execute task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** every flag write prunes the flag's history to its newest 100 versions;
`rollback_flag` and `flag_history` keep working and say where the bound is.

**Architecture:** A constant and one `DELETE` in `record_version`
(`crates/flags-core/src/flags.rs`), in the writer's transaction. Message and tool-description
updates. No migration, no background job.

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
    prints nothing
  - `podman build -t otto-flags:check .` (crates changed; once, before the PR)
- Evidence: pass counts per test binary; skipped tests are not a pass.
- Invariants: no migration; every change still records exactly one version; the prune is a
  tenant query in the caller's `Tx` with `org_id` in its predicate; no SQL outside
  `flags-core`; runtime `sqlx::query` only; database error text never reaches a tool response.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Refs #123` (part 2 closes the issue; this PR's body says `Refs #123`, not `Fixes`, so the
  issue stays open). No attribution of any kind.
- Reviews: rust-pro, architect, blind security, plus database-optimizer (SQL changes).

## Task 1: Prune history on write

Model: sonnet

Files:
- Modify: `crates/flags-core/src/flags.rs` (module docs, `MAX_RETAINED_VERSIONS`,
  `record_version`)
- Modify: `crates/flags-core/src/error.rs` (`VersionNotFound` message)
- Modify: `crates/flags-core/src/audit.rs` (module doc: the flag-change trail is the newest 100
  versions)
- Modify: `crates/flags-mcp/src/tools/insight.rs` (`flag_history` description),
  `crates/flags-mcp/src/tools/flags.rs` (`rollback_flag` description)
- Modify: `crates/flags-core/tests/db.rs` (new test at the end)

- [ ] **Step 1: Failing DB test** `flag_history_keeps_the_newest_100_versions` at the end of
  `crates/flags-core/tests/db.rs`: create an app (`production`) and flag `f`; make 104
  `update_flag` changes (name `n{i}`) so the flag is at version 105. Assert: `flag_versions`
  holds 100 rows for it, `min(version) = 6`, `max = 105`; `flag_history(&flag, 200)` returns 100
  rows, first `105`, last `6`. `rollback_flag(Some(5))` fails with code `version_not_found` and a
  message containing `100`; `rollback_flag(Some(6))` succeeds, returns `(flag, 6)` with
  `flag.version == 106` and `flag.name == "n5"` (version 6's name), and afterwards the
  versions are `7..=106` (100 rows); a second flag `g` with 3 versions keeps all 3; `rollback_flag(None)` then succeeds (to 105, as version 107; versions `8..=107` remain).
- [ ] **Step 2: Run, see it fail** (101+ rows):
  `DATABASE_URL=postgres://flags:flags@localhost:15452/otto_flags cargo test -p flags-core --test db flag_history_keeps`
- [ ] **Step 3: Implement.** In `flags.rs`: `pub const MAX_RETAINED_VERSIONS: i32 = 100;` with
  the reasoning as its doc comment (count not age; pruned in the writer's transaction; legacy
  histories pruned on their next write); in `record_version`, after the `INSERT`, run
  `DELETE FROM flag_versions WHERE org_id = $1 AND flag_id = $2 AND version <= $3 - $4` bound
  with the org, the flag id, `flag.version`, and `MAX_RETAINED_VERSIONS`. Update the module docs
  (line 3-8) to say the newest 100 versions are kept. In `error.rs`: `VersionNotFound` message
  `"flag {key:?} has no version {version}; only the newest {} versions of a flag are kept. Call
  flag_history to see them"` with `crate::flags::MAX_RETAINED_VERSIONS`. Tool descriptions:
  `flag_history` "The newest versions of a flag (the newest 100 are kept), newest first: ...";
  `rollback_flag` "... flag_history shows the versions it can restore."
- [ ] **Step 4: Run, see it pass**, then the whole flags-core DB binary and the e2e binary.
- [ ] **Step 5: Commit** `feat(core): keep a flag's newest 100 versions` with a why-body ending
  `Refs #123`.

## Task 2: Full gates

Model: haiku

- [ ] Run test, lint, format, extra, and `podman build` gates; record per-binary pass counts and
  `git rev-parse HEAD`.
