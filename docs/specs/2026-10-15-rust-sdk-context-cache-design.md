# Rust SDK caches evaluations per flag and context

**Status:** DRAFT
**Issue:** #91

## 1. Brief

`FlagClient::evaluate` in `packages/rust-server/src/client.rs` caches results by flag key alone
(`Cache::get_entry(flag_key)` / `cache.set(flag_key, ...)` in `src/cache.rs`), ignoring the
evaluation context. Within the cache TTL (default 60 s), the first context's result is returned
for every later context: a flag targeted at user A, or rolled out to a percentage, gives user B
whatever A got.

Acceptance criteria (#91):

- Two `evaluate` calls for the same flag with different contexts (different `user_id`, or
  attributes a rule targets) within the TTL each get the server's answer for their own context.
- A test in `packages/rust-server/tests/` against wiremock pins this (two mocks distinguished by
  `body_partial_json` on the context).
- Caching still avoids a request for a repeated identical context within the TTL, or caching is
  dropped in favor of the `/api/sdk/flags` + local evaluation model the other SDKs use (decide in
  the spec).

Out of scope (#91): server changes.

Caller's addition: drop the README caveat #94 added ("Caching caveat: ... until this is fixed
(#91)") once fixed.

## 2. Premise corrections

- None material. The cache is `Cache` (`src/cache.rs`), `pub(crate)` in a private module, so its
  signature can change without touching the public API.

## 3. Assumptions

- Every Run Profile value comes from the Development Workflow section; none assumed.
- **Decision: keep server-side evaluation and key the cache by (flag key, context).** Local
  evaluation over `/api/sdk/flags` would put targeting rules in the SDK; the Invariants say a
  client key never sees targeting rules, so that model is not open to this SDK (and it would be a
  much larger change). The evaluate endpoint's answer depends only on the flag key, the API key
  (fixed per client), and the request body's context, so (flag key, context) identifies a result.
- The context in the key is the one actually sent: after `application_id` is filled from
  `Config::application_id`. Every serialized field takes part (`user_id`, `session_id`,
  `application_id`, `attributes`, `environment`, `ip_address`, `user_agent`). Narrowing the key to
  the fields the server reads today would silently break when the server reads another one;
  a wider key only costs hit rate.
- No version bump and no changeset (unpublished crate, not a pnpm package).

## 4. Goal and success criteria

A cached result is only ever returned for the same flag and the same context that produced it.

- Different `user_id`s for one flag within the TTL each reach the server and get their own answer
  (wiremock test, two mocks distinguished by `body_partial_json`, each `expect(1)`).
- Same for contexts that differ only in one attribute value.
- A repeated identical context is served from the cache (`reason == "cached"`, one request),
  including when its attributes were inserted in a different order.
- `invalidate_cache(Some(flag))` drops the flag's entries for every context.
- The cache for one flag holds at most `MAX_CONTEXTS_PER_FLAG` entries.

## 5. Scope

In: `src/cache.rs` (two-level map, canonical context key, per-flag bound, unit tests),
`src/client.rs` (`evaluate` computes the key from the prepared context before the cache lookup;
doc comments), `tests/integration_test.rs` (new tests), `README.md` (drop the caveat; say what
the cache is keyed by).

Out: server changes; local evaluation; URL encoding of the key (#95); fallback on transport errors
and `Metadata` (#96); SSE invalidation.

## 6. Architecture

`Cache` (`src/cache.rs`) becomes
`Arc<RwLock<HashMap<String /* flag key */, HashMap<String /* context key */, CacheEntry>>>>`.

- `context_key(ctx: &Context) -> Option<String>`: `serde_json::to_value(ctx)`, recursively
  rebuild every object with its entries sorted by key, then `to_string`. Sorting explicitly makes
  the key independent of `HashMap` iteration order whether or not some dependency turns on
  serde_json's `preserve_order`. `None` only if serialization fails (it cannot for `Context`, but
  no `unwrap`): `evaluate` then skips the cache for that call, reading and writing nothing.
- `get_entry(flag, ctx_key) -> Option<CacheEntry>`: as today, expired means absent.
- `set(flag, ctx_key, value, configuration, variation)`: inserts. If the flag's inner map already
  holds `MAX_CONTEXTS_PER_FLAG` (10 000) entries and `ctx_key` is new, first drop its expired
  entries; if it is still full, clear that flag's inner map. Clearing (rather than evicting one
  entry) keeps the expensive sweep amortized: at most one O(n) pass per 10 000 inserts. Cost of
  hitting the bound is cache misses, never a wrong answer. This bounds memory per flag; the
  number of flags is bounded by the host code's flag keys.
- `invalidate(Some(flag))` removes the flag's inner map (all contexts); `invalidate(None)` clears
  everything. Override setters already call this, so overrides still invalidate every context.

`FlagClient::evaluate` (`src/client.rs`): build the prepared context (`unwrap_or_default` plus
the `application_id` fill-in, moved above the cache lookup), compute `context_key`, look up
`(flag_key, ctx_key)`, and on a miss send that same context and `set` under the same key. Override
application on both paths is unchanged.

Public API: unchanged (`Cache` is crate-private). Behavior change: a cache hit now requires an
identical context; `invalidate_cache(Some(flag))` documents that it covers all contexts.

## 7. Invariants impact

- SDK REST contract: unchanged; same request, same body.
- SDKs never throw on a lookup: unchanged (#96 owns transport errors).
- Client keys public / no targeting rules in the SDK: preserved; this is why local evaluation is
  rejected.
- Licensing: MIT package, no server code copied.
- No end-user context persisted: the cache is in-process memory only, bounded and TTL'd, as before;
  it now keys on context, which it already held in memory per request.

## 8. Migration

None.

## 9. Out-of-band

None. SDK-only change: post-merge verification is server health only; the SDK is not published.

## 10. Error handling and edge cases

- `context: None` and `Some(Context::default())` produce the same prepared context, so they share
  an entry (correct: the server sees the same body).
- Attribute insertion order does not matter (sorted key).
- Errors (non-2xx) are still not cached.
- A poisoned lock: reads return `None`, writes are skipped, as today.

## 11. Testing approach

Gates (Development Workflow > Gates, `packages/rust-server/**`): `cargo fmt --check`,
`cargo clippy --locked --all-targets -- -D warnings`, `cargo test --locked` in
`packages/rust-server`; `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D
warnings`, and `cargo test --locked` in `examples/rust-server` (a path dependent of this crate,
green since #90).

- `tests/integration_test.rs`: different `user_id`s each get their own answer (A, B, A, B: two
  requests total, second A and B are `cached` with their own values); contexts differing in one
  attribute each get their own answer; identical contexts with attributes inserted in different
  orders hit the cache; `invalidate_cache(Some(flag))` drops every context's entry (both
  re-evaluated).
- `src/cache.rs` unit tests (the per-flag bound is a field, set small by a crate-private
  constructor in tests): `context_key` is order-independent and distinguishes different
  values; `invalidate(Some(flag))` removes all contexts of that flag only; a full flag map is
  swept/cleared and still accepts the new entry.

## 12. Risks and open questions

- Hit rate drops for callers that put per-request values (`user_agent`, `ip_address`) in the
  context. Accepted: correctness first; those fields can change targeting.
- `examples/rust-server` (ported in #90) depends on this crate by path; the API is unchanged, so
  it still builds and its tests pass.
- The `context_key` `None` path is untested by design: serializing a `Context` cannot fail.
- `attributes: None` and `attributes: Some(empty)` get different keys; correct, since the request
  bodies differ.
