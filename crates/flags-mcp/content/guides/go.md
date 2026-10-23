# otto-flags Go SDK (app: {{APP_NAME}})

`github.com/savvagent/otto-flags/packages/go-server` (package `ottoflags`) evaluates flags for a Go
service. This SDK accepts only the app's public client key (it refuses any other key at
construction), so it is not a secret and can sit in configuration; it is shown by `list_apps` and
filled in below when you pass the app to this tool. The app's server key is not used here. Go 1.21
or later.

## Install

This module is not installable by version tag yet. Install it from source, pinned to a full commit
SHA of `savvagent/otto-flags`. Find the current tip of `main` with:

```bash
git ls-remote https://github.com/savvagent/otto-flags main
```

Ask a person to review and approve the SHA before you pin it, or use the SHA of a commit they have
already checked. It must be a commit on `main` of `savvagent/otto-flags`, not one from a fork. Use
the approved SHA in place of `<commit>`:

```bash
go get github.com/savvagent/otto-flags/packages/go-server@<commit>
```

Go records the commit in `go.mod` as a pseudo-version, so a build is reproducible. Pin a SHA, not a
branch name.

## Initialize

Create one client per process and reuse it; it is safe for concurrent use and owns the cache.

```go
package main

import (
    "log"

    ottoflags "github.com/savvagent/otto-flags/packages/go-server"
)

func main() {
    client, err := ottoflags.NewClient(ottoflags.Config{
        APIKey:        "{{CLIENT_KEY}}",
        ApplicationID: "{{APP_ID}}",
        Defaults:      map[string]bool{"new-checkout": false},
        OnError:       func(err error) { log.Printf("otto-flags: %v", err) },
    })
    if err != nil {
        log.Fatal(err) // a missing or malformed key fails at start-up
    }
    defer client.Close()
}
```

Other `Config` fields, all optional: `BaseURL` (default `https://otto-flags.savvagent.com`),
`CacheTTL` (default 60 seconds), `Timeout` (default 5 seconds), `EnableRealtime` (default off, see
below), and `Defaults` (flag key to the value served when evaluation fails). `OnError`
defaults to printing to standard output, so set it.

**Environment.** The `environment` this app evaluates in, wherever this guide sets it (the client
configuration or a context), must be one of the app's environments: {{ENVIRONMENTS}}. Read it from the
build's or deployment's configuration instead of hardcoding it.

## Evaluate a flag

Pass a `*ottoflags.Context` describing who the evaluation is for. Percentage rollouts bucket on
`UserID` (or `SessionID`), so pass a stable identifier. A nil context is allowed.

```go
ctx := &ottoflags.Context{
    UserID:      "user-123",
    Environment: "production",
    Attributes:  map[string]interface{}{"plan": "pro"},
}

if client.IsEnabled("new-checkout", ctx) {
    renderNewCheckout()
} else {
    renderOldCheckout()
}

result, err := client.Evaluate("new-checkout", ctx)
if err != nil {
    log.Printf("evaluate: %v", err) // result is still usable: it carries the default
}
_ = result.Value // result.Variation, result.Configuration, result.Reason
```

Also available: `GetConfig(flagKey, ctx)` for configuration attached to an enabled flag and
`GetVariation(flagKey, ctx)` for multi-variant flags. `InvalidateCache(flagKey)` drops a flag's
cached results; an empty key clears them all.

Results are cached per flag and per evaluation context for `CacheTTL`, so two users never share a
cached result.

## Frameworks

### net/http

```go
http.HandleFunc("/checkout", func(w http.ResponseWriter, r *http.Request) {
    ctx := &ottoflags.Context{UserID: r.Header.Get("X-User-ID")}
    if client.IsEnabled("new-checkout", ctx) {
        w.Write([]byte("new checkout"))
        return
    }
    w.Write([]byte("old checkout"))
})
```

### Gin

Build the client once in `main` and reach it from handlers:

```go
r := gin.Default()
r.GET("/checkout", func(c *gin.Context) {
    ctx := &ottoflags.Context{UserID: c.GetHeader("X-User-ID")}
    c.JSON(200, gin.H{"newCheckout": client.IsEnabled("new-checkout", ctx)})
})
```

## Notes

- A flag lookup does not panic on a network or server error: `Evaluate` returns a result carrying
  the value from `Defaults` (or `false`) with `Reason == "error"` together with the error, and
  `IsEnabled` returns just that value. Always use the result even when `err` is set. An invalid
  flag key (1 to 128 ASCII letters, digits, `-`, `_` and `.`) is refused the same way without a
  request.
- `EnableTelemetry` is a field of `Config`, but this SDK does not act on it: it reports no
  evaluation counts or errors, so `flag_health` and auto-rollback get no data from it.
- Real-time updates are off unless you set `EnableRealtime: true` (a Go `bool` cannot default to
  true). With it on, a change made with `set_flag_environment` or `rollback_flag` reaches the app
  within about a round trip: the client reads the server's flag-change stream in a background
  goroutine and drops the changed flag's cached results. The stream never delays or fails a
  lookup; while it is down, or with `EnableRealtime` off, a change reaches the app when the cached
  result expires (`CacheTTL`). It reconnects with backoff and drops the whole cache on every
  connection. `Close` stops it, and `OnError` may be called from its goroutine.
- Create flags before code references them: an unknown flag key evaluates to its default.
- The wire contract the SDK speaks is `docs/SDK-DEVELOPER-GUIDE.md` in the repository.
