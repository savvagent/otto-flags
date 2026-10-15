# Rust SDK lint gates Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** `packages/rust-server` passes `cargo fmt --check` and clippy `-D warnings` on the lib
and `tests/dynamic_config_test.rs`, without changing what `set_config_override(.., None)` does.

**Architecture:** Manual `Default` for `ConfigOverrideOptions` delegating to `new()`, then
clippy's `unwrap_or_default()`; delete the dead `Cache::get`; rustfmt the crate.

**Tech Stack:** Rust (stable, edition 2021), cargo fmt, clippy.

**Ticket:** #15

**Spec:** `docs/specs/2026-10-15-rust-sdk-lint-design.md`

## Global Constraints

- Gates, run in `packages/rust-server` with `CARGO_BUILD_JOBS=3` and `TMPDIR=$PWD/target/tmp`
  (create it first):
  - `cargo fmt --check` (must exit 0)
  - `cargo clippy --locked --lib --test dynamic_config_test -- -D warnings` (must exit 0)
  - `cargo clippy --locked --all-targets --keep-going -- -D warnings` (known red, #4: only
    `integration_test` and `override_test` may fail; record any lints in other targets)
  - `cargo test --locked --lib --test dynamic_config_test` (must pass, nothing ignored that was
    not ignored before)
  - `cargo test --locked` (known red, #4: no new failures against `origin/main`)
- Evidence: pass/fail/ignored counts per test binary; skipped tests are not a pass.
- Invariants: SDK REST contract unchanged; SDKs never throw into the host app; MIT package.
- No `#[allow(...)]` to pass a gate. No version bump, no changeset.
- Worktree only; trunk changes only through the PR.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, why-body ending `Fixes #15`; no
  attribution of any kind (no Co-Authored-By, no "Generated with", no credit markers).

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `packages/rust-server/src/types.rs` | Manual `Default` for `ConfigOverrideOptions`; unit test |
| **Modify.** `packages/rust-server/src/client.rs` | `unwrap_or_default()`; rustfmt |
| **Modify.** `packages/rust-server/src/cache.rs` | Remove dead `Cache::get` |
| **Modify.** `packages/rust-server/tests/*.rs` | rustfmt; unused import in `dynamic_config_test.rs` |
| **Modify.** `CLAUDE.md` | Known-red notes for the Rust SDK |

## Task Order & Rationale

One task: the edits are small and only meaningful together (the clippy fix is unsafe without the
`Default` fix).

---

### Task 1: Rust SDK lint fixes

**Files:**
- Modify: `packages/rust-server/src/types.rs`, `src/client.rs`, `src/cache.rs`, `tests/*.rs`
- Modify: `CLAUDE.md` (Gates > known red)

**Out-of-band / invariants:** none out-of-band; SDK lookup path behavior unchanged.

**Model:** haiku — mechanical, fully specified.

- [x] **Step 1: Write the failing test** at the end of `src/types.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::ConfigOverrideOptions;

    // `set_config_override(.., None)` uses `unwrap_or_default()`, so `default()` must keep
    // validation on, matching `new()` and the field docs (#15).
    #[test]
    fn config_override_options_default_validates() {
        let opts = ConfigOverrideOptions::default();
        assert!(opts.validate);
        assert!(!opts.merge);
    }
}
```

- [x] **Step 2: Run it, see it fail** (in `packages/rust-server`, with
  `CARGO_BUILD_JOBS=3 TMPDIR=$PWD/target/tmp`):
  `cargo test --locked --lib config_override_options_default_validates`
  → FAIL (derived `Default` gives `validate: false`).
- [x] **Step 3: Implement:** in `src/types.rs` change the derive to `#[derive(Debug, Clone)]` and
  add, after the struct:

```rust
impl Default for ConfigOverrideOptions {
    fn default() -> Self {
        Self::new()
    }
}
```

  In `src/client.rs` `set_config_override`: `let opts = options.unwrap_or_default();`.
  In `src/cache.rs`: delete `Cache::get` and its doc comment.
  In `tests/dynamic_config_test.rs`: drop the unused `Context` from the `otto_flags` import.
- [x] **Step 4: Run it, see it pass**, then `cargo fmt` and every gate in Global Constraints.
- [x] **Step 5: `CLAUDE.md`:** replace the two Rust SDK known-red lines with this line verbatim:

```
- known red: Rust SDK `cargo clippy --all-targets` and `cargo test` (two integration test files do not compile) — #4
```
- [x] **Step 6: Commit:** `fix(sdk): make the rust sdk pass rustfmt and clippy`, body explaining
  the `Default` trap and the `#4` split, ending `Fixes #15`.

Before merge (ship phase): the spec's `**Status:**` becomes `IMPLEMENTED (#<PR>)` and these boxes
are ticked, in the same PR.
