# Pin otto-platform 0.5.0 Implementation Plan

**Goal:** Pin `otto-tenant` and `otto-resource` at otto-platform 0.5.0 (savvagent/otto-flags#25).

**Global constraints:** gates from CLAUDE.md (test with `DATABASE_URL`, clippy, fmt, extra,
`podman build -t otto-flags:check .`); worktree only, trunk only via PR; no attribution of any kind;
no migration is added or modified.

### Task 1: Bump the pin

**Model:** haiku

**Files:** `Cargo.toml`, `Cargo.lock`, `crates/flags-server/tests/e2e.rs`

- [ ] Set both `rev` values to `fa1ae08c8d5760c1df9752281a078192032e7d24`.
- [ ] `cargo update -p otto-tenant -p otto-resource`; confirm only those two packages changed.
- [ ] `cargo build --workspace --all-targets`; see it fail on the `UsageStatus` literal in
  `usage_status`.
- [ ] Add `features: Default::default(),`; rebuild.
- [ ] Run the gates.
- [ ] Commit `build: pin otto-platform 0.5.0 for plan features`, body ending `Fixes #25`.
