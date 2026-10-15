# Rust SDK passes rustfmt and clippy on its library

**Status:** IMPLEMENTED (#86)
**Issue:** #15

## 1. Brief

`packages/rust-server` (the `otto-flags` crate) fails `cargo fmt --check` (diffs in
`src/client.rs`, `tests/dynamic_config_test.rs`, `tests/integration_test.rs`,
`tests/override_test.rs`) and `cargo clippy -D warnings` (two lib errors: `Cache::get` is dead
code, and `options.unwrap_or_else(ConfigOverrideOptions::new)` trips `clippy::unwrap_or_default`).

Acceptance criteria (#15):

- `cargo fmt --check` passes in `packages/rust-server`.
- `cargo clippy --locked --lib --tests -- -D warnings` passes for the lib and
  `tests/dynamic_config_test.rs`.
  (The commands actually run, while #4's two files do not compile, are in section 2.)
- No `#[allow(...)]` added just to pass the gate.
- `set_config_override(key, cfg, None)` still validates the config
  (`ConfigOverrideOptions::default().validate == true`), and a unit test pins this.
- No public API change.

Out of scope (#15): making `tests/integration_test.rs` and `tests/override_test.rs` compile (#4);
a CI job for the crate (#4).

Open item decided by the caller: format the whole crate, including `tests/integration_test.rs`
and `tests/override_test.rs`, so `cargo fmt --check` passes; #4 rewrites those files after this
merges.

## 2. Premise corrections

- The issue calls the cache type `FlagCache`; it is `Cache` in `src/cache.rs`, declared
  `pub(crate)` in a private module, so removing its `get` method is internal.
- `ConfigOverrideOptions` is not re-exported from `src/lib.rs` (`mod types` is private; only
  `Config`, `Context`, `EvaluationResult`, `Metadata` are). Outside the crate it is unnameable, so
  external callers of `FlagClient::set_config_override` can pass only `None`. Changing its
  `Default` impl therefore changes nothing observable outside the crate; inside, it makes
  `default()` agree with `new()` and with the field doc ("default: true"). (That
  `tests/override_test.rs` imports it from the crate root is one reason that file fails to
  compile; #4.)
- `cargo clippy --lib --tests` also builds `tests/integration_test.rs` and
  `tests/override_test.rs`, which do not compile (#4). The AC scopes clippy to the lib and
  `dynamic_config_test`, so the gate run here is
  `cargo clippy --locked --all-targets --keep-going -- -D warnings`, passing when the only failing
  targets are those two (#4), plus `cargo clippy --locked --lib --test dynamic_config_test -- -D
  warnings` exiting 0. (The crate has no benches or examples, so `--all-targets` and
  `--lib --tests` cover the same targets.)
- The issue lists only the two lib errors because clippy stopped there. On `origin/main`,
  `tests/dynamic_config_test.rs` also has an unused import (`Context`), which `-D warnings` turns
  into an error once the lib builds; it is removed here.

## 3. Assumptions

- Every Run Profile value comes from the Development Workflow section; none assumed.
- The unit test lives beside the code (`#[cfg(test)] mod tests` in `src/types.rs`), per Notes >
  Test placement ("pure logic as unit tests beside the code").
- `Cache::get` is removed rather than used: every caller reads `get_entry`, and nothing needs a
  bool-only accessor.
- No version bump and no changeset: the crate is not a pnpm package and is not published.

## 4. Goal and success criteria

The Rust SDK's lint gates are green for everything outside #4's two broken test files.

- `cargo fmt --check` exits 0 in `packages/rust-server`.
- `cargo clippy --locked --lib --test dynamic_config_test -- -D warnings` exits 0.
- `ConfigOverrideOptions::default()` equals `new()` field by field, pinned by a unit test.
- `cargo test --locked --lib` and `--test dynamic_config_test` pass.
- `CLAUDE.md` > Gates known-red notes say what is still red for the Rust SDK: the two lines
  become one, ``known red: Rust SDK `cargo clippy --all-targets` and `cargo test` (two
  integration test files do not compile) — #4``.

## 5. Scope

In: `src/cache.rs` (remove `get`), `src/types.rs` (manual `Default`, unit test), `src/client.rs`
(`unwrap_or_default()`, formatting), the unused `Context` import in
`tests/dynamic_config_test.rs`, rustfmt over `tests/*.rs`, `CLAUDE.md` known-red lines.

Out: compiling `integration_test.rs`/`override_test.rs`, re-exporting `ConfigOverrideOptions`,
CI for the crate (all #4), `examples/rust-server` (#16).

## 6. Architecture

- `src/types.rs`: replace `#[derive(Debug, Clone, Default)]` on `ConfigOverrideOptions` with
  `#[derive(Debug, Clone)]` plus `impl Default for ConfigOverrideOptions { fn default() -> Self {
  Self::new() } }`.
- `src/client.rs` `set_config_override`: `options.unwrap_or_default()`.
- `src/cache.rs`: delete `Cache::get`.
- Formatting only elsewhere. Public interface: unchanged (no exported signature or type changes).

## 7. Invariants impact

- SDK REST contract and "SDKs never throw into the host app": untouched; no wire or lookup-path
  change.
- Licensing: MIT package, no server code copied.
- Server invariants (migrations, RLS, SQL, auth): not touched; no file under `crates/`.

## 8. Migration

None.

## 9. Out-of-band

None. Merge redeploys unchanged server code; verification is health only, and the changed
surface (an unpublished SDK) is not deployed. Publishing is Rob's and not requested.

## 10. Error handling and edge cases

`set_config_override(..., None)` keeps `merge: false, validate: true`, as before. An explicit
`Some(ConfigOverrideOptions::default())` (only possible inside the crate) now validates, which
matches its documented default.

## 11. Testing approach

- New unit test in `src/types.rs`: `ConfigOverrideOptions::default()` has `validate == true` and
  `merge == false`. Fails before the `Default` change (derive gives `validate: false`).
- Gates (`packages/rust-server`, `CARGO_BUILD_JOBS=3`, `TMPDIR=$PWD/target/tmp`): `cargo fmt
  --check`; `cargo clippy --locked --lib --test dynamic_config_test -- -D warnings`; `cargo clippy
  --locked --all-targets --keep-going -- -D warnings` (known red: only #4's two files may fail);
  `cargo test --locked --lib --test dynamic_config_test`; `cargo test --locked` (known red, #4:
  no new failures against `origin/main`). Baseline on `origin/main`, taken before the change:
  `--lib` 0 tests, `dynamic_config_test` 6 passed, `integration_test` (109 errors) and
  `override_test` (1 error) do not compile; clippy fails on the lib's two errors.
  `podman build` does not apply (`.dockerignore` excludes `packages`; nothing under `crates/`,
  `Cargo.toml`, or `Cargo.lock`). The root `cargo fmt --all -- --check` and `cargo clippy
  --workspace` run locally; the root `cargo test --workspace` needs Postgres and touches no
  changed file, so it is left to CI's `rust` job (merge gate), and the PR says so.

## 12. Risks and open questions

- Formatting `integration_test.rs` and `override_test.rs` may conflict with #4's rewrite; #4 is
  worked after this merges, so it rebases on formatted files.
- `--all-targets` lints in #4's files stay unknown until they compile; reported in the PR.
