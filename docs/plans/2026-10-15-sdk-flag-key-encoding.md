# SDK flag key encoding Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** Every SDK under `packages/` except Rust refuses a flag key outside the key rule with no
request and returns its failed-lookup result. Each SDK percent-encodes the key as one path
segment.

**Architecture:** Each SDK gets a private key check that mirrors the Rust SDK's
`is_valid_flag_key`. The check guards the top of `evaluate` (and of iOS/Android `getVariation`),
and the key segment goes through the platform encoder. Each SDK keeps its current endpoint path
(#112 fixes the paths).

**Tech Stack:** TypeScript (jest, pnpm workspace), Python (pytest, respx), Go, Java (JUnit 5,
OkHttp MockWebServer), Swift (XCTest, MockURLProtocol), Kotlin (JUnit 4, MockWebServer).

**Ticket:** #107

**Spec:** `docs/specs/2026-10-15-sdk-flag-key-encoding-design.md`

## Global Constraints

- **The key rule.** A key is valid iff it has 1-128 characters, each in `[A-Za-z0-9._-]`, and it
  is not `.` or `..`.
  - Match the whole string, with no trimming.
  - Python uses `re.fullmatch`, never `re.match` with `$`, because `$` also matches before a
    trailing `\n`.
  - A non-string key in JS is invalid.
- **Order in `evaluate`.** The check is the first thing in `evaluate`, before overrides, cache,
  context or telemetry. An invalid key makes no HTTP request and records no telemetry. It returns
  the SDK's failed-lookup result:
  - TS, Node, Python, Go, Java: the configured default (`defaults[key]`, else `false`), with
    reason `"error"`.
  - iOS: throw `OttoFlagsError.invalidFlagKey`.
  - Android: `Result.failure(OttoFlagsException(...))`.
  - iOS/Android `getVariation`: return the caller's `defaultValue`.
- **Error report.** The SDK reports the invalid key through its existing error channel, with a
  message giving an escaped prefix of at most 64 characters plus the key length:
  - TS/Node: `onError(new Error(...))`, using `JSON.stringify(prefix)`.
  - Python: `on_error(InvalidFlagKeyError(...))` if `on_error` is set, using `repr(prefix)`.
  - Go: `OnError(err)`, with `err` wrapping the exported sentinel `ErrInvalidFlagKey`, using `%q`.
  - Java: `logger.error` + `onError`, using `gson.toJson(prefix)`.
  - Android: `logError`, using a hand-written escaper (printable ASCII other than `"` and `\`
    kept, everything else `\uXXXX`). Never `org.json`, which is a stub in local unit tests.
  - iOS: the thrown error.
- **Encoding.** Encode the key segment with:
  - TS/Node: `encodeURIComponent`
  - Python: `urllib.parse.quote(key, safe="")`
  - Go: `url.PathEscape`
  - Swift: `addingPercentEncoding(withAllowedCharacters:)` with a `CharacterSet` of exactly the
    RFC 3986 unreserved ASCII characters (`A-Z a-z 0-9 - . _ ~`). Do not use `.alphanumerics`,
    which is Unicode-wide.
  - Java/Kotlin: build the URL with OkHttp `HttpUrl` from the base URL plus `addPathSegment` for
    each segment.

  Keep each SDK's existing path text. A valid key must produce the same URL as before. The Java
  and Kotlin `HttpUrl` change additionally keeps a base-URL path prefix and avoids `//` after a
  trailing slash.
- **Gates.** Run from the worktree root
  `/home/robhicks/dev/otto-flags/.claude/worktrees/fix-107-sdk-flag-key-encoding`:
  - pnpm (TS/Node): `pnpm install --frozen-lockfile && pnpm lint && pnpm build && pnpm test`
  - Python: `v=$(mktemp -d)/venv && python3 -m venv "$v" && "$v/bin/pip" install -q -e 'packages/python-server[dev]' && cd packages/python-server && "$v/bin/pytest" -p no:cacheprovider && "$v/bin/mypy" --cache-dir=/dev/null src && "$v/bin/ruff" check --no-cache .`
    (`TMPDIR` must point under `/tmp/claude-1000/-home-robhicks-dev-otto-flags/70c8b960-e3dc-4420-87f1-4f9d3e054741/scratchpad/tmp`, because `/tmp` is nearly full)
  - Go (`go vet ./...`, `go test ./...`), Java (`mvn -B verify`) and Swift (`swift test`) are not
    run: there is no toolchain on this host and none may be installed. Android has no gate.
    Self-check those files by careful reading instead: imports, types and nullability.
- **Evidence:** pass/fail/skip counts per test binary or package. A skipped test is not a pass.
- **Invariants:**
  - SDKs never throw into the host app on a flag lookup (iOS/Android keep their existing failure
    channels).
  - The SDK REST contract is unchanged for valid keys.
  - `packages/` is MIT, and no server code is copied in.
  - Do not touch `packages/rust-server`, the server crates, or any version file.
- **Commits.** Worktree only; trunk only via PR. Tasks run in parallel in one worktree, so an
  implementer **does not run any git write command** (no add, commit, stash or checkout). The
  orchestrator commits each task's files as
  `fix(sdk): <imperative, lower-case>`, with a why-body ending `Fixes #107`.
- No attribution of any kind.

## File Structure

| File | Responsibility |
|---|---|
| Modify `packages/typescript/src/client.ts`; test `packages/typescript/tests/client.test.ts` | key check + `encodeURIComponent` |
| Modify `packages/node-server/src/client.ts`; test `packages/node-server/src/client.test.ts` | same |
| Create `.changeset/<name>.md` (×2, patch: `@otto-flags/sdk`, `@otto-flags/node-server`) | release notes |
| Create `packages/python-server/src/otto_flags/_flag_key.py`; modify `exceptions.py`, `__init__.py`, `client.py`, `async_client.py`; tests in `tests/test_client.py`, `tests/test_async_client.py` | shared check, `InvalidFlagKeyError`, guard + `quote` |
| Modify `packages/go-server/client.go` (or new `flagkey.go`); test `packages/go-server/client_test.go` | `ErrInvalidFlagKey`, check, `url.PathEscape` |
| Modify `packages/java-server/src/main/java/com/savvagent/ottoflags/sdk/FlagClient.java`; test `.../FlagClientTest.java` | check + `HttpUrl` |
| Modify `packages/ios-sdk/Sources/OttoFlagsSDK/OttoFlagsClient.swift`; test `packages/ios-sdk/Tests/OttoFlagsSDKTests/OttoFlagsClientNetworkTests.swift` | `invalidFlagKey`, check, encoding (evaluate + getVariation) |
| Modify `packages/android-sdk/src/main/kotlin/com/savvagent/ottoflags/sdk/OttoFlagsClient.kt`; test `.../OttoFlagsClientNetworkTest.kt` | check + `HttpUrl` (evaluate + getVariation) |

## Task Order & Rationale

The tasks touch disjoint directories and run in parallel. Tasks 1 and 2 share the pnpm gate, so
they are one task.

---

### Task 1: TypeScript and Node SDKs

**Model:** sonnet

- [ ] Write failing jest tests in both packages, with `global.fetch` mocked. Construct each client
  with `enableRealtime: false, enableTelemetry: false`; otherwise the realtime and telemetry
  calls hit the fetch mock.
  - For each of `a/b`, `a?b`, `a#b`, `.`, `..`, `''`, `bad key` and a 129-character key:
    `evaluate` returns the configured default with reason `error`, `fetch` is not called, and
    `onError` is called once.
  - A valid key, `new-checkout.v2_b`, calls fetch with `<baseUrl>/api/flags/new-checkout.v2_b/evaluate`.
  - The key `...` is valid and fetches.
- [ ] Run `pnpm --filter @otto-flags/sdk test` and `pnpm --filter @otto-flags/node-server test`
  and see the new tests fail.
- [ ] Implement `isValidFlagKey` and the guard in each `client.ts`, and wrap the key in
  `encodeURIComponent`.
- [ ] Add the two patch changesets. Each is a `.changeset/*.md` file with frontmatter naming the
  package, and a one-line summary.
- [ ] Run the full pnpm gate and see it pass. Report the counts per package.

### Task 2: Python SDK

**Model:** sonnet

- [ ] Write failing tests in `tests/test_client.py` and `tests/test_async_client.py`, with respx
  routes asserting `called is False` or checking `respx` call counts. The cases are the same as
  Task 1, and each also asserts that `on_error` received an `InvalidFlagKeyError`. A valid key
  hits `/api/flags/new-checkout.v2_b/evaluate`.
- [ ] Run pytest and see the new tests fail.
- [ ] Implement:
  - `_flag_key.py` with `is_valid_flag_key(key: str) -> bool` and `describe_flag_key(key) -> str`
    (the bounded repr plus the length).
  - `InvalidFlagKeyError(OttoFlagsError)`, exported from `otto_flags`.
  - The guard at the top of both `evaluate` methods, and the `quote(flag_key, safe="")` URL.
- [ ] Run the full Python gate and see it pass (pytest count, mypy, ruff).

### Task 3: Go SDK

**Model:** sonnet

- [ ] Write a test in a new file `packages/go-server/flagkey_test.go`. The existing
  `client_test.go` uses config fields that no longer exist; do not copy its setup. Use the real
  `Config`: `APIKey` (must start with `sdk_`), `BaseURL`, `EnableRealtime: false`,
  `EnableTelemetry: false`, `Defaults`, `OnError`. Read `types.go` and `NewClient`.
  - Use an `httptest.Server` that counts requests.
  - The invalid keys (as in Task 1) give 0 requests, the default value, `Reason == "error"`, and
    `errors.Is(err, ErrInvalidFlagKey)`.
  - A valid key reaches path `/api/evaluate/new-checkout.v2_b`.
- [ ] Implement:
  - `var ErrInvalidFlagKey = errors.New(...)`.
  - The check, using a loop or a regexp; regexp `$` in Go matches only at end of text.
  - Name the URL variable something other than `url` for clarity.
  - The guard at the top of `Evaluate`: call `OnError`, then return `c.getDefaultResult(flagKey, err), err`.
  - `url.PathEscape(flagKey)` in the `Sprintf`.
- [ ] Gate not run. Self-check that it compiles by reading it: imports (`errors`, `net/url`) and
  no shadowing. Note in the report that the package's tests cannot compile today.

### Task 4: Java SDK

**Model:** sonnet

- [ ] Write a test in a new class `packages/java-server/src/test/java/com/savvagent/ottoflags/sdk/FlagKeyTest.java`
  with `MockWebServer` (JUnit 5). The existing `FlagClientTest` uses builder methods that no
  longer exist; do not copy its setup. Use the real `FlagClientConfig.builder(apiKey)` with
  `.baseUrl(...)`, `.enableRealtime(false)`, `.enableTelemetry(false)`, `.defaults(...)` and
  `.onError(...)`. Read `FlagClientConfig.java`.
  - The invalid keys give `getRequestCount() == 0`, the default value, and reason `"error"`.
  - A valid key's request path is `/api/evaluate/new-checkout.v2_b`.

- [ ] Implement a private static `isValidFlagKey`, and the guard at the top of `evaluate` that
  logs, calls `onError`, and returns `new FlagEvaluationResult(flagKey, default, "error", null)`.
  Build the URL as
  `HttpUrl.get(config.getBaseUrl()).newBuilder().addPathSegment("api").addPathSegment("evaluate").addPathSegment(flagKey).build()`.
- [ ] Gate not run. Self-check the imports (`okhttp3.HttpUrl`) and types.

### Task 5: iOS SDK

**Model:** sonnet

- [ ] Add an `internal init(config: OttoFlagsConfig, session: URLSession)`. The public
  `init(config:)` delegates to it with `URLSession(configuration: .default)`.
- [ ] Write tests in `OttoFlagsClientNetworkTests.swift`. Build the client with a session whose
  configuration has `protocolClasses = [MockURLProtocol.self]`, and with `enableWebSocket: false`.
  Record the URLs of requests through `MockURLProtocol.requestHandler`. Count only requests whose
  path ends in `/evaluate` or `/variation`, because polling may fetch other paths.
  - `evaluate` with each invalid key throws `OttoFlagsError.invalidFlagKey` with 0 requests.
  - `getVariation` with an invalid key returns `defaultValue` with 0 requests.
  - A valid key's request URL path is `/v1/flags/new-checkout.v2_b/evaluate`.
- [ ] Implement:
  - `case invalidFlagKey(String)`, where the associated message comes from
    `String(reflecting:)` of the 64-character prefix plus the length.
  - A private static `isValidFlagKey` that checks `utf8` bytes.
  - The guard at the top of `evaluate` and `getVariation`.
  - Encoding with the unreserved `CharacterSet`, in both URLs.
- [ ] Gate not run. Self-check by reading.

### Task 6: Android SDK

**Model:** sonnet

- [ ] Write tests in `OttoFlagsClientNetworkTest.kt`, or a new class beside it, with a client
  built with `enableLogging = false` so `android.util.Log` is never called. Keep the guard inside
  the existing `try`.
  - `evaluate` with each invalid key returns `Result.failure` and `mockWebServer.requestCount == 0`.
  - `getVariation` returns `defaultValue` with no request.
  - A valid key's `takeRequest().path` is `/v1/flags/new-checkout.v2_b/evaluate`.
- [ ] Implement a private `isValidFlagKey` and the guard. Build the URL with
  `config.apiUrl.toHttpUrl().newBuilder().addPathSegment("v1").addPathSegment("flags").addPathSegment(flagKey).addPathSegment("evaluate"|"variation").build()`,
  importing `okhttp3.HttpUrl.Companion.toHttpUrl`.
- [ ] No gate. Self-check by reading.

### Task 7 (orchestrator): commit, review, record

- [ ] Commit each task's files separately (`fix(sdk): …`, ending `Fixes #107`).
- [ ] Run the final full gates (pnpm, Python), then open the PR. The PR body lists the Go, Java
  and Swift gates as not run (no toolchain) and notes that Android has no gate.
