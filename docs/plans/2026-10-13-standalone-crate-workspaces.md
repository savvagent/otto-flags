# Standalone Rust crates as their own workspace roots

**Issue:** #14. **Fast-path:** no spec (two manifests that mirror each other plus the gate text in
`CLAUDE.md`; no public interface, dependency, migration, or server change).

**Goal:** `packages/rust-server` and `examples/rust-server` build in place inside a git worktree
under `.claude/worktrees/`, so the Rust SDK gates in `CLAUDE.md` drop the temp-copy workaround.

## Global constraints

- Worktree only; `main` changes only through a PR. No attribution of any kind.
- Gates (`CLAUDE.md` > Gates): `cargo test --workspace` (with the local `DATABASE_URL`, serialized
  across worktrees), `cargo clippy --workspace --all-targets -- -D warnings`,
  `cargo fmt --all -- --check`, the migrations `extra` check, `podman build` (not run: the root
  `Cargo.toml` is untouched, and `.dockerignore` excludes `packages` and `examples`, so the image
  cannot change), and the `packages/rust-server` and
  `examples/rust-server` gates, which are known red (#15, #4, #16): pass means no new failures
  against an `origin/main` baseline.
- Do not bump versions; nothing is published.

### Task 1: empty `[workspace]` tables and in-place gates

- [ ] **Failing check first:** in the worktree, `cd packages/rust-server && cargo locate-project
  --workspace` (and the same in `examples/rust-server`) fails with cargo's "current package
  believes it's in a workspace when it's not".
- [ ] Add an empty `[workspace]` table, with a comment saying why, to
  `packages/rust-server/Cargo.toml` and `examples/rust-server/Cargo.toml`.
- [ ] Re-run: each prints its own manifest; at the repo root it still prints the root manifest and
  `cargo metadata --no-deps` lists only the four `crates/flags-*` members.
- [ ] `CLAUDE.md`: the five Rust SDK/example gates run in place (`cd <crate> && cargo …`); drop the
  sentence explaining the temp copy and #14; the Notes line says the crates are their own
  workspace roots.
- [ ] Run every gate above; compare the known-red gates with the `origin/main` baseline (run
  there from a temp copy, since the baseline lacks the fix).
- [ ] Commit: `fix: make the standalone rust crates their own workspace roots`, body explaining
  why, ending `Fixes #14`.
