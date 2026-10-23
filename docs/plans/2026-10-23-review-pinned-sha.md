# Review the pinned SHA in the SDK guides Implementation Plan

**Goal:** guides, `AGENTS.md` and `llms.txt` tell agents to have the SHA reviewed; JS builds skip install scripts (#156).
**Spec:** docs/specs/2026-10-23-review-pinned-sha-design.md

## Global Constraints

- Gates: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`,
  `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`, the
  migrations diff, `podman build -t otto-flags:check .`.
- No registry or publication wording; worktree only, trunk only via PR; no attribution of any kind.

### Task 1: Verify --ignore-scripts

**Model:** haiku

- [x] Fresh clone: `pnpm install --frozen-lockfile --ignore-scripts`; build and pack the eleven packages.

### Task 2: Content and tests

**Model:** sonnet **Files:** `crates/flags-mcp/content/guides/*.md`, `crates/flags-mcp/content/llms.txt`, `llms.txt`, `AGENTS.md`, `crates/flags-mcp/src/guide.rs`

- [x] Add the three tests (fail), then add the sentence and the JS paragraph and flag; `cargo test -p flags-mcp` passes.

### Task 3: Gates, commit

- [x] Run every gate; commit `docs(mcp): tell agents to review the pinned SHA and skip install scripts`, body why, `Fixes #156`.
