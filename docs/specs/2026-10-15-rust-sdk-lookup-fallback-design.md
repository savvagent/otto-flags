# Rust SDK lookups fall back on every failure; metadata carries what the server sends

**Status:** DRAFT
**Issues:** #96, #92

## 1. Brief

#96: In `packages/rust-server`, `get_config` and `get_variation` return `Err` when the server is
unreachable or a 200 body is not JSON, because `evaluate` propagates `send().await?` and
`response.json().await?`. Only `is_enabled` falls back to the default. CLAUDE.md's invariant says
SDKs "fall back to defaults and cached values and never throw into the host app on a flag
lookup". A Rust `Result` is not a throw, but callers using `?` will fail their request on a flags
outage. `EvaluationResult.metadata` reads `flag_id`, `description`, and `variant`, which the
server never sends, so it is always empty. The server does send `version` and `scope`.

Acceptance criteria (#96):
- Decide, in a spec, whether `evaluate`/`get_config`/`get_variation` return the default result
  (`reason: "error"`) on transport and parse errors as they already do on non-2xx. Tests pin the
  chosen behavior.
- `Metadata` reflects fields the server sends, or is documented as reserved. Any change is
  additive.

#92 (folded in; its `value`→`enabled` part was fixed by #94): the result of a response the SDK
could not read "is then cached for the TTL as if it were real", and "Metadata fields map to what
the server returns, or are dropped."

## 2. Premise corrections

- Since #95, an unparseable `base_url` is a fourth `Err` source (`ApiError` from `evaluate_url`).
  It is covered by the same decision.
- #92's main criterion (read `enabled`) is already met on `main` (#94). What remains is metadata
  and the caching of an unreadable 200: today a 200 without a boolean `enabled` returns the
  default with `reason: "evaluated"` and caches it for the TTL.

## 3. Assumptions

- Every Run Profile value comes from the Development Workflow section; none assumed.
- **Decision: a lookup never returns `Err`.** `evaluate` returns `Ok` with the default result
  (`value` = `Config::defaults` entry or `false`, no configuration or variation, `reason:
  "error"`, `metadata: None`) for every failure after `FlagClient::new`: an invalid `base_url`, a
  transport error or timeout, a non-2xx status (as today), a body that is not JSON, and a 200
  whose `enabled` is missing or not a boolean. `get_config` and `get_variation` inherit it. The
  signatures keep their `Result` (removing it would break callers); the docs say a lookup does not
  return `Err`. This is the SDK contract, matches `is_enabled`, and makes `?` safe in host code.
  Callers that need to know can test `reason == "error"`; each failure is logged at `warn`.
- **Fallback results are never cached**, so recovery is immediate. A 200 without a boolean
  `enabled` is a failure (a proxy or captive page, not this server), not a default to cache.
- No stale-cache fallback: an unexpired cached result is already served before any request; an
  expired one is not resurrected (YAGNI; the guide's "cached/default" is met by the TTL cache).
- **Metadata:** add `version: Option<i64>` and `scope: Option<String>`, filled from the response's
  `version` and `scope`; keep `flag_id`, `description`, `variant` as fields documented as not sent
  by the server (always `None`), since removing them would break callers that read them. Derive
  `Default` on `Metadata` so callers can build one with `..Default::default()`. Adding public
  fields breaks only code that builds `Metadata` with a struct literal or destructures it
  exhaustively; the crate is unpublished
  (0.1.0) and nothing in the repo (including `examples/rust-server`) builds one, so this is
  accepted as additive in practice and named here.
- Cached hits carry the metadata of the response they came from (today `None`): `CacheEntry`
  stores it.
- `OttoFlagsError::RequestFailed`/`ApiError` stay (public enum; `ApiError` is still returned by
  `set_config_override` validation). No version bump or changeset.

## 4. Goal and success criteria

A flags outage or a garbled response can never fail a host request through the SDK.

- Unreachable server: `evaluate` is `Ok` with the default and `reason: "error"`; `get_config` is
  `Ok(None)`; `get_variation` is `Ok` with `"control"`.
- A 200 with a non-JSON body, and a 200 without a boolean `enabled`: `Ok` default, `reason:
  "error"`, not cached (a second call reaches the server).
- An invalid `base_url`: `Ok` default, `reason: "error"`.
- A good response yields `metadata.version` and `metadata.scope`; a cached hit returns the same.

## 5. Scope

In: `src/client.rs` (`evaluate` failure paths, docs), `src/cache.rs` (`CacheEntry.metadata`),
`src/types.rs` (`Metadata`), tests, `README.md` (error handling, `EvaluationResult`/`Metadata`).

Out: `srv_` keys and telemetry (#93); stale-cache fallback; other SDKs; server changes.

## 6. Architecture

`evaluate` (after the key check and cache lookup) becomes: build the URL, send, check status, read
the body as JSON, read `enabled` as a bool; any `Err`/`None` along the way logs at `warn` and
returns `self.get_default_result(flag_key)` without touching the cache. Implementation: a private
`async fn fetch(&self, flag_key, &ctx) -> Result<serde_json::Value, String>` returning the parsed
body or a short failure description, so `evaluate` has one fallback point. `Metadata` is built
from the body: `version: data["version"].as_i64()`, `scope: data["scope"].as_str()`, the three
legacy fields `None`. `Cache::set` takes the `Option<Metadata>` and `CacheEntry` stores it.

Public API: `Metadata` gains `version`, `scope` and `Default` (additive, see §3); `evaluate`,
`get_config`, `get_variation` signatures unchanged; their failure behavior changes from `Err` to
`Ok(default)` (the decision #96 asks for).

## 7. Invariants impact

- Never throw on a lookup: now true for every lookup method, not just `is_enabled`.
- SDK REST contract: unchanged; the SDK reads documented fields (`enabled`, `version`, `scope`).
- Licensing: MIT; no server code.

## 8. Migration

None.

## 9. Out-of-band

None. SDK-only: post-merge verification is server health only; not published.

## 10. Error handling and edge cases

- Log text names the flag key (`{:?}`) and the failure kind (status code, `reqwest` error, "body
  is not JSON", "no boolean `enabled`"); never the SDK key or body.
- `reqwest` errors are logged with `without_url()`: the URL holds only `base_url` and the key
  (the SDK key is a header), but a `base_url` with userinfo would otherwise reach the log.
- A timeout (`Config::timeout`) is a transport error: fallback.

## 11. Testing approach

Gates for `packages/rust-server/**` (fmt, clippy, test in `packages/rust-server` and
`examples/rust-server`). In `tests/integration_test.rs`: replace the `Err(RequestFailed)` assertion
in `an_unreachable_server_falls_back_to_the_default` with the fallback; add tests for
`get_config`/`get_variation` unreachable, non-JSON 200 (not cached, `expect(2)`), missing
`enabled` (not cached), invalid `base_url`, and metadata on fresh and cached results.

## 12. Risks and open questions

- A caller who matched on `Err(RequestFailed)` to detect outages now sees `Ok` with `reason:
  "error"`. That is the decision the issue asks for; the README says how to detect it.
