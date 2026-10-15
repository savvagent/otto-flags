# Rust SDK integration tests compile and run, on the documented evaluate contract

**Status:** IMPLEMENTED (#94)
**Issue:** #4

## 1. Brief

`cargo test` in `packages/rust-server` fails to build two of its three integration test files:

| Test file | Compile errors | Cause |
|---|---|---|
| `tests/integration_test.rs` | 109 | Written against an older `Config` (`api_url`, `sdk_key`, `environment`, `enable_websocket`, `polling_interval`); `Context` initializers miss fields added since; methods that now return `bool` are `.unwrap()`ed |
| `tests/override_test.rs` | 1 | Imports `otto_flags::ConfigOverrideOptions`, which the crate does not export |
| `tests/dynamic_config_test.rs` | 0 | Builds; 6 tests pass |

Also to check: the SDK evaluates via `POST /api/evaluate/{key}`, while every other SDK uses the
documented `POST /api/flags/{key}/evaluate`. Neither CI workflow builds this crate; fixing it
should include a CI job for it.

Decisions from the caller (fixed inputs):

- Port the tests to the current API, keeping their intent; they run offline against a mock server
  (`wiremock`, already a dev-dependency), never against production.
- Move the evaluate call to `POST /api/flags/{key}/evaluate` if `docs/SDK-DEVELOPER-GUIDE.md`
  confirms it is the documented route. It does (Core Endpoints > Flag Evaluation, and the endpoint
  table).
- Export `ConfigOverrideOptions` from `lib.rs` (additive).
- Add a CI job for the crate: `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D
  warnings`, `cargo test --locked`.
- Remove the Rust SDK known-red line (#4) from `CLAUDE.md` > Gates and drop `--keep-going` from the
  clippy gate.
- Public API changes are additive only: `examples/rust-server` (#16, worked concurrently) depends
  on this crate by path.

Acceptance criteria:

- `cargo test --locked` in `packages/rust-server` builds every target and passes, offline.
- `cargo clippy --locked --all-targets -- -D warnings` and `cargo fmt --check` pass.
- The SDK calls the documented evaluate route.
- A GitHub Actions job runs the three commands when the crate changes.
- `CLAUDE.md` no longer lists the Rust SDK as known red.

## 2. Premise corrections

- **The SDK reads the wrong response field, so against the real server every flag evaluates to its
  default.** `FlagClient::evaluate` (`src/client.rs`) reads `data["value"]`; the server's
  `EvaluateResponse` (`crates/flags-api/src/sdk.rs:63`) and the SDK guide's response table send
  `enabled`, never `value`. `value` is always absent, so the result falls back to
  `Config::defaults` or `false`, and `is_enabled` never returns `true` for a server-side `true`.
  The existing tests hid this by mocking `{"value": true}`. Porting the tests "to the documented
  contract" means mocking `enabled`, so the SDK must read `enabled`. This is fixed here: it is one
  line on the same code path as the route move, and the ported tests would otherwise assert the
  bug. `EvaluationResult.value` (the public Rust field) keeps its name.
- (Found in review.) The server always sends `"configuration": null` for a flag without one
  (`EvaluateResponse` has no `skip_serializing_if`), and the SDK returned it as `Some(Null)`, so
  `get_config` gave `Some` for every enabled flag without configuration once `enabled` is read.
  `null` is treated as absent (the guide types the field `object?`).
- `get_variation` returns `VariationResult`, which (like `ConfigOverrideOptions`) is public in
  signature but not exported, so callers cannot name the type. It is exported too (additive).
- The old `integration_test.rs` exercised APIs that no longer exist: `track(event, ctx, props)`
  (no event API in the SDK, and the server has no `/v1/events`), `get_variation` with a typed
  default (it now returns `Result<VariationResult>`), and `is_enabled` returning `Result` (it now
  returns `bool`, falling back to the default). The port keeps each test's intent on the current
  API; `test_track_event` has no current equivalent and is dropped (section 5).
- The old tests expected an error from `is_enabled` on HTTP 404/500. The current, documented SDK
  behavior is to fall back to defaults and never throw into the host app (Invariants). The ported
  tests assert the fallback: `is_enabled` returns the configured default, `evaluate` returns
  `Ok` with `reason == "error"`.

## 3. Assumptions

- Every Run Profile value comes from the Development Workflow section; none assumed.
- The CI job is a new workflow, `.github/workflows/rust-sdk.yml`, path-filtered to
  `packages/rust-server/**` and the workflow file itself, rather than a job in `sdks.yml`: `sdks.yml`
  runs on every JS/TS change and its `Test` job is named in the merge gate; a separate workflow
  keeps Rust builds off JS-only PRs. #16 can extend the same workflow for `examples/rust-server`.
- The SDK reads `enabled` only, with no fallback to `value`: no otto-flags server sends `value`,
  and the retired API the old field matched is gone.
- `/api/evaluate/{key}` stays on the server (the route alias is server code under `crates/`, and
  already-built copies of the SDK may call it). Removing it is out of scope.
- `tests/dynamic_config_test.rs` mocks move to the documented route and `enabled` field too, or
  they would stop matching the SDK's requests.
- No version bump, no changeset (not a pnpm package; not published).

## 4. Goal and success criteria

The Rust SDK's whole test suite builds and runs offline in CI, against the documented wire
contract.

- `cargo test --locked` passes: lib unit tests, `dynamic_config_test`, `integration_test`,
  `override_test`, with no `#[ignore]`.
- A ported test fails if the SDK reads `value` instead of `enabled`, or calls `/api/evaluate/{key}`
  (wiremock returns 404 for unmatched routes, which the SDK turns into the default).
- `.github/workflows/rust-sdk.yml` (job `Rust SDK`, `working-directory: packages/rust-server`,
  `dtolnay/rust-toolchain@stable` with `rustfmt` and `clippy`; the root `rust-toolchain.toml` also
  applies there) runs fmt, clippy, and test, all `--locked` where cargo accepts it, on PRs and
  pushes touching the crate or the workflow.
- `CLAUDE.md` > Ship > merge gate names the new check: `Rust SDK` succeeded when any changed file
  matches `packages/rust-server/**` or `.github/workflows/rust-sdk.yml`.
- `CLAUDE.md` > Gates has no Rust SDK known-red line and no `--keep-going`.

## 5. Scope

In: `src/lib.rs` (exports), `src/client.rs` (route, `enabled`), the three `tests/*.rs`, the new
workflow, `CLAUDE.md` (Gates, Ship > merge gate, Notes on CI), README if it names the route or
the field.

Also in (from review): the null-configuration fix above, extra fallback and cache tests, the
workflow trigger on the root `rust-toolchain.toml`, and a README caveat about the cache (#91).

Out: the SDK's cache keying (it caches per flag key, ignoring the context, so targeted results
leak between users within the TTL; #91); telemetry/event tracking (the SDK has none;
`test_track_event` is dropped, not reimplemented); removing the server's `/api/evaluate/{key}`
alias; URL-encoding the flag key (#95); transport-error fallback in `get_config`/`get_variation`
and the always-empty `Metadata` (#96); `examples/rust-server` (#16); publishing.

## 6. Architecture

- `src/lib.rs`: `pub use types::{Config, ConfigOverrideOptions, Context, EvaluationResult,
  Metadata, VariationResult};` Additive.
- `src/client.rs` `evaluate`: URL `"{base_url}/api/flags/{flag_key}/evaluate"`; value from
  `data["enabled"].as_bool()`, then defaults, then `false`. Request body (`{"context": ...}`) and
  `Authorization: Bearer` header unchanged; both match the guide and `presented_key`
  (`crates/flags-api/src/sdk.rs:28`).
- Public interface: two new re-exports; no signature, field, or type change. The wire change is
  the request path (both paths are served) and reading the documented response field.

## 7. Invariants impact

- **SDK REST contract is stable**: the SDK moves onto the documented contract; the server is
  untouched.
- **SDKs never throw into the host app on a flag lookup**: unchanged; the ported tests pin the
  fallback on 404 and 500.
- **The SDK path never calls the platform** / **client keys are public**: untouched.
- **Licensing**: MIT package; no server code copied (tests mock JSON shapes from the guide).
- Server invariants (migrations, RLS, SQL, MCP auth, metering): no file under `crates/`.

## 8. Migration

None.

## 9. Out-of-band

None. The merge redeploys unchanged server code: verification is health only, and the changed
surface (an unpublished SDK) is not deployed. Publishing is Rob's and not requested.

## 10. Error handling and edge cases

- Non-2xx (404 unknown flag, 401 bad key, 500): `evaluate` returns `Ok` with the default value and
  `reason: "error"`; `is_enabled` returns the default. Unchanged behavior, now tested.
- Response without `enabled` (malformed): falls back to the default, as before for `value`.
- Network error: `evaluate` returns `Err(RequestFailed)`, `is_enabled` returns the default.
  Unchanged; tested by pointing the client at a closed port is out of scope (timing-dependent).

## 11. Testing approach

All in `packages/rust-server/tests/`, against `wiremock::MockServer` with `Config::new("sdk_…")
.with_base_url(mock.uri())`:

- `integration_test.rs` (ported): `is_enabled` true and false from `enabled`; the request carries
  `Authorization: Bearer <key>` and the context (`body_partial_json` with `user_id` and
  attributes); 404 and 500 fall back to the configured default (`with_default`) and `evaluate`
  reports `reason == "error"`; `get_variation` returns the server's variation and configuration,
  and `"control"` on error; `Config` and `Context` builders set their fields (pure tests).
- `override_test.rs`: unchanged logic; mocks move to the documented route and `enabled`.
- `dynamic_config_test.rs`: mocks move to the documented route and `enabled`.
- Tests do not assert on `Metadata` (`flag_id`, `description`, `variant`): the server sends none of
  those fields. Each test uses a fresh client, so the per-key cache never hides a second mock;
  error results are not cached (`get_default_result` returns before `cache.set`).
- Mocks match the exact route with `path(...)` so a regression to the old route or field fails a
  test (the mock server 404s unmatched requests and the SDK returns the default `false`).

Gates (`packages/rust-server`, `CARGO_BUILD_JOBS=3`, `TMPDIR=$PWD/target/tmp`): `cargo fmt
--check`; `cargo clippy --locked --all-targets -- -D warnings`; `cargo test --locked`. Baseline on
`origin/main` (44fb4e2): lib 1 passed, `dynamic_config_test` 6 passed, `integration_test` (109
errors) and `override_test` (1 error) do not compile. The `examples/rust-server` gates are
known red (#16) and must show no new failures. `podman build` does not apply (`.dockerignore`
excludes `packages`; no `crates/` or root manifest change). Root `cargo fmt`/`clippy` touch no
changed file but run anyway; the root DB test gate needs no run (no `crates/` change; CI's `rust`
job covers it).

## 12. Risks and open questions

- `examples/rust-server` (#16) may read `EvaluationResult.value`; it keeps its name, so the port
  is unaffected. If #16 merges first and adds jobs to a workflow, rebasing reconciles them.
- Reading `enabled` changes runtime results for anyone running this SDK against otto-flags: flags
  the server enables now evaluate `true`. That is the fix, but it is a behavior change; the PR
  calls it out.
