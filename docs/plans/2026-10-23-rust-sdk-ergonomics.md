# Rust SDK ergonomics Implementation Plan

**Goal:** env-derived defaults, a sync cached read and a static client in the Rust SDK (#159).
**Spec:** docs/specs/2026-10-23-rust-sdk-ergonomics-design.md

## Global Constraints

- Gates: in `packages/rust-server` and `examples/rust-server`: `cargo fmt --check`,
  `cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked`; workspace: test (with
  `DATABASE_URL`), clippy, fmt, migrations diff, `podman build -t otto-flags:check .`.
- Additive only; no version bump; worktree only, trunk only via PR; no attribution of any kind.

### Task 1: Config defaults

**Model:** sonnet **Files:** `packages/rust-server/src/types.rs`, `tests/ergonomics_test.rs`

- [x] Failing tests for `with_default_env` / `with_default_fn`, then implement; run `cargo test --locked`.

### Task 2: Sync cached read and static client

**Model:** sonnet **Files:** `packages/rust-server/src/client.rs`, `tests/ergonomics_test.rs`

- [x] Failing tests for `is_enabled_cached` and `static_flags`; implement (extract `apply_overrides`); run `cargo test --locked`.

### Task 3: Docs and guide

**Model:** haiku **Files:** `packages/rust-server/README.md`, `crates/flags-mcp/content/guides/rust.md`

- [x] README entries and example; guide mentions; run the workspace tests (guide hygiene).

### Task 4: Gates, commit

- [x] Run every gate; commit `feat(sdk): add env defaults, a sync cached read and a static client to the Rust SDK`, body why, `Fixes #159`.
