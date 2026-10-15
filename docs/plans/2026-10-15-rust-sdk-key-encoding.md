# Rust SDK Flag Key Encoding Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** The flag key can never change which endpoint the Rust SDK calls.

**Architecture:** `evaluate` refuses keys the server could not hold (and `.`/`..`) without a
request, falling back to the default; the URL is built from `reqwest::Url` path segments.

**Tech Stack:** Rust 2021, reqwest 0.11 (`reqwest::Url`), wiremock.

**Ticket:** #95

**Spec:** `docs/specs/2026-10-15-rust-sdk-key-encoding-design.md`

## Global Constraints

- Gates, each run from `packages/rust-server` and again from `examples/rust-server`, with
  `export CARGO_BUILD_JOBS=3 TMPDIR=<worktree>/packages/rust-server/target/tmp` (mkdir it first):
  `cargo fmt --check`; `cargo clippy --locked --all-targets -- -D warnings`; `cargo test --locked`.
- Evidence: pass counts per test binary (lib, dynamic_config_test, integration_test,
  override_test, doc-tests; example: 1 binary); a test that did not run is not a pass.
- Invariants: SDK REST contract unchanged for valid keys; never throw on a lookup (refusal is
  `Ok` with the default); MIT package, no server code copied; public API unchanged.
- Worktree only; trunk changes only through the PR.
- Commit format: `fix(sdk): <imperative, lower-case>`, body explains why, ends `Fixes #95`; no
  attribution of any kind (no Co-Authored-By, no "Generated with", no credit markers). Commit from
  the worktree root.

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `packages/rust-server/src/client.rs` | `is_valid_flag_key`, `evaluate_url`, refusal in `evaluate`, unit test |
| **Modify.** `packages/rust-server/tests/integration_test.rs` | Wiremock tests for refusal and URL shape |
| **Modify.** `packages/rust-server/README.md` | One line: keys outside the server's charset fall back without a request |

## Task Order & Rationale

One task; the guard and the URL builder are one change to one function.

---

### Task 1: Refuse unaddressable keys and build the URL from segments

**Files:** Modify `packages/rust-server/src/client.rs`, `packages/rust-server/README.md`; Test
`packages/rust-server/tests/integration_test.rs`.

**Interfaces:** private `fn is_valid_flag_key(&str) -> bool`; private
`fn evaluate_url(&self, &str) -> Result<reqwest::Url, OttoFlagsError>`.

**Out-of-band / invariants:** none out-of-band; see Global Constraints.

**Model:** sonnet

- [x] **Step 1: Write the failing integration tests** (append to `tests/integration_test.rs`;
  add `use wiremock::matchers::any;`)

```rust
async fn refuses_without_a_request(flag: &str) {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "enabled": false })))
        .expect(0)
        .mount(&server)
        .await;
    let client = FlagClient::new(
        Config::new(KEY).with_base_url(server.uri()).with_default(flag, true),
    )
    .unwrap();

    let result = client.evaluate(flag, None).await.unwrap();
    assert!(result.value, "{flag:?} should fall back to its default");
    assert_eq!(result.reason, "error");
    assert!(client.is_enabled(flag, None).await);
}

#[tokio::test]
async fn a_key_with_path_or_query_characters_is_refused_without_a_request() {
    for flag in ["a/b?c", "../../api/sdk/flags", "x#y", "50%", "two words", "", ".", ".."] {
        refuses_without_a_request(flag).await;
    }
}

#[tokio::test]
async fn a_key_using_every_allowed_character_reaches_its_own_path() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/flags/My.flag_1-x/evaluate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "enabled": true })))
        .expect(1)
        .mount(&server)
        .await;
    assert!(client(&server).is_enabled("My.flag_1-x", None).await);
}

#[tokio::test]
async fn a_base_url_path_prefix_and_trailing_slash_are_kept() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/flags/api/flags/test-flag/evaluate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "enabled": true })))
        .expect(1)
        .mount(&server)
        .await;
    let client = FlagClient::new(
        Config::new(KEY).with_base_url(format!("{}/flags/", server.uri())),
    )
    .unwrap();
    assert!(client.is_enabled("test-flag", None).await);
}
```

- [x] **Step 2: Run, see them fail**

Run: `cargo test --locked --test integration_test` (env per Global Constraints).
Expected: the refusal test fails (`expect(0)` violated / wrong value for `a/b?c`), and the prefix
test fails (request goes to `/flags//api/...`).

- [x] **Step 3: Implement in `src/client.rs`**

```rust
/// Whether the server could hold this flag key, and a URL path can address it.
///
/// Mirrors the server's key rule (1-128 ASCII letters, digits, `-`, `_`, `.`; `validate_key` in
/// `crates/flags-core/src/flags.rs`), so refusing anything else loses no flag. `.` and `..` pass
/// that rule but no URL can carry them as a path segment (the `url` crate drops them and
/// WHATWG parsing treats `%2e` as a dot segment), so they would reach another endpoint (#95).
fn is_valid_flag_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 128
        && key != "."
        && key != ".."
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}
```

In `impl FlagClient`:

```rust
/// `<base_url>/api/flags/<key>/evaluate`, with the key as one encoded path segment.
fn evaluate_url(&self, flag_key: &str) -> Result<reqwest::Url, OttoFlagsError> {
    let mut url = reqwest::Url::parse(&self.config.base_url)
        .map_err(|e| OttoFlagsError::ApiError(format!("invalid base_url: {e}")))?;
    url.path_segments_mut()
        .map_err(|()| OttoFlagsError::ApiError("invalid base_url: cannot be a base".into()))?
        .pop_if_empty()
        .extend(["api", "flags", flag_key, "evaluate"]);
    Ok(url)
}
```

At the top of `evaluate`, before preparing the context:

```rust
if !is_valid_flag_key(flag_key) {
    tracing::warn!("Flag key {:?} is not a valid flag key; using its default", flag_key);
    return self.get_default_result(flag_key);
}
```

Replace `let url = format!(...)` with `let url = self.evaluate_url(flag_key)?;` and pass `url`
to `.post(url)`.

Unit test in a `#[cfg(test)] mod tests` at the bottom of `src/client.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::is_valid_flag_key;

    #[test]
    fn flag_keys_follow_the_server_rule_minus_dot_segments() {
        for ok in ["a", "My.flag_1-x", "...", &"k".repeat(128)] {
            assert!(is_valid_flag_key(ok), "{ok:?}");
        }
        for bad in ["", ".", "..", &"k".repeat(129), "a/b", "a?b", "a#b", "a%2Fb", " a", "é"] {
            assert!(!is_valid_flag_key(bad), "{bad:?}");
        }
    }
}
```

- [x] **Step 4: README** — under the `evaluate` method docs, add: "Flag keys are 1-128 ASCII
  letters, digits, `-`, `_` and `.` (not `.` or `..`), as the server requires; any other key
  returns its default with `reason: "error"` and makes no request."

- [x] **Step 5: Run all gates** (Global Constraints), `cargo fmt` first. Expected: all pass.

- [x] **Step 6: Commit** — `fix(sdk): send the rust sdk flag key as one path segment`, why-body,
  `Fixes #95`.
