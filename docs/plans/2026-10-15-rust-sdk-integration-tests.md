# Rust SDK integration tests Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** Every `packages/rust-server` test target builds and passes offline against mocks of the
documented evaluate contract, and CI runs the crate's fmt, clippy, and tests.

**Architecture:** Export the two public-in-signature types; move `FlagClient::evaluate` to
`POST /api/flags/{key}/evaluate` and read `enabled`; point every mock at that contract; port
`integration_test.rs` to the current API; add `.github/workflows/rust-sdk.yml`; retire the
known-red gate entry.

**Tech Stack:** Rust (stable, edition 2021), reqwest 0.11, tokio, wiremock 0.6, GitHub Actions.

**Ticket:** #4

**Spec:** `docs/specs/2026-10-15-rust-sdk-integration-tests-design.md`

## Global Constraints

- Gates, run in `packages/rust-server` with `export CARGO_BUILD_JOBS=3 TMPDIR=$PWD/target/tmp`
  (`mkdir -p target/tmp` first):
  - `cargo fmt --check`
  - `cargo clippy --locked --all-targets -- -D warnings`
  - `cargo test --locked`
  - after Task 1–2, also, from the repo root, `cd examples/rust-server && cargo fmt --check` and `cargo clippy --locked
    --all-targets -- -D warnings` (known red, #16: no new failures; it does not compile on main)
- Evidence: pass/fail/ignored counts per test binary; skipped or ignored tests are not a pass; no
  test may need network beyond the local wiremock server.
- Invariants: SDK REST contract (the SDK moves onto the documented route and field; server
  untouched); SDKs never throw into the host app on a flag lookup; MIT package, no server code
  copied; public API changes additive only (`examples/rust-server` depends on it by path).
- No `#[allow(...)]` to pass a gate. No version bump, no changeset.
- Worktree only; trunk changes only through the PR.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, why-body ending `Fixes #4`; no
  attribution of any kind (no Co-Authored-By, no "Generated with", no credit markers).

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `packages/rust-server/src/lib.rs` | Re-export `ConfigOverrideOptions`, `VariationResult` |
| **Modify.** `packages/rust-server/src/client.rs` | Documented route; read `enabled` |
| **Modify.** `packages/rust-server/tests/dynamic_config_test.rs` | Mocks on the documented contract |
| **Modify.** `packages/rust-server/tests/override_test.rs` | Mocks on the documented contract |
| **Rewrite.** `packages/rust-server/tests/integration_test.rs` | Ported tests on the current API |
| **Create.** `.github/workflows/rust-sdk.yml` | CI for the crate |
| **Modify.** `CLAUDE.md` | Gates, Ship > merge gate, Notes on CI |

The README names neither the route nor the response field, so it needs no change. Commit steps
run from the repo root.

## Task Order & Rationale

Task 1 changes the wire behavior with its tests (mocks first, so they fail on the old SDK).
Task 2 ports the broken file onto the fixed SDK. Task 3 wires CI and the docs once everything is
green.

---

### Task 1: Documented evaluate contract and exports

**Files:**
- Modify: `packages/rust-server/src/lib.rs`, `packages/rust-server/src/client.rs`
- Test: `packages/rust-server/tests/dynamic_config_test.rs`, `packages/rust-server/tests/override_test.rs`

**Interfaces:** produces `otto_flags::{ConfigOverrideOptions, VariationResult}` (used by Task 2).

**Out-of-band / invariants:** SDK lookup still never panics; non-2xx still returns the default.

**Model:** haiku — mechanical edits, fully specified.

- [ ] **Step 1: Point the mocks at the documented contract.** In both test files, replace every
  `path("/api/evaluate/<key>")` with `path("/api/flags/<key>/evaluate")` (same `<key>`), and in
  every mock response body rename the `"value":` key to `"enabled":` (only the top-level field of
  the `set_body_json(json!({...}))` object; leave configuration contents alone).

- [ ] **Step 2: Run, see it fail.** `cargo test --locked --test dynamic_config_test`
  Expected: FAIL (the SDK still calls `/api/evaluate/...`; wiremock 404s, so values come back
  default `false`/`None`). `override_test` still fails to compile (missing export).

- [ ] **Step 3: Implement.** In `src/lib.rs` replace the `types` re-export with:

```rust
pub use types::{
    Config, ConfigOverrideOptions, Context, EvaluationResult, Metadata, VariationResult,
};
```

In `src/client.rs` `evaluate`, change

```rust
        let url = format!("{}/api/evaluate/{}", self.config.base_url, flag_key);
```
to
```rust
        let url = format!("{}/api/flags/{}/evaluate", self.config.base_url, flag_key);
```
and
```rust
        let value = data["value"]
```
to
```rust
        let value = data["enabled"]
```

- [ ] **Step 4: Run, see it pass.** `cargo test --locked --lib --test dynamic_config_test --test
  override_test` — expected lib 1, dynamic_config_test 6, override_test 8 passed, 0 ignored.
  `cargo clippy --locked --lib --test dynamic_config_test --test override_test -- -D warnings`
  exits 0; `cargo fmt --check` exits 0.

- [ ] **Step 5: Commit.**

```bash
git add packages/rust-server/src packages/rust-server/tests/dynamic_config_test.rs packages/rust-server/tests/override_test.rs
git commit -m "fix(sdk): evaluate rust sdk flags on the documented route and field" -m "The SDK posted to /api/evaluate/{key} and read a 'value' field the server never sends, so every flag evaluated to its default. Use POST /api/flags/{key}/evaluate and read 'enabled', as the SDK developer guide documents, and export ConfigOverrideOptions and VariationResult, which public methods already take and return." -m "Fixes #4"
```

---

### Task 2: Port integration_test.rs to the current API

**Files:**
- Rewrite: `packages/rust-server/tests/integration_test.rs`

**Interfaces:** consumes `otto_flags::{Config, Context, FlagClient, VariationResult}` (Task 1).

**Out-of-band / invariants:** tests assert the never-throw fallback on 404/500.

**Model:** sonnet — test port with some judgment on matchers.

- [ ] **Step 1: Replace the file with:**

```rust
//! The SDK against a mock otto-flags server speaking the documented contract
//! (`docs/SDK-DEVELOPER-GUIDE.md`, Flag Evaluation): `POST /api/flags/{key}/evaluate`
//! with `Authorization: Bearer <key>` and `{"context": ...}`, answered with `enabled`.

use otto_flags::{Config, Context, FlagClient, VariationResult};
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "sdk_test_key";

fn client(server: &MockServer) -> FlagClient {
    FlagClient::new(Config::new(KEY).with_base_url(server.uri())).unwrap()
}

async fn mock_evaluate(server: &MockServer, flag: &str, status: u16, body: serde_json::Value) {
    Mock::given(method("POST"))
        .and(path(format!("/api/flags/{flag}/evaluate")))
        .and(header("Authorization", format!("Bearer {KEY}").as_str()))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

#[tokio::test]
async fn is_enabled_returns_true_when_the_server_enables_the_flag() {
    let server = MockServer::start().await;
    mock_evaluate(&server, "test-flag", 200, json!({ "key": "test-flag", "enabled": true })).await;

    let ctx = Context::new().with_user_id("user-123");
    assert!(client(&server).is_enabled("test-flag", Some(ctx)).await);
}

#[tokio::test]
async fn is_enabled_returns_false_when_the_server_disables_the_flag() {
    let server = MockServer::start().await;
    mock_evaluate(&server, "test-flag", 200, json!({ "key": "test-flag", "enabled": false })).await;

    // A configured default of true proves `false` came from the server, not a fallback.
    let client = FlagClient::new(
        Config::new(KEY)
            .with_base_url(server.uri())
            .with_default("test-flag", true),
    )
    .unwrap();
    let ctx = Context::new().with_user_id("user-123");
    assert!(!client.is_enabled("test-flag", Some(ctx)).await);
}

#[tokio::test]
async fn evaluate_sends_the_context_and_reports_the_server_result() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/flags/test-flag/evaluate"))
        .and(header("Authorization", format!("Bearer {KEY}").as_str()))
        .and(body_partial_json(json!({
            "context": {
                "user_id": "user-123",
                "environment": "staging",
                "attributes": { "email": "test@example.com", "plan": "pro" }
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "key": "test-flag",
            "enabled": true,
            "scope": "application",
            "variation": null,
            "configuration": null,
            "timestamp": 1_700_000_000,
            "version": 3
        })))
        .expect(1)
        .mount(&server)
        .await;

    let ctx = Context::new()
        .with_user_id("user-123")
        .with_environment("staging")
        .with_attribute("email", json!("test@example.com"))
        .with_attribute("plan", json!("pro"));
    let result = client(&server)
        .evaluate("test-flag", Some(ctx))
        .await
        .unwrap();

    assert!(result.value);
    assert_eq!(result.key, "test-flag");
    assert_eq!(result.reason, "evaluated");
}

#[tokio::test]
async fn unknown_flag_falls_back_to_the_default() {
    let server = MockServer::start().await;
    mock_evaluate(
        &server,
        "missing-flag",
        404,
        json!({ "error": "flag_not_found" }),
    )
    .await;

    let client = FlagClient::new(
        Config::new(KEY)
            .with_base_url(server.uri())
            .with_default("missing-flag", true),
    )
    .unwrap();

    let result = client.evaluate("missing-flag", None).await.unwrap();
    assert!(result.value);
    assert_eq!(result.reason, "error");
    assert!(client.is_enabled("missing-flag", None).await);
}

#[tokio::test]
async fn server_error_falls_back_to_the_default() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/flags/test-flag/evaluate"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal server error"))
        .mount(&server)
        .await;

    let client = client(&server);
    let result = client.evaluate("test-flag", None).await.unwrap();
    assert!(!result.value);
    assert_eq!(result.reason, "error");
    assert!(!client.is_enabled("test-flag", None).await);
}

#[tokio::test]
async fn get_variation_returns_the_server_variation() {
    let server = MockServer::start().await;
    mock_evaluate(
        &server,
        "theme",
        200,
        json!({
            "key": "theme",
            "enabled": true,
            "variation": "dark-mode",
            "configuration": { "background": "#000" }
        }),
    )
    .await;

    let VariationResult {
        variation,
        enabled,
        configuration,
    } = client(&server).get_variation("theme", None).await.unwrap();
    assert_eq!(variation, "dark-mode");
    assert!(enabled);
    assert_eq!(configuration.unwrap()["background"], "#000");
}

#[tokio::test]
async fn get_variation_falls_back_to_control_on_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/flags/theme/evaluate"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let result = client(&server).get_variation("theme", None).await.unwrap();
    assert_eq!(result.variation, "control");
    assert!(!result.enabled);
    assert!(result.configuration.is_none());
}

#[test]
fn config_builder_sets_its_fields() {
    let config = Config::new("sdk_key")
        .with_application_id("app-1")
        .with_base_url("https://flags.example.com")
        .with_cache_ttl(std::time::Duration::from_secs(30))
        .with_timeout(std::time::Duration::from_secs(2))
        .with_default("beta", true);

    assert_eq!(config.api_key, "sdk_key");
    assert_eq!(config.application_id.as_deref(), Some("app-1"));
    assert_eq!(config.base_url, "https://flags.example.com");
    assert_eq!(config.cache_ttl, std::time::Duration::from_secs(30));
    assert_eq!(config.timeout, std::time::Duration::from_secs(2));
    assert_eq!(config.defaults.get("beta"), Some(&true));
}

#[test]
fn client_rejects_a_key_without_the_sdk_prefix() {
    assert!(FlagClient::new(Config::new("not-an-sdk-key")).is_err());
}

#[test]
fn context_builder_sets_its_fields() {
    let context = Context::new()
        .with_user_id("user-123")
        .with_session_id("session-1")
        .with_environment("production")
        .with_attribute("email", json!("test@example.com"))
        .with_attribute("plan", json!("pro"))
        .with_attribute("age", json!(30));

    assert_eq!(context.user_id.as_deref(), Some("user-123"));
    assert_eq!(context.session_id.as_deref(), Some("session-1"));
    assert_eq!(context.environment.as_deref(), Some("production"));
    let attributes = context.attributes.unwrap();
    assert_eq!(attributes["email"], "test@example.com");
    assert_eq!(attributes["plan"], "pro");
    assert_eq!(attributes["age"], 30);
}
```

Then run `cargo fmt` (the file must match rustfmt output).

- [ ] **Step 2: Run, see it pass.** `cargo test --locked --test integration_test` — expected 10
  passed, 0 ignored. (Its tests cannot fail first: before Task 1 the file does not compile, and the
  regression they guard is proven by Step 3.)

- [ ] **Step 3: Prove the guards.** Temporarily revert `"enabled"` to `"value"` in
  `src/client.rs`: `is_enabled_returns_true_when_the_server_enables_the_flag`,
  `evaluate_sends_the_context_and_reports_the_server_result`, and
  `get_variation_returns_the_server_variation` must fail. Restore it. Temporarily revert the URL
  to `/api/evaluate/{}`: the same tests must fail. Restore it; `git diff src/` must be empty.

- [ ] **Step 4: Full gates.** `cargo fmt --check`; `cargo clippy --locked --all-targets -- -D
  warnings`; `cargo test --locked` (lib 1, dynamic_config_test 6, integration_test 10,
  override_test 8, doc-tests 1 passed — the `no_run` quick start compiles).

- [ ] **Step 5: Commit.**

```bash
git add packages/rust-server/tests/integration_test.rs
git commit -m "test(sdk): port the rust sdk integration tests to the current api" -m "They targeted a Config, Context, and method set the SDK no longer has and did not compile. Keep each test's intent on the current API against a mock of the documented evaluate contract; error cases now assert the documented fallback to defaults instead of an error. The event-tracking test is dropped: the SDK has no event API." -m "Fixes #4"
```

---

### Task 3: CI job and gate docs

**Files:**
- Create: `.github/workflows/rust-sdk.yml`
- Modify: `CLAUDE.md`

**Out-of-band / invariants:** none.

**Model:** haiku — fully specified.

- [ ] **Step 1: Create `.github/workflows/rust-sdk.yml`:**

```yaml
name: Rust SDK

# packages/rust-server, a standalone crate outside the server workspace (ci.yml
# does not build it). Tests run offline against wiremock.
on:
  push:
    branches: [main]
    paths: ['packages/rust-server/**', '.github/workflows/rust-sdk.yml']
  pull_request:
    branches: [main]
    paths: ['packages/rust-server/**', '.github/workflows/rust-sdk.yml']

concurrency:
  group: rust-sdk-${{ github.event_name == 'pull_request' && github.ref || github.run_id }}
  cancel-in-progress: ${{ github.event_name == 'pull_request' }}

permissions:
  contents: read

env:
  CARGO_TERM_COLOR: always

jobs:
  rust-sdk:
    name: Rust SDK
    runs-on: ubuntu-latest
    timeout-minutes: 20
    defaults:
      run:
        working-directory: packages/rust-server
    steps:
      - uses: actions/checkout@v4

      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt, clippy

      - uses: Swatinem/rust-cache@v2
        with:
          workspaces: packages/rust-server

      - name: cargo fmt --check
        run: cargo fmt --check

      - name: cargo clippy
        run: cargo clippy --locked --all-targets -- -D warnings

      - name: cargo test
        run: cargo test --locked
```

- [ ] **Step 2: Edit `CLAUDE.md`.**
  - Gates: `cargo clippy --locked --all-targets --keep-going -- -D warnings` (rust-server line)
    becomes `cargo clippy --locked --all-targets -- -D warnings`; delete the line
    ``- known red: Rust SDK `cargo clippy --all-targets` and `cargo test` (two integration test files do not compile; every other target must stay clean) — #4``.
  - Ship > merge gate: after the `Test` (SDKs workflow) clause, add ``; `Rust SDK` (Rust SDK
    workflow) succeeded when any changed file matches `packages/rust-server/**` or
    `.github/workflows/rust-sdk.yml``.
  - Notes: ``Only the JS/TS packages have CI (`.github/workflows/sdks.yml`). The Go, Python,
    Java, Swift, Kotlin, and Rust SDKs have none; their local gates are under Gates.`` becomes
    ``The JS/TS packages have CI in `.github/workflows/sdks.yml` and the Rust SDK in
    `.github/workflows/rust-sdk.yml`. The Go, Python, Java, Swift, and Kotlin SDKs have none;
    their local gates are under Gates.``

- [ ] **Step 3: Validate.** `python3 -c "import yaml,sys;yaml.safe_load(open('.github/workflows/rust-sdk.yml'))"`
  exits 0; `grep -n "#4\|keep-going" CLAUDE.md` prints nothing.

- [ ] **Step 4: Commit.**

```bash
git add .github/workflows/rust-sdk.yml CLAUDE.md
git commit -m "ci(sdk): build, lint, and test the rust sdk" -m "Neither workflow built packages/rust-server, which is how two test files rotted unnoticed. Run fmt, clippy, and tests on changes to the crate, and drop its known-red gate entry now that every target builds." -m "Fixes #4"
```
