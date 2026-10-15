# Rust SDK lookups back off after a failure and read a bounded response

**Status:** IMPLEMENTED (#117)
**Issue:** #111

## 1. Brief

#111: Since #96 (PR #109), a failed Rust SDK lookup returns the default and is not cached, so
recovery is immediate. During an outage, though, every lookup still sends a new request and can
wait up to `Config::timeout` (5 s by default). A proxy or captive portal answering 200 with a
non-evaluate body costs one request per lookup. And `Response::json()` reads the whole body with
no size limit, so a misconfigured `base_url` or a portal can push a large body into host memory.

Acceptance criteria (verbatim):
- After a failed lookup, a short cooldown (a few seconds, per client or per flag) returns the
  default without a request. `Retry-After` on 429/503 is honoured. A wiremock test shows that
  requests during the cooldown are skipped and that the first lookup after it reaches the server.
- The evaluate response body is read with a size cap (for example 64 KiB) and an oversized body is
  a failed lookup. A test covers this.
- Warning logs during an outage are bounded, for example one per cooldown window rather than one
  per lookup.

Out of scope (ticket): server changes; other SDKs.

## 2. Premise corrections

- A purely per-client cooldown would let one flag's own failure turn every other flag to its
  default: the server answers `404 flag_not_found` for a key with no active flag
  (`crates/flags-api/src/error.rs`), and a stale key in a hot path would then black out the whole
  client for every window. Failures that belong to one flag (404, an oversized body) cool down that
  flag only; failures that say the service or the path to it is unhealthy cool down the client.
- The server's only `Retry-After` is `5` (seconds) on its 503 (`crates/flags-api/src/error.rs`);
  nothing on the evaluate path sends 429 today, but a proxy may.
- The server caps no flag `configuration` size, so a legitimate evaluate response is not bounded
  by any server rule. The 64 KiB example in the ticket could refuse a real configuration; the cap
  is set at 1 MiB (see Assumptions).

## 3. Assumptions

- Every Run Profile value comes from the Development Workflow section; none assumed.
- **Cooldown length:** 5 s by default (the server's own `Retry-After`), configurable through a new
  `Config::failure_cooldown: Duration` field and `Config::with_failure_cooldown(Duration)`.
  `Duration::ZERO` turns backoff off entirely (including `Retry-After`), giving the #109 behavior.
  Adding a public field to `Config` breaks only code that builds `Config` with a struct literal;
  `Config` has a builder and `Default`, the crate is unpublished (0.1.0), and nothing in the repo
  (including `examples/rust-server`) builds one by literal, so this is accepted as additive in
  practice, as #109 did for `Metadata`.
- **Scope of a cooldown** (revised in review: the security audit showed that a failure the
  request itself causes must open no window, or one end user's context could switch every
  user's flags to their defaults, repeatably):
  - per flag: `404`;
  - none (`Scope::Request`): any other 4xx except 401, 408 and 429 (the server answers
    400/413/422 for a request body it refuses, and 401 for every key problem, so a 403 comes from
    a WAF or proxy that may be blocking this request for its content; only a 401 logs at
    `error` as a rejected key), a body over the size cap (the server echoes the context, so a large context can
    push the response over it), and a request body over 64 KiB (`MAX_REQUEST_BYTES`), which is
    refused before sending rather than left to fail mid-upload as a transport error;
  - per client: everything else that follows a request (transport error or timeout, 401, 408,
    429, 5xx, a body that is not JSON or has no boolean `enabled`, a read error mid-body),
    plus an unparseable `base_url` (a configuration error that never heals and would otherwise
    log on every lookup).
  An invalid flag key starts no cooldown: it sends no request and is already refused cheaply.
- **`Retry-After`:** honoured on 429 and 503 only, in its delta-seconds form. The window is
  `max(failure_cooldown, min(Retry-After, 60 s))`: never shorter than the configured cooldown and
  capped at 60 s, so a proxy cannot turn flags off for hours. The HTTP-date form, or an
  unparseable value, uses the configured cooldown (no date-parsing dependency for a form the
  server never sends).
- **During a cooldown** a lookup returns the default result (`reason: "error"`, uncached, as for
  any failure) without a request, logging at `debug` only. The first lookup after the window ends
  sends a request; if it fails, a new window starts with one new `warn` (or `error` for a rejected
  key). So an outage logs one line per window per client (plus, at most, one per lookup already
  in flight when the window started).
- **Per-flag state is bounded:** at most 256 flag entries; expired entries are pruned before an
  insert, and when the map is still full the entry is not recorded (that flag then just asks the
  server each time, as today). Host code can build keys from input, so the map must not grow
  without bound.
- **Body cap:** 1 MiB, an internal constant (`MAX_RESPONSE_BYTES`). Checked against
  `Content-Length` before reading when present, and against the bytes read so far while reading
  chunk by chunk otherwise, so no more than the cap plus one chunk is held. Not configurable
  (YAGNI).
- Cooldown uses `std::time::Instant` (monotonic); the logic takes `now` as a parameter so it is
  unit-tested without sleeping.
- `record` is called only on a failure; a success never touches a window (a request already in
  flight when another lookup opened a window may still succeed, and is used as usual).
- There is no single-probe state: every lookup that arrives after a window ends, before a new
  failure is recorded, sends a request.
- The cooldown is per `FlagClient` and shared by its clones (it lives behind an `Arc`, like the
  cache).

## 4. Goal & Success Criteria

A Rust SDK client facing an outage, a portal, or a misbehaving proxy costs at most one request and
one warning per cooldown window, and never reads more than about 1 MiB of a response.

- After a client-wide failure, lookups for any flag within the window send no request and return
  the default; the first lookup after the window reaches the server (wiremock test).
- A 503 or 429 with `Retry-After: N` holds the window for `N` seconds (bounded); test.
- A 404 for one flag does not stop lookups of another flag; a 400 or 413 opens no window; test.
- A body over 1 MiB (with and without `Content-Length`) is a failed lookup that opens no window;
  test.
- `Duration::ZERO` restores the #109 behavior (every lookup asks); existing tests pin it.

## 5. Scope

In: `packages/rust-server` (`src/backoff.rs` new, `src/client.rs`, `src/types.rs`, tests,
README), this spec and its plan.

Out: the server; other SDKs; retrying a failed lookup; serving stale cache entries during a
cooldown; telemetry backoff (`telemetry.rs` already batches on a timer).

## 6. Architecture

- **`src/backoff.rs` (new, crate-private).** `Backoff { cooldown: Duration, state:
  Arc<Mutex<State>> }`, `State { client_until: Option<Instant>, flags: HashMap<String, Instant> }`,
  `enum Scope { Client, Flag, Request }`.
  - `fn remaining(&self, flag: &str, now: Instant) -> Option<Duration>`: the time left in the
    client window, else in the flag's window; `None` when lookups may go out (always `None` when
    `cooldown` is zero).
  - `fn record(&self, flag: &str, scope: Scope, retry_after: Option<Duration>, now: Instant) ->
    Duration`: starts the window (length per §3), returns it for the log line, or zero when
    nothing was recorded (`Scope::Request`, a full flag map, an overflow). A later
    `client_until` is never shortened.
  - `fn parse_retry_after(value: &str) -> Option<Duration>`: delta-seconds only.
  - A poisoned mutex is treated as "no cooldown" (lookups go out; never panic in the host).
- **`src/client.rs`.**
  - `FlagClient` gains `backoff: Backoff`, built from `config.failure_cooldown`.
  - `evaluate`: after the flag-key check and the cache lookup (a cached answer is still served
    during a cooldown), before `fetch`: if `backoff.remaining(flag_key, now)` is `Some`, log at
    `debug` and return `get_default_result`. On `fetch` error: `backoff.record(...)`, then the
    existing `warn`/`error` line, extended with the window length.
  - `Failure` gains `scope: Scope` and `retry_after: Option<Duration>`; `fetch` sets them (404 → `Flag`;
    other 4xx except 401/408/429, oversized response or request → `Request`; 429/503 read `Retry-After`).
  - `fetch` reads the body through a new private `read_capped(response, MAX_RESPONSE_BYTES)` using
    `Response::content_length` and `Response::chunk`, then `serde_json::from_slice`.
- **`src/types.rs`.** `Config.failure_cooldown` (doc comment), default 5 s,
  `with_failure_cooldown`.

Public interface changes: `Config::failure_cooldown` field and `Config::with_failure_cooldown`
(additive in practice, §3). Behavior change: during a cooldown a lookup returns the default
without asking the server. The REST contract and request shape are unchanged.

## 7. Invariants impact

- **SDK falls back, never throws:** unchanged; a skipped lookup is the same default result.
- **SDK REST contract stable:** no wire change; the SDK sends fewer requests.
- **Licensing:** MIT package; no server code copied (the 404 code is read, not copied).
- Others (migrations, RLS, MCP, metering, server) are not touched.

## 8. Migration

None.

## 9. Out-of-band

None. Not published (release is Rob's decision; no version bump). Merging redeploys unchanged
server code: post-merge verification is health only.

## 10. Error handling & edge cases

- Concurrent lookups that were in flight when the window started still complete and may each log
  once; new lookups are skipped.
- Clock: `Instant` is monotonic; `checked_add` overflow (absurd cooldowns) saturates to "no
  window" rather than panicking — the builder documents a few seconds.
- A `Retry-After` of `0` gives the configured cooldown (the max rule).
- Body exactly at the cap is accepted; one byte over is refused.
- A rejected key (401/403) still logs at `error`, once per window.
- Cache hits are still served during a cooldown; a skipped lookup is the same un-overridden
  default as any other failure.
- The `std::sync::Mutex` in `Backoff` is only locked inside the sync `remaining` and `record`,
  never across an `.await`.

## 11. Testing approach

Gates: `packages/rust-server` `cargo fmt --check`, `cargo clippy --locked --all-targets -- -D
warnings`, `cargo test --locked`; `examples/rust-server` `cargo fmt --check` and `cargo clippy
--locked --all-targets -- -D warnings` (the example must still compile against the SDK).

- Unit tests in `src/backoff.rs` with injected `now`: client window blocks every flag, flag window
  blocks only that flag, windows end, `Retry-After` max/min rule, zero cooldown never blocks,
  per-flag map bound and pruning, `parse_retry_after`.
- Wiremock tests in a new `tests/backoff_test.rs`: client-wide skip then first lookup after the
  window reaches the server (short cooldown, e.g. 300 ms, asserted by request counts);
  `503` + `Retry-After: 1` holds past the configured cooldown; a 404 on one flag leaves another
  flag's lookup going out; an oversized body with and without `Content-Length` falls back.
- Existing tests that expect each failed lookup to reach the server use
  `with_failure_cooldown(Duration::ZERO)`.
- Log bounding is covered by construction (the skip path logs at `debug`) and by the unit tests of
  the window; no `tracing` capture test (the crate has no subscriber dev-dependency).

## 12. Risks & open questions

- A flag configuration over 1 MiB now falls back to its default (with a warning naming the cap).
  The server has no configuration size limit; #115 proposes one.
- A 5 s client-wide window after a single transient failure turns all uncached flags to their
  defaults for 5 s. That is the trade the ticket asks for; `with_failure_cooldown` tunes it.
