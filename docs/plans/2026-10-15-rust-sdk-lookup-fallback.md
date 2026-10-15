# Rust SDK Lookup Fallback and Metadata Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** Every Rust SDK lookup falls back to the default on any failure, never caching it, and
`Metadata` carries the `version` and `scope` the server sends.

**Architecture:** `evaluate` funnels the request into a private `fetch` that returns the parsed
body or a failure description; one fallback point returns `get_default_result` uncached.
`Metadata` gains `version`/`scope` (+ `Default`); `CacheEntry` stores the metadata.

**Tech Stack:** Rust 2021, reqwest 0.11, serde_json, wiremock.

**Ticket:** #96 (and #92)

**Spec:** `docs/specs/2026-10-15-rust-sdk-lookup-fallback-design.md`

## Global Constraints

- Gates, each from `packages/rust-server` and from `examples/rust-server`, with
  `export CARGO_BUILD_JOBS=3 TMPDIR=<worktree>/packages/rust-server/target/tmp` (mkdir first):
  `cargo fmt --check`; `cargo clippy --locked --all-targets -- -D warnings`; `cargo test --locked`.
  Run `cargo fmt` before `--check`.
- Evidence: pass counts per test binary (lib, dynamic_config_test, integration_test,
  override_test, doc-tests; example: 1); a test that did not run is not a pass.
- Invariants: never throw on a lookup; SDK REST contract unchanged; MIT, no server code; public
  API changes additive (`Metadata` fields named in the spec §3).
- Worktree only; trunk changes only through the PR.
- Commit format: `fix(sdk): <imperative, lower-case>`, why-body, ends `Fixes #96` and `Fixes #92`;
  no attribution of any kind (no Co-Authored-By, no "Generated with", no credit markers). Commit
  from the worktree root.

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `packages/rust-server/src/client.rs` | `fetch`, single fallback point, metadata from body, docs |
| **Modify.** `packages/rust-server/src/cache.rs` | `CacheEntry.metadata`, `set` takes it |
| **Modify.** `packages/rust-server/src/types.rs` | `Metadata { version, scope, .. }`, `Default`, field docs |
| **Modify.** `packages/rust-server/tests/integration_test.rs` | Fallback and metadata tests |
| **Modify.** `packages/rust-server/README.md` | Error handling section; `EvaluationResult`/`Metadata` |

## Task Order & Rationale

One task: the fallback, cache, and metadata changes all meet in `evaluate`.

---

### Task 1: Fall back on every lookup failure; fill metadata

**Model:** sonnet

- [x] **Step 1: Failing tests** in `tests/integration_test.rs`:
  - Change `an_unreachable_server_falls_back_to_the_default` to assert
    `evaluate` is `Ok` with `value == true` (configured default) and `reason == "error"`; and add
    `get_config(..)` is `Ok(None)` and `get_variation(..)` is `Ok` with `"control"` against the
    same unreachable URL. Drop the then-unused `OttoFlagsError` import only if clippy flags it
    (it is still used by `client_rejects_a_key_without_the_sdk_prefix`).
  - `a_non_json_body_falls_back_and_is_not_cached`: 200 with `set_body_string("<html>")`,
    `expect(2)`; two `evaluate` calls both `Ok`, `reason == "error"`, value = configured default.
  - `a_body_without_enabled_falls_back_and_is_not_cached`: 200 `{"value": true}`, `expect(2)`,
    configured default `false`... assert `reason == "error"` both times. Update the existing
    `a_response_without_enabled_falls_back_to_the_default` to also assert `reason == "error"`.
  - `an_invalid_base_url_falls_back`: `with_base_url("not a url")`, `evaluate` is `Ok`,
    `reason == "error"`.
  - `metadata_carries_the_version_and_scope_the_server_sends`: 200
    `{"enabled": true, "scope": "application", "version": 7}`, `expect(1)`; first result's
    `metadata.version == Some(7)`, `scope == Some("application")`, legacy fields `None`; second
    (cached) result has the same metadata.
- [x] **Step 2: Run** `cargo test --locked --test integration_test`; expect the new/changed tests
  to fail (Err instead of Ok; `reason == "evaluated"`; caching; `version` field missing → compile
  error is an acceptable "fail" for the metadata test).
- [x] **Step 3: Implement**
  - `types.rs`: `#[derive(Debug, Clone, Default, Serialize, Deserialize)] pub struct Metadata`
    with `version: Option<i64>` ("the flag's version that produced this result") and
    `scope: Option<String>` (`"application"` or `"enterprise"`), both
    `#[serde(skip_serializing_if = "Option::is_none")]`; doc the three existing fields as "not
    sent by the server; always `None`, kept for compatibility".
  - `cache.rs`: `CacheEntry { .., pub metadata: Option<Metadata> }`; `set(&self, flag_key,
    context_key, value, configuration, variation, metadata)`; update unit tests' `set` calls
    with a trailing `None`.
  - `client.rs`: `async fn fetch(&self, flag_key: &str, ctx: &Context) -> Result<Value, String>`
    doing URL → send → status check → `json::<Value>()` → require `data["enabled"].as_bool()`;
    each failure maps to a short string (`format!("invalid base_url: {e}")`, `format!("request
    failed: {e}")`, `format!("status {}", status)`, `"body is not JSON"`, `"no boolean \`enabled\`"`).
    In `evaluate`: `match self.fetch(..).await { Ok(d) => d, Err(why) => { tracing::warn!("Flag
    {:?} lookup failed ({}); using its default", flag_key, why); return
    self.get_default_result(flag_key); } }`, then read `value` from `enabled` (now guaranteed),
    build `metadata`, cache with metadata, return `metadata: Some(..)`. Cached hits return
    `metadata: cached_entry.metadata.clone()`. Remove `evaluate_url`'s `?` path from `evaluate`
    (it moves into `fetch`). Doc on `evaluate`/`get_config`/`get_variation`: a lookup does not
    return `Err`; failures give the default with `reason: "error"`.
  - `README.md`: `EvaluationResult` block lists `configuration`, `variation`, `metadata`;
    a `Metadata` block with `version`/`scope`; the Error Handling section shows checking
    `result.reason == "error"` and says `FlagClient::new` is the only call that returns `Err`
    (`InvalidApiKey`, or `RequestFailed` if the HTTP client cannot be built).
- [x] **Step 4: Run all gates** (Global Constraints). Expected green.
- [x] **Step 5: Commit** `fix(sdk): fall back on every rust sdk lookup failure and fill metadata`,
  why-body, `Fixes #96`, `Fixes #92`.
