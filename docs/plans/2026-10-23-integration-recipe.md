# Integration recipe in sdk_guide Implementation Plan

**Goal:** every `sdk_guide` result starts with an end-to-end recipe; `{{ENVIRONMENTS}}`; the only app is the default (#158).
**Spec:** docs/specs/2026-10-23-integration-recipe-design.md

## Global Constraints

- Gates: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`,
  `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`, the
  migrations diff, `podman build -t otto-flags:check .`.
- No registry or publication wording (the recipe is tested too); no server key in any rendering;
  worktree only, trunk only via PR; no attribution of any kind.

### Task 1: Recipe content and rendering

**Model:** sonnet **Files:** `crates/flags-mcp/content/recipe*.md`, `crates/flags-mcp/src/guide.rs`

- [x] Failing unit tests (recipe first and once, key steps, environments); add content, `server_key`, `{{ENVIRONMENTS}}`, `render`; `cargo test -p flags-mcp`.

### Task 2: Guides

**Model:** haiku **Files:** `crates/flags-mcp/content/guides/*.md`

- [x] Environments note before `## Evaluate a flag` in all 17 guides.

### Task 3: Tool and instructions

**Model:** sonnet **Files:** `crates/flags-mcp/src/tools/guide.rs`, `tools/out.rs`, `server.rs`, `crates/flags-server/tests/e2e.rs`

- [x] Failing e2e (none/one/several apps); default or list in the tool; instructions; update the existing e2e.

### Task 4: Docs and pointers

**Model:** haiku **Files:** `AGENTS.md`, `llms.txt`, `crates/flags-mcp/content/llms.txt`, `docs/SDK-DEVELOPER-GUIDE.md`

- [x] Point at the recipe; document what is billed.

### Task 5: Gates, commit

- [x] Run every gate; commit `feat(mcp): give sdk_guide an end-to-end recipe, key handoff and environments`, body why, `Fixes #158`.
