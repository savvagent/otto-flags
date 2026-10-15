# Rust SDK sends the flag key as one path segment

**Status:** DRAFT
**Issue:** #95

## 1. Brief

`FlagClient::evaluate` (`packages/rust-server/src/client.rs`) builds its URL with
`format!("{}/api/flags/{}/evaluate", base_url, flag_key)`, so the flag key is not
percent-encoded. A key containing `/`, `?`, `#`, or `..` changes the request target on the flags
host, and the `sdk_` key goes with it (for example, `..` normalises to `/api/evaluate`). The host
cannot change. Keys normally come from host code, but some apps build them from input.

Acceptance criteria (#95):

- The key is sent as one percent-encoded path segment, for example with
  `Url::path_segments_mut().push(flag_key)`, or keys outside the server's key charset are rejected
  without a request.
- A wiremock test shows a key containing `/` and `?` reaches `/api/flags/<encoded>/evaluate`, or
  is refused.

## 2. Premise corrections

- `path_segments_mut().push` alone is not enough: the `url` crate silently **drops** a `.` or `..`
  segment (`PathSegmentsMut::extend`, url 2.5.8: "a segment is ignored if it is `"."` or
  `".."`"), so `push("..")` would send `/api/flags/evaluate`. Percent-encoding them does not help
  either: WHATWG URL parsing treats `%2e` and `%2e%2e` as dot segments. A key of `.` or `..`
  cannot be addressed in a URL path at all.
- The server's key rule (`crates/flags-core/src/flags.rs`, `validate_key`) is 1-128 characters of
  ASCII letters, digits, `-`, `_`, `.`, after trimming. It accepts `.` and `..` as keys, which no
  REST SDK can evaluate; that is a server-side issue, out of scope here (#95: no server changes),
  and filed as a follow-up.

## 3. Assumptions

- Every Run Profile value comes from the Development Workflow section; none assumed.
- **Decision: both guards.** `evaluate` refuses, without a request, any key the server could not
  have stored (empty, over 128 bytes, or a character outside `[A-Za-z0-9._-]`) and the keys `.`
  and `..`; and it builds the URL with `reqwest::Url` path segments, so the key is always exactly
  one encoded segment and a `base_url` path prefix is kept. The charset check alone already makes
  encoding a no-op for valid keys; the segment builder is the structural guarantee and also stops
  a trailing `/` in `base_url` producing `//api/...`.
- A refused key returns the same fallback as a non-2xx answer: `Ok` with the configured default
  and `reason: "error"`, with a `tracing::warn!` naming the key in `{:?}` form (escaped). That is
  the SDK contract ("fall back to defaults ... never throw into the host app on a flag lookup")
  and costs nothing: no flag with such a key can exist.
- Trimming is not applied: a key with surrounding whitespace is refused rather than silently
  evaluated as a different key.
- `reqwest::Url` is reqwest's re-export of `url::Url`; no new dependency.
- An unparseable `base_url` (or one that cannot be a base, e.g. `mailto:`) returns
  `Err(OttoFlagsError::ApiError(..))` from `evaluate`; today it is `Err(RequestFailed)` from
  `send`. Both are `Err`; #96 decides whether such errors fall back. No new error variant (the
  enum is not `#[non_exhaustive]`, so a variant would be breaking).
- No version bump or changeset (unpublished crate, not a pnpm package).

## 4. Goal and success criteria

The flag key can never change which endpoint the SDK calls or where its `sdk_` key is sent.

- A key containing `/` and `?` (and one containing `#`, `%`, a space) makes no request and returns
  the default with `reason: "error"` (wiremock, a catch-all mock with `expect(0)`).
- `.` and `..` make no request.
- A valid key using every allowed character class (`My.flag_1-x`) reaches
  `/api/flags/My.flag_1-x/evaluate`.
- A `base_url` with a trailing `/` or a path prefix (`http://host/flags/`) reaches
  `<prefix>/api/flags/<key>/evaluate`.
- `is_enabled` on a refused key returns the configured default.

## 5. Scope

In: `src/client.rs` (`is_valid_flag_key`, `evaluate_url`, the refusal at the top of `evaluate`),
`tests/integration_test.rs`, `README.md` (one line on the key rule).

Out: server key validation (follow-up issue); transport-error fallback and `Metadata` (#96);
other SDKs.

## 6. Architecture

In `src/client.rs`:

- `fn is_valid_flag_key(key: &str) -> bool`: `!key.is_empty() && key.len() <= 128 && key != "."
  && key != ".." && key.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' |
  b'.'))`. Its doc names the server rule it mirrors (`flags_core::flags::validate_key`, by path in
  prose; no code shared, the package is MIT).
- `fn evaluate_url(&self, flag_key: &str) -> Result<reqwest::Url, OttoFlagsError>`:
  `Url::parse(&self.config.base_url)`, then `path_segments_mut()` → `pop_if_empty()` →
  `extend(["api", "flags", flag_key, "evaluate"])`.
- `evaluate`: first statement checks `is_valid_flag_key`; on failure warn and
  `return self.get_default_result(flag_key)` (before the cache lookup, so nothing is cached). The
  request uses `evaluate_url(flag_key)?`.

Public API: unchanged. Behavior: invalid keys no longer reach the network.

## 7. Invariants impact

- SDK REST contract: unchanged for every key the server can hold.
- SDKs never throw on a lookup: a refused key falls back (`Ok`, `reason: "error"`).
- Client keys public: the `sdk_` key is now only ever sent to the evaluate endpoint.
- Licensing: the rule is restated, not copied; MIT package.

## 8. Migration

None.

## 9. Out-of-band

None. SDK-only: post-merge verification is server health only; the SDK is not published.

## 10. Error handling and edge cases

- Multi-byte characters are refused (non-ASCII), length is in bytes, which equals characters for
  the allowed set.
- A key refused here is not cached, so fixing the key in host code takes effect immediately.

## 11. Testing approach

Gates for `packages/rust-server/**`: `cargo fmt --check`, `cargo clippy --locked --all-targets --
-D warnings`, `cargo test --locked` in `packages/rust-server` and in `examples/rust-server`.
Integration tests in `tests/integration_test.rs` (wiremock) for each success criterion; a unit
test beside `is_valid_flag_key` for the boundary cases (128 vs 129 bytes, `.`, `..`, `...`,
empty, non-ASCII).

## 12. Risks and open questions

- A host that today evaluates a key with an invalid character gets the default instead of a 404
  answer, which was also the default. No behavior change for it in practice.
- `...` is a valid server key and a valid path segment; it is allowed.
