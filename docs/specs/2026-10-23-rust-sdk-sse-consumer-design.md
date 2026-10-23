# Rust SDK: consume the flag-change stream

**Status:** APPROVED
**Ticket:** savvagent/otto-flags#167 (first of five per-SDK PRs, as the ticket directs; this one is Rust only)

## Brief

#167, verbatim parts that apply to this PR:

> The server streams flag changes only over server-sent events: `GET /api/flags/stream`. Every SDK
> should consume it, but only three do [...] Rust, Go and Java document and enable `enableRealtime`
> by default [...] but nothing in them reads the setting. A service using them believes it gets live
> updates and does not, so a kill switch waits for the cache TTL.
>
> Connect to `GET /api/flags/stream` with the SDK key, parse the flag-change events the server sends
> [...] and invalidate the changed flag's cache entries through the generation counters. Reconnect
> with backoff and jitter; fall back to the TTL (server SDKs) [...] while the stream is down. Honour
> `enableRealtime`; when it is off, do not open the stream. Never throw into the host app and never
> block a flag lookup on the stream.
>
> AC: each SDK consumes the stream when `enableRealtime` is on, with a test that a flag-change event
> invalidates the cached evaluation; a test for reconnect after a dropped stream and for the
> fallback while the stream is down; no SDK opens a stream when `enableRealtime` is false; a lookup
> never throws or waits on the stream; READMEs and `sdk_guide` text describe real-time behaviour
> accurately.

## Premise corrections

- The ticket says "one PR per SDK". This PR is the Rust SDK only. Go, Java, Swift and Kotlin
  cannot be compiled on this machine (no `go`, `mvn` or `swift` here; the section forbids installing one), so their
  consumers would ship unverified. They stay open on #167.
- `Config::enable_realtime` exists and defaults to `true`, but has no builder method, so a host
  cannot turn it off without struct-update syntax. This PR adds `Config::with_realtime(bool)`
  (additive).
- The stream carries events `connected`, `heartbeat`, `flag.created`, `flag.updated`,
  `flag.deleted` (`crates/flags-api/src/stream.rs`); each flag event's JSON has `key`.

## Assumptions

- Run Profile values all come from CLAUDE.md; none assumed.
- The PR body says `Refs #167`, not `Fixes #167`, although the section says the body must contain
  `Fixes #N`: merging with `Fixes` would close #167 with four SDKs undone. #167 stays open.
- No new dependency. `reqwest 0.11` `Response::chunk()` streams the body; the SSE framing is a
  small parser in the crate. Jitter uses `std::hash::RandomState`, not a `rand` crate.
- The stream is started by `FlagClient::new` when a Tokio runtime is current, else lazily by the
  first `evaluate` (the same pattern as the telemetry flusher, `telemetry.rs` `ensure_flusher`).
- One stream per client family (all clones share it); it ends when the last clone is dropped.
- A missed event is repaired by dropping the whole cache on reconnect, and the TTL bounds
  staleness while the stream is down; no polling is added to the Rust SDK.

## Goal & Success Criteria

With `enable_realtime` on (the default), a flag change reaches the cache in about a round trip
instead of after `cache_ttl`.

- A `flag.updated` / `flag.created` / `flag.deleted` event drops the flag's cached results for every
  context and bumps its generation (via `Cache::invalidate(Some(key))`), so an evaluation in flight
  when it arrived is not cached (#137).
- A dropped or failed stream reconnects with exponential backoff and jitter; after a reconnect the
  whole cache is dropped, since events may have been missed.
- `with_realtime(false)` opens no stream.
- `evaluate` never waits on, or fails because of, the stream.

## Scope

In: `packages/rust-server` (new `src/realtime.rs`, `client.rs`, `types.rs`, tests, README,
crate docs); the Rust `sdk_guide` text (`crates/flags-mcp/content/guides/rust.md`).
Out: Go, Java, iOS, Android SDKs; server stream endpoint and event format; WebSocket.

## Architecture

- `realtime.rs`: `Realtime` (Arc'd `Inner` with a `running` flag and a shutdown `Notify`, as in
  `telemetry.rs`). `ensure_started(cache, http, url, key)` spawns one task on the current runtime.
  The task holds a `Weak<Inner>` so dropping every client ends it (`Inner::drop` notifies).
- The task loop: connect `GET <base_url>/api/flags/stream` with `Authorization: Bearer <key>` and
  `Accept: text/event-stream`, using a dedicated `reqwest::Client` with `connect_timeout =
  config.timeout` and no total timeout (the evaluate client's total timeout would kill the stream).
  Read with `chunk()` under an idle timeout of 90 s (three heartbeats). Feed bytes to `SseParser`;
  on each event: `flag.*` with a valid key (`client::is_valid_flag_key`) invalidates that key;
  `connected` invalidates everything (a result cached before the first connect, or events missed while down, last until the TTL otherwise); `heartbeat` and unknown events are ignored.
- `SseParser`: `feed(&[u8]) -> Result<Vec<SseEvent>, TooLarge>`; handles `\n`, `\r\n`, comments,
  multi-line `data`, a leading space after the colon. A pending event over 4 MiB is an error that
  ends the connection (one event carries one flag with up to 20 variations and their configurations;
  the cap is generous but finite).
- Backoff: `reconnect_delay(attempt, unit)` = `min(1 s * 2^attempt, 30 s)` scaled into
  `[50%, 100%]` by `unit` (a random value in `[0,1)`). The attempt counter resets only when a
  connection stayed up at least 30 s, so a server that accepts and drops at once is not retried
  at the base delay forever. Stream failures never feed the evaluate `Backoff`/`failure_cooldown`. HTTP 401, 403 and 404 use a 5 min cap (the key is rejected or the
  endpoint absent; retrying fast changes nothing); 429 and 503 honour `Retry-After` (capped at
  60 s, never shortening the backoff).
- `client.rs`: `FlagClient` gains `realtime: Realtime`; `new` and `evaluate` call
  `ensure_started` when `config.enable_realtime`. The call is an atomic load on the hot path.
- Logging through `tracing`: connect/disconnect at `info`/`debug`, a failed connect at `warn`
  (at `error` for 401), never the key.

Public interface: additive only (`Config::with_realtime`). No wire change.

## Invariants impact

- **The SDK path never calls the platform** / **SDK REST contract stable**: only the documented
  stream endpoint is used; no server change.
- **Client keys are public / SDK-reported text untrusted**: the key is sent in the `Authorization`
  header, never the URL, never logged. Stream event text is used only as a flag key after
  `is_valid_flag_key`; it is never echoed to an agent.
- **SDKs fall back to defaults and never throw into the host app**: the task swallows and logs every
  error; `evaluate` does no I/O for the stream.
- **Small shared database**: each stream is one long-lived HTTP connection on the server and holds
  no pool connection (`stream.rs` uses `begin_live` per notification only). Open-stream cost grows
  with the number of SDK processes; one stream per client family keeps it at one per process.
- Not touched: migrations, RLS, metering, bucketing, webhooks, licensing (the crate stays MIT; no
  server code is copied).

## Migration

None.

## Out-of-band

None: no secret, scope, config key or registration. The `sdk_guide` text change under `crates/`
deploys with the server on merge (`ci-auto`). The Rust crate is unpublished; no version bump.

## Error handling & edge cases

- No Tokio runtime at construction: start lazily on the first `evaluate`. A new runtime after the
  old one is dropped gets a new task (`running` resets on task drop, as in `telemetry.rs`).
- Server closes the stream cleanly: treated as a drop; reconnect.
- Malformed event JSON or an invalid key: skipped, logged at `debug` with a bounded prefix.
- Lagged server subscriber skips events (documented in `stream.rs`): covered by the TTL, which
  stays in force the whole time.
- `base_url` invalid: `new` parses the stream URL once; if it fails, it logs one `warn` and no
  stream is ever started (`evaluate` already falls back per lookup). Lookups are unaffected.

## Testing approach

Gates: `cd packages/rust-server && cargo fmt --check`, `cargo clippy --locked --all-targets -- -D
warnings`, `cargo test --locked`; for the `crates/` guide text, the workspace gates and the
container build. Unit tests beside the code (`realtime.rs`): parser framing cases, the backoff
function, key filtering. Integration tests in `tests/realtime_test.rs` against `wiremock`:
(1) an update event invalidates a cached evaluation, (2) a dropped stream is reconnected and the cache is cleared by the new `connected`,
(3) while the stream is down lookups are served by the server and the TTL still expires,
(4) `with_realtime(false)` makes no stream request, (5) a lookup returns promptly while the stream
response is delayed, (6) a request carries the bearer header and `Accept`.

## Risks & open questions

- Cost: an open stream keeps a suspended Fly machine awake, and each deploy drops every stream
  (every client then clears its cache). The JS, Node and Python consumers already do this; the
  default-on Rust consumer adds to it.
- Timing-based integration tests: delays and polling windows are generous (seconds) to avoid
  flakes; the reconnect test waits for the first backoff (0.5 to 1 s).
- Go, Java, iOS and Android remain: #167 stays open.
