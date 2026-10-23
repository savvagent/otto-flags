# iOS SDK: consume the flag-change stream

**Status:** DRAFT
**Ticket:** savvagent/otto-flags#167 (fifth and last of the per-SDK PRs; after Rust #170, Go #171, Java #173, Android #177)

Applies the approved design of `2026-10-23-rust-sdk-sse-consumer-design.md` to `packages/ios-sdk`;
only the iOS-specific parts are here. The Android port
(`2026-10-23-android-sdk-sse-consumer-design.md`) is the closest reference (also a mobile SDK
with a polling fallback and `enableRealtime` default off).

## Premise corrections

- The ticket says iOS "has no TTL, so it never refreshes a cached evaluation except through
  polling". Correct: `OttoFlagsClient` polls `GET /api/sdk/flags` every `pollingInterval`
  (60 s) and drops cached results (#163); the cache has no expiry.
- The ticket says `URLSession.bytes` needs iOS 15 while `Package.swift` declares iOS 13 and asks
  for a recorded choice. **Choice: a `URLSessionDataDelegate`, deployment targets unchanged**
  (raising the targets is a breaking change for every host app, for a feature that is off by
  default). The same reasoning as #119's `fetchData` wrapper in `URLSession+Compat.swift`.
- The leftover `enableWebSocket` is already ignored and kept for source compatibility (#163).
- The client has no `close()`; polling stops only when the client is deallocated. This PR adds
  `close()` (additive) so a host can stop the stream without dropping the client.

## Assumptions

- `OttoFlagsConfig.enableRealtime: Bool = false`, added as the LAST `init` parameter with a default
  so existing callers compile. **Default off**, as on Android: a held connection costs battery and
  radio wake-ups on a phone and a server connection per install, and the poll already bounds
  staleness to `pollingInterval`. Apps that need kill switches fast set it to true.
- Polling stays on whatever the stream does (fallback and repair of missed events).
- No new dependency; Foundation only (`URLSession` + `URLSessionDataDelegate`), working on the
  declared platforms (iOS 13, macOS 10.15, tvOS 13, watchOS 6) and, via `FoundationNetworking`, on
  Linux.
- No lifecycle integration: the stream runs while the client is open and not closed. Apps should
  call `close()` when backgrounding. Documented.
- **Gates:** `cd packages/ios-sdk && swift test` is the section's gate and **cannot run here: there
  is no Swift toolchain on the dev host, and the section forbids installing one.** The gate is also
  already known red (savvagent/otto-flags#119, never compiled). Nothing in this PR can be compiled
  or run; the PR says so. The code is written for Swift 5.9, kept simple, and re-read for compile
  and concurrency errors.
- `Refs #167`, not `Fixes` (this is the last SDK PR; the tracking issue is closed by hand once the
  toolchain-less SDKs have been run on a machine with their toolchains).

## Design

- `RealtimeStream.swift` (internal): `SSEParser` (struct; `mutating func feed(_ data: Data)
  throws -> [SSEEvent]`; comments, `\r\n`, multi-line data, 4 MiB cap per event; bytes buffered and
  a line decoded only once complete so a split UTF-8 character survives) and `RealtimeStream`
  (final class) with an internal reconnect-delay function.
- `RealtimeStream` owns a dedicated `URLSession` built with `URLSessionConfiguration.default`
  copy: `timeoutIntervalForRequest = 90` (idle watchdog: for a session task this is the time
  between data packets; heartbeats arrive every 30 s), `timeoutIntervalForResource` very large,
  `httpShouldUsePipelining` unchanged, no cache (`urlCache = nil`, `requestCachePolicy =
  .reloadIgnoringLocalCacheData`), a delegate object, and a serial delegate `OperationQueue`. The
  session is created by an injectable factory (`(URLSessionDataDelegate, OperationQueue) ->
  URLSession`) so tests can supply `protocolClasses`.
- Request: `GET <apiUrl>/api/flags/stream` built with `URLComponents` (path prefix kept, no query),
  headers `Authorization: Bearer <sdkKey>`, `Accept: text/event-stream`, `Accept-Encoding:
  identity`.
- Delegate callbacks (all on the delegate queue; state guarded by one lock or by confining it to
  that queue): `didReceive response` -> require HTTP 2xx and a `Content-Type` containing
  `text/event-stream`, else cancel the attempt as refused (`completionHandler(.cancel)`) with the
  status recorded (and `Retry-After` for 429/503); `didReceive data` -> parser -> events;
  `willPerformHTTPRedirection` -> `completionHandler(nil)` (a redirect is a refused attempt; the
  key must not follow it); `didCompleteWithError` -> schedule the reconnect. The delegate holds the
  stream **weakly** (a `URLSession` retains its delegate strongly); the stream calls
  `invalidateAndCancel()` on stop and in `deinit`, so there is no retain cycle.
- Events: `connected` -> invalidate the whole cache; `flag.created|updated|deleted` -> parse `data`
  with `JSONSerialization` as `[String: Any]`, take String `key`, if the client's flag-key rule
  accepts it -> invalidate that flag; everything else ignored; malformed data skipped. Invalidation
  calls back into the client through a closure `(String?) -> Void` that does the same
  `queue.sync { invalidateCacheLocked(...) }` as the poll.
- Reconnect: delay `min(1 s * 2^attempt, cap)` scaled into [50%, 100%] by jitter; cap 30 s, 5 min
  after 401/403/404; `Retry-After` (seconds, capped 60 s) on 429/503 never shortens; the attempt
  counter resets only after a connection stayed up >= 30 s; after a connection that was up add a
  random 0 to 5 s to the first retry. Scheduling uses `DispatchQueue.asyncAfter` on a private
  serial queue, cancelled by a generation token on `stop()`. Timings are internal init parameters
  so tests use milliseconds.
- Failures are swallowed (`#if DEBUG` print at most; never the key); nothing is thrown into the
  host; `evaluate` is untouched; lookups never wait on the stream.
- `OttoFlagsClient` creates the stream in `init` when `config.enableRealtime`, stops it in
  `close()` (idempotent) and in `deinit`.

## Invariants impact

Same as the Rust spec: key only in the `Authorization` header; stream text used only as a cache key
after the flag-key check; additive API (`enableRealtime` last and defaulted, `close()`); MIT package;
no server code copied; no version bump; no new dependency. Out-of-band: none. The `sdk_guide` text
under `crates/` deploys the server on merge (`ci-auto`).

## Testing

XCTest in `RealtimeStreamTests.swift`, with a streaming `URLProtocol` stub (delivers headers, then
body chunks on demand, can fail or hold the connection) injected through the session factory: a
`flag.updated` event drops a cached evaluation; a dropped stream reconnects and `connected` clears
the cache; while the stream answers 503 lookups still work and the poll still refreshes; no
request to the stream path when `enableRealtime` is false (the default); a lookup is prompt while
the stream is held; headers and no query; events for another flag and an invalid key leave the cache
alone; a redirect is refused; `close()` cancels the connection and stops reconnecting; `SSEParser`
table test across every chunk boundary including inside a UTF-8 character; reconnect delay bounds.
Docs: README, the config doc comments, `crates/flags-mcp/content/guides/ios.md`.
