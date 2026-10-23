# Rust SDK: env-derived defaults, a sync cached read and a static test client

**Status:** IMPLEMENTED (#174)
**Ticket:** savvagent/otto-flags#159

## Brief

> An agent evaluating the Rust SDK (`packages/rust-server`) found three gaps that make a standard
> migration from env-var flags harder than it should be.
> 1. **Default from env.** `Config::with_default(key, bool)` takes a constant [...]
> 2. **Sync read.** `FlagClient::is_enabled` is async [...] A cheap synchronous read of the last
>    cached value would fit those cases.
> 3. **Test client.** There is no in-memory or fake client [...]
>
> Acceptance: a documented way to default a flag from an env var, with a test; a sync cached read
> with tests for cached value and cache-miss-falls-back-to-default; a static client usable without
> network, with a test and a README example; additive changes; the package gate passes (fmt,
> clippy, `cargo test --locked` in `packages/rust-server` and `examples/rust-server`); the Rust
> `sdk_guide` text mentions the new APIs. Non-goals: the REST contract, rollout bucketing,
> publishing or bumping the crate version. Check whether the other server SDKs have the same gaps
> and file follow-ups rather than widening the issue.

## Premise corrections

None. `Config::with_default` is at `types.rs` and `FlagClient::is_enabled` at `client.rs`, as cited
(line numbers have moved).

## Assumptions

- Env defaults are read once, when the config is built, because `Config::defaults` is a
  `HashMap<String, bool>` consulted on failure paths. Re-reading per lookup would change the type.
- A sync read honours the cache TTL (an expired entry is a miss), so a stale value never outlives
  the bound the async path gives a kill switch. The cost is that callers keep the cache warm;
  documented.
- A sync cache hit is counted as an evaluation in telemetry, exactly as a cached `evaluate` is; a
  miss is not.
- Accepted env spellings: `1/true/yes/on` and `0/false/no/off`, ignoring case and whitespace. An
  unset variable is ignored silently (the normal production case); an empty, non-Unicode or
  unrecognized one is ignored with a warning that names the variable, not its value.
- The static client keeps config and variation overrides working, so tests of override-reading code
  need no second client.
- The sync read takes `Option<&Context>` (no clone needed by the caller), unlike the async methods'
  `Option<Context>`.
- No version bump and no changeset (not a pnpm package).

## Goal & Success Criteria

A service migrating from env-var flags can default a flag from an env var, read a flag from a
non-async helper, and test with flags fixed, without wrapper code.

- `Config::with_default_env` and `with_default_fn` set defaults (tests).
- `FlagClient::is_enabled_cached` returns the cached value on a hit and the default on a miss,
  expiry, other-context lookup or invalidation, with no request (tests).
- `FlagClient::static_flags` builds and answers with no network or runtime (tests).
- Existing signatures and behaviour are unchanged; all existing tests pass.

## Scope

In: `packages/rust-server` (types, client, tests, README), the Rust `sdk_guide` text. Out: REST
contract, bucketing, publishing, other SDKs (a follow-up issue records the survey).

## Architecture

- `types.rs`: `Config::with_default_fn(key, FnOnce() -> Option<bool>)` and
  `with_default_env(key, var)` (built on it); a private `parse_bool`.
- `client.rs`: a `fixed: Option<Arc<HashMap<String, bool>>>` field, `FlagClient::static_flags`,
  `FlagClient::is_enabled_cached`, and a private `apply_overrides` extracted from two identical
  inline blocks in `evaluate` (same behaviour) so the static path reuses it. `evaluate` returns
  early with `reason: "static"` when `fixed` is set, after the flag-key validity check.
- All changes are additive public API; `FlagClient`/`Config` gain no public fields.
- `crates/flags-mcp/content/guides/rust.md`: mentions the new APIs (no placeholders added).

## Invariants impact

- SDK REST contract stable: untouched.
- SDKs never throw into the host app on a flag lookup: `is_enabled_cached` and the static path do
  not return `Err` or panic; `static_flags` `expect`s on a constructor that cannot fail for its
  fixed arguments (static key, telemetry and realtime off).
- Rollout bucketing: untouched. Client keys public: the static client's placeholder key never
  leaves the process (no request is made).
- Licensing: MIT package; no server code copied. The guide edit is text in the AGPL crate.

## Migration / Out-of-band

None. The merge redeploys the server because `crates/flags-mcp/content/guides/rust.md` changed
(the guide text is served by `sdk_guide`); the SDK change itself is not published.

## Error Handling & Edge Cases

Unset, empty, non-boolean or non-Unicode env vars leave the earlier default. A context that cannot
be serialized for the cache key is a miss. The sync read outside a Tokio runtime works: it makes no
request and does not wait (a hit queues a telemetry event, which starts the flusher only when a
runtime is current). An invalid flag key gets its default on both `evaluate` and the sync read,
including on a static client. The follow-up survey of the other SDKs is #175.

## Testing Approach

`packages/rust-server/tests/ergonomics_test.rs` (wiremock for the cached cases; an unreachable
port for the fallback). Gates: package fmt/clippy/test `--locked` in `packages/rust-server` and
`examples/rust-server`, plus the workspace gates and Docker build because `crates/` changed.

## Risks & Open Questions

The sync read returns the default for a flag nobody has evaluated yet; a caller that never warms
the cache would always get the default. The docs say so. The follow-up survey of other SDKs is
#175.
