# Go SDK: consume the flag-change stream

**Status:** IMPLEMENTED (#171)
**Ticket:** savvagent/otto-flags#167 (second of five per-SDK PRs; follows the Rust SDK, #170)

This applies the approved design of `2026-10-23-rust-sdk-sse-consumer-design.md` to
`packages/go-server`. Read it for the rationale; only the Go-specific parts are here.

## Premise corrections

- `Config.EnableRealtime` is documented as "default: true" but is a plain `bool`: Go cannot tell
  `false` from unset, so nothing can default it on without a breaking change, and nothing reads it.
- The SDK key check accepts only `sdk_` keys (`NewClient`); the stream uses the same key.

## Assumptions

- `EnableRealtime` stays a `bool` and the stream opens only when it is `true`. The default is
  therefore off, and the doc comment, README and `sdk_guide` text say so (a kill switch then needs
  `EnableRealtime: true`). Making it default-on needs a `*bool` or an inverted field, a breaking
  change left to a maintainer. No stream means no new baseline load on the server.
- Gates `go vet ./...` and `go test ./...` cannot run here (no Go toolchain, Go 1.21+ needed); the
  PR says so. The code is written for Go 1.21 (no range-over-func, no `min`/`max` builtins needed
  beyond 1.21).
- No new dependency (stdlib `net/http`, `bufio`).
- `Refs #167`, not `Fixes`.

## Design

- `realtime.go`: `startRealtime()` called from `NewClient` when `EnableRealtime`. One goroutine,
  stopped by `Close()` (a `context.Context` cancelled by `Close` and a done
  channel so `Close` returns promptly, once, and is safe to call twice).
- Connect `GET <BaseURL>/api/flags/stream` (path prefix kept, as the evaluate URL is built) with
  `Authorization: Bearer <key>` and `Accept: text/event-stream` using an `http.Client` with no total
  timeout (a `Transport` with `ResponseHeaderTimeout` 90 s, dial timeout `Config.Timeout`) and
  `CheckRedirect` returning `http.ErrUseLastResponse`. A 200 without `Content-Type` containing
  `text/event-stream` is a refused attempt. Read in 32 KiB chunks into a parser that caps one event (line, name and data) at 4 MiB, and an idle watchdog of 90 s that cancels the request.
- SSE framing: `event:` / `data:` fields, comments, `\r\n`, multi-line data, blank line dispatches.
- Events: `connected` clears the whole cache (`cache.Invalidate("")`); `flag.created|updated|deleted`
  decode `data` JSON, take string `key`, and if `isValidFlagKey` (flagkey.go) call
  `cache.Invalidate(key)`; everything else ignored. Unparsable data is skipped.
- Reconnect: delay `min(1 s * 2^attempt, cap)` scaled into [50%, 100%] by jitter; cap 30 s, 5 min after
  401/403/404; `Retry-After` on 429/503 (seconds, capped 60 s) never shortens the wait. The attempt
  counter resets only after a connection stayed up 30 s. After a connection that was up, add a
  random extra 0 to 5 s to the first retry (reconnects after a deploy must not arrive together).
  Stream failures never touch the evaluate path or its error handler except one `OnError` call per
  failed connect attempt run (rate-limited to the backoff).
- Evaluate is untouched: the goroutine only calls `cache.Invalidate`, which takes the cache lock
  briefly; a lookup never waits on the stream. While the stream is down, `CacheTTL` bounds staleness.
- Errors are never thrown to the host app; the key is never logged or put in a URL.

## Invariants impact

Same as the Rust spec: key only in the header; stream text used only as a cache key after
`isValidFlagKey`; additive public API (none new beyond making `EnableRealtime` effective; no new
exported symbol); MIT package, no server code copied; no version bump. Out-of-band: none. The
`sdk_guide` text under `crates/` deploys the server on merge (`ci-auto`).

## Testing

`httptest` servers in `realtime_test.go` (package `ottoflags`): (1) a `flag.updated` event drops
the cached evaluation (no `connected` in the body); (2) a dropped stream reconnects and `connected`
clears the cache; (3) lookups are served and the TTL expires while the stream returns 503; (4) no
request to the stream path when `EnableRealtime` is false; (5) a lookup returns promptly while the
stream response is held; (6) headers `Authorization` and `Accept`, no query; (7) events for another
flag and an invalid key leave the cache alone; (8) `Close` stops the goroutine and is idempotent;
(9) SSE parser table test across chunk boundaries; reconnect delay function bounds. Tests use small
injectable timings (unexported fields on the client) so none sleeps for seconds. Docs: README,
`types.go` comment, `crates/flags-mcp/content/guides/go.md` (its line about changes reaching the app
only when the cached result expires).
