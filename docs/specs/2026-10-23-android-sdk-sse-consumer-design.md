# Android SDK: consume the flag-change stream

**Status:** DRAFT
**Ticket:** savvagent/otto-flags#167 (fourth of five per-SDK PRs; after Rust #170, Go #171, Java #173)

Applies the approved design of `2026-10-23-rust-sdk-sse-consumer-design.md` to
`packages/android-sdk`; only the Android-specific parts are here. The Java port
(`2026-10-23-java-sdk-sse-consumer-design.md`) is the closest reference.

## Premise corrections

- The ticket says Android "has no TTL, so it never refreshes a cached evaluation except through
  polling". Correct: `OttoFlagsClient` polls `GET /api/sdk/flags` every `pollingInterval`
  (60 s default) and drops cached results (#163); the cache has no expiry otherwise.
- The leftover `enableWebSocket` config the ticket says to remove is already "ignored, kept so
  callers compile" (#163). It stays as it is.
- There is no `enableRealtime` option in `OttoFlagsConfig`; this PR adds one.

## Assumptions

- `OttoFlagsConfig.enableRealtime: Boolean = false`, added as the LAST parameter so positional
  and named callers keep compiling. **Default off**: a held connection costs battery and radio
  wake-ups on a phone and a server connection per install, and the poll already bounds staleness
  to `pollingInterval`. Apps that need kill switches fast set it to true. (The server SDKs default
  on.)
- Polling stays on whatever the stream does: it is the fallback while the stream is down, and it
  repairs any missed event.
- No new dependency: a small hand-written reader over the existing `okhttp` (bounded lines via
  `BufferedSource.readUtf8LineStrict(limit)`), not `okhttp-sse`.
- No lifecycle integration: the stream runs while the client is open. Apps should call `close()`
  when backgrounding, or build the client with `enableRealtime` only for foreground use. Documented.
- Gates: no Gradle wrapper in the repo, so `packages/android-sdk` has no gate in CLAUDE.md. The
  dev host has a Kotlin compiler (Android Studio) but no configured Gradle build; compile and
  tests are run as far as the host allows and the PR says exactly what ran.
- `Refs #167`, not `Fixes`.

## Design

- `RealtimeStream.kt` (internal): `SseParser` (feed lines or bytes, emit `SseEvent(event, data)`;
  comment lines, `\r\n`, multi-line data, 4 MiB cap per event) and a `RealtimeStream` class owning
  a coroutine launched on the client's `scope`, cancelled by `close()`.
- Connect `GET <apiUrl>/api/flags/stream` (path prefix kept, query dropped) with
  `Authorization: Bearer <sdkKey>` and `Accept: text/event-stream`, using
  `okHttpClient.newBuilder()` with `readTimeout(90 s)` (idle watchdog; heartbeats come every 30 s),
  `callTimeout(0)`, `followRedirects(false)`, `followSslRedirects(false)`. Read in
  `Dispatchers.IO`; cancel the `Call` when the coroutine is cancelled. A 2xx whose content type is
  not `text/event-stream` is a refused attempt.
- Events: `connected` -> `invalidateCache(null)`; `flag.created|updated|deleted` -> parse `data` as
  JSON, take string `key`, if `isValidFlagKey` -> `invalidateCache(key)`; everything else ignored;
  malformed data skipped.
- Reconnect: delay `min(1 s * 2^attempt, cap)` scaled into [50%, 100%] by jitter; cap 30 s, 5 min
  after 401/403/404; `Retry-After` (seconds, capped 60 s) on 429/503 never shortens; the attempt
  counter resets only after a connection stayed up >= 30 s; after a connection that was up add a
  random 0 to 5 s to the first retry. Timings are internal constructor parameters so tests use
  milliseconds.
- Failures are logged through the client's existing `logError` (never the key) once per failed
  attempt run; nothing is thrown into the host; `evaluate` is untouched.

## Invariants impact

Same as the Rust spec: key only in the header; stream text used only as a cache key after
`isValidFlagKey`; additive API (one new defaulted config parameter, last); MIT package; no server
code copied; no version bump; no new dependency. Out-of-band: none. The `sdk_guide` text under
`crates/` deploys the server on merge (`ci-auto`).

## Testing

`MockWebServer` + JUnit 4 tests in `OttoFlagsClientRealtimeTest` (style of
`OttoFlagsClientNetworkTest`): a `flag.updated` event drops a cached evaluation; a dropped stream
reconnects and `connected` clears the cache; while the stream returns 503 lookups still work and
the poll still refreshes; no request to the stream path when `enableRealtime` is false (the
default); a lookup is prompt while the stream response is held; headers and no query; events for
another flag and an invalid key leave the cache alone; `close()` stops reconnecting; SseParser
table test across chunk boundaries; delay function bounds. Docs: README, the config KDoc,
`crates/flags-mcp/content/guides/android.md`.
