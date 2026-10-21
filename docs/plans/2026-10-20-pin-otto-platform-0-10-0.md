# Pin otto-platform 0.10.0 Implementation Plan

**Goal:** Pin `otto-tenant` and `otto-resource` at otto-platform v0.10.0, with no behavior change
(savvagent/otto-flags#140, PR 1 of 2).

**Global constraints:**
- **Gates** (from CLAUDE.md):
  - `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo fmt --all -- --check`
  - the extra check: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'` must print nothing
  - `podman build -t otto-flags:check .`
- **Evidence:** pass counts per test binary.
- **Invariants:** the otto-platform pin, append-only migrations, and metering.
- **Process:** worktree only; trunk only via PR; no attribution of any kind; no migration is added
  or modified.

### Task 1: Bump the pin

**Model:** haiku

**Files:** `Cargo.toml`, `Cargo.lock`, `crates/flags-core/src/usage.rs`

- [ ] Set both `rev` values to `bfda55e155be8985761850a7c42980cc926fd4cb`.
- [ ] Run `cargo build --workspace --all-targets`. It fails on the `UsageEvent` literal in
  `ship_once` (missing `meter`, `quantity` and `service`).
- [ ] Add `service: None, meter: None, quantity: None` to that literal, then rebuild.
- [ ] Confirm that `git diff Cargo.lock` changes only the two platform packages, plus their
  `hmac`/`sha2` edges.
- [ ] Run the gates.
- [ ] Run the extra migrations check after committing (it compares committed history).
- [ ] Commit `chore: pin otto-platform 0.10.0 for service meters`, with a body ending
  `Refs #140` (#140 closes with the metering PR).
- [ ] Open the PR with `Refs #140` in its body, not `Fixes #140`, and say why (see the spec's
  Brief).
- [ ] Before merging, add a final docs commit: the spec's `**Status:**` becomes
  `IMPLEMENTED (#<PR>)` and this plan's boxes are ticked.
