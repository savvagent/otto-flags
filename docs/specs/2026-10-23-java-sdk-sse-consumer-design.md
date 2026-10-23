# Java SDK: consume the flag-change stream

**Status:** DRAFT
**Ticket:** savvagent/otto-flags#167 (third of five per-SDK PRs; after Rust #170 and Go #171)

Applies the approved design of `2026-10-23-rust-sdk-sse-consumer-design.md` to
`packages/java-server`; only the Java-specific parts are here. The Go port
(`2026-10-23-go-sdk-sse-consumer-design.md`, merged) is the closest reference.

## Premise corrections

- `FlagClientConfig.enableRealtime` already defaults to `true` (builder) but nothing reads it.
- The ticket asks to remove a leftover `enableWebSocket` option; there is none in
  `packages/java-server` (nothing to remove).
- `okhttp-sse` (`okhttp3.sse`) is already a declared dependency, so no pom change is needed.

## Assumptions

- Use `okhttp3.sse.EventSources` / `EventSourceListener` rather than a hand-written parser: it
  is declared, and it cannot be compiled here. Its reader does not cap one event's size; the
  stream's peer is the user's configured `baseUrl`, and the residual risk (an `http` URL
  through a hostile network path) is recorded in the PR.
- The gates `mvn -B verify` cannot run here (no `mvn`/`javac`; only a JRE); the PR says so.
- `Refs #167`, not `Fixes`.
- The stream is on by default (the builder default `true` already documents it).

## Design

- `RealtimeStream` (package-private class in `com.savvagent.ottoflags.sdk`), created in the
  `FlagClient` constructor when `config.isEnableRealtime()`; stopped by `close()` (idempotent).
- Own `OkHttpClient` derived with `httpClient.newBuilder()`: `readTimeout(90 s)` (the idle
  watchdog: heartbeats arrive every 30 s), `callTimeout(0)`, `connectTimeout(config.getTimeout())`,
  `followRedirects(false)`, `followSslRedirects(false)`, `retryOnConnectionFailure(false)`.
- Request `GET <baseUrl>/api/flags/stream` built with `HttpUrl` (path prefix kept, query dropped),
  headers `Authorization: Bearer <key>`, `Accept: text/event-stream`. A response whose content type
  is not `text/event-stream` is a refused attempt.
- Listener: `onOpen` records the connect time; `onEvent(type, data)`: `connected` ->
  `invalidateCache(null)`; `flag.created|updated|deleted` -> parse `data` with Gson as a
  `JsonObject`, take string `key`, if `isValidFlagKey` -> `invalidateCache(key)`; else ignore. A
  `JsonParseException`/`ClassCastException`/`IllegalStateException` skips the event. `onClosed` and
  `onFailure` schedule a reconnect.
- Reconnect on a single-thread `ScheduledExecutorService` (daemon thread): delay
  `min(1 s * 2^attempt, cap)` scaled into [50%, 100%] by jitter; cap 30 s, 5 min after 401/403/404;
  `Retry-After` (seconds, capped 60 s) on 429/503 never shortens; attempt counter resets only
  after a connection stayed up >= 30 s; after a connection that was up add a random 0 to 5 s to the
  first retry. Timings are constructor parameters of `RealtimeStream` (package-private) so tests use
  milliseconds.
- `onError` is called at most once per failed attempt run (a failed connect, not a normal drop).
  It may run on the stream thread; document that.
- Never throws into the host: everything in the listener and scheduler is caught and logged
  through SLF4J (never the key). `evaluate` is untouched; only `invalidateCache` runs from the
  stream thread (it already takes `generationLock` briefly).

## Invariants impact

Same as the Rust spec. Public API additive: none new (a package-private class; `enableRealtime`
becomes effective). MIT package, no server code copied, no version bump, no new dependency.
Out-of-band: none. The `sdk_guide` text under `crates/` deploys the server on merge (`ci-auto`).

## Testing

`MockWebServer` tests in `RealtimeStreamTest` (JUnit 5): a `flag.updated` event drops a cached
evaluation (response body without `connected`, with a body delay so the evaluate happens first);
a dropped stream reconnects and `connected` clears the cache; lookups served and TTL-bounded while
the stream returns 503; no request to the stream path when `enableRealtime(false)`; a lookup is
prompt while the stream response is held; request headers and no query; events for another flag
and an invalid key leave the cache alone; `close()` stops reconnecting and is idempotent; delay
function bounds. Existing tests already pass `.enableRealtime(false)`. Docs: README, the
`FlagClientConfig` javadoc, `crates/flags-mcp/content/guides/java.md` (its line saying the SDK does
not act on `enableRealtime`).
