# SDKs send the flag key as one path segment

**Status:** IMPLEMENTED (#118)
**Issue:** #107

## 1. Brief

From #107:

> Several SDKs put the flag key into the evaluate URL by string interpolation, without
> percent-encoding it. The Rust SDK had the same bug, fixed in #95. A key containing `/`, `?`, `#`
> or `..` changes which endpoint on the flags host receives the request, and the SDK key goes with
> it. Keys normally come from host code, but some apps build them from input.
>
> **Where**
> - `packages/typescript/src/client.ts:390`: `` `${this.config.baseUrl}/api/flags/${flagKey}/evaluate` ``
>   (the browser SDKs built on it inherit this)
> - `packages/python-server/src/otto_flags/client.py:169` and `async_client.py:188`: f-string
> - `packages/go-server/client.go:110`: `fmt.Sprintf("%s/api/evaluate/%s", ...)`
> - `packages/java-server/src/main/java/com/savvagent/ottoflags/sdk/FlagClient.java:87`: string
>   concatenation
> - Also check the iOS and Android SDKs.
>
> `docs/SDK-DEVELOPER-GUIDE.md` (Flag Evaluation, from #95) now states the rule. The key is one
> percent-encoded path segment, and keys are 1-128 ASCII letters, digits, `-`, `_` and `.`. A key
> outside that set, or `.`/`..`, is a failed lookup that returns the default with no request.
>
> **Acceptance criteria**
> - In each SDK, a key containing `/`, `?`, `#`, or one of the dot segments `.`/`..`, sends no
>   request and returns the default.
> - Each SDK builds the URL with its platform's path-segment encoding (`encodeURIComponent`,
>   `urllib.parse.quote(..., safe="")`, `url.PathEscape`, …).
> - Each SDK has a test for this.
>
> **Out of scope**
> - The Rust SDK (#95) and the server (#105).

## 2. Premise corrections

- **The Node server SDK has the same bug, and the issue does not list it.**
  `packages/node-server/src/client.ts:138` interpolates the key the same way. It is fixed here too.
- **iOS and Android have a second interpolated URL each.** Besides evaluate, each has a
  `getVariation` call to `/v1/flags/{key}/variation`: `OttoFlagsClient.swift:251` and
  `OttoFlagsClient.kt:264`. Both are fixed here.
- **iOS and Android have no configured defaults.** `OttoFlagsConfig` has no `defaults` map, so
  these SDKs have no "default" to return from `evaluate`. Their own failed-lookup behaviour is the
  equivalent:
  - iOS `evaluate` throws an `OttoFlagsError`.
  - Android `evaluate` returns `Result.failure`.
  - Both `getVariation` methods return the caller's `defaultValue`.

  An invalid key follows the same path, with no request.
- **The browser framework SDKs do not build URLs.** `angular`, `astro`, `nextjs`, `react`,
  `remix`, `solid`, `svelte`, `sveltekit` and `vue` call `@otto-flags/sdk` (grep for
  `api/flags`). They are fixed through the `typescript` package.
- **Go, Java, iOS and Android call paths the server does not serve.** Go and Java use
  `/api/evaluate/{key}`; iOS and Android use `/v1/flags/{key}/…`. The Python, Node, Go and Java
  SDKs also read `value`, where the server sends `enabled`. Fixing the contract is filed as #112.
  This change keeps each SDK's existing path and changes only how the key goes into it, so the two
  fixes stay independently reviewable.

## 3. Assumptions

- **One key rule everywhere:** the key is valid when it has 1-128 characters, each an ASCII letter,
  digit, `-`, `_` or `.`, and is not `.` or `..`. This is the guide's rule, and the Rust SDK's
  `is_valid_flag_key`.
  - The server trims a key before validating it (`validate_key`,
    `crates/flags-core/src/flags.rs:151`). The SDKs do not trim: a key with whitespace is invalid,
    as in the Rust SDK.
  - Each SDK matches the whole string. Python uses `re.fullmatch`, not `$`, which also matches
    before a trailing newline. Go, Java and Kotlin also match the full string. Swift checks the
    UTF-8 bytes directly.
- **Validation runs first in `evaluate`,** before overrides, cache, context building or telemetry,
  as in the Rust SDK. A key no flag can have is a failed lookup whatever else is configured, and
  failing early keeps such keys out of the telemetry these SDKs send to the server.
- **The host is told through the SDK's existing error channel:**
  - TypeScript and Node call `config.onError(Error)`.
  - Python calls `config.on_error(InvalidFlagKeyError)` when one is set.
  - Go calls `config.OnError` and returns the default result with a non-nil error. Every other Go
    failure returns default + error, and `IsEnabled` drops the error.
  - Java logs at error level and calls `onError`, then returns the default result with reason
    `"error"`, like its catch path.
  - iOS throws a new `OttoFlagsError.invalidFlagKey` case.
  - Android returns `Result.failure(OttoFlagsException(...))` and calls `logError`.

  The message names the key as an escaped prefix of at most 64 characters plus its length, as the
  Rust SDK logs it, because hosts may build keys from input. The escapers are:

  | SDK | Escaper |
  |---|---|
  | TypeScript, Node | `JSON.stringify` |
  | Python | `repr` |
  | Go | `%q` |
  | Java | gson `toJson` |
  | Kotlin | a small hand-written escaper: printable ASCII other than `"` and `\` kept, everything else `\uXXXX`, in quotes |
  | Swift | `String(reflecting:)` |

  Kotlin does not use `org.json`'s `JSONObject.quote`, because `org.json` is a stub in Android
  local unit tests.
- **Reason `"error"`** in the returned result where the SDK has a reason field (TS, Node, Python,
  Go, Java), as for every other failed lookup.
- **Encoding is applied even though a valid key needs none.** Every character the rule allows is
  RFC 3986 unreserved, so encoding a valid key does not change it. The encoder is defense in
  depth, and it is what the issue asks for. The encoders:
  - TS and Node: `encodeURIComponent`
  - Python: `urllib.parse.quote(key, safe="")`
  - Go: `url.PathEscape`
  - Swift: `addingPercentEncoding` with the RFC 3986 unreserved set
  - Java and Kotlin: OkHttp's `HttpUrl.Builder.addPathSegment`, which encodes one segment
- **Java and Android build the whole URL with `HttpUrl`.** They start from the base URL and
  `addPathSegment` each part, which keeps a path prefix in the base URL. An unparsable base URL
  goes down the SDK's existing failure path (the catch block or `Result.failure`). The other SDKs
  keep their string prefix (`<base>/api/flags/`) and encode only the key segment. Changing how
  they treat a trailing slash on the base URL is out of scope.
- **Changesets:** `@otto-flags/sdk` and `@otto-flags/node-server` each get a patch changeset. The
  framework packages depend on `@otto-flags/sdk` through `workspace:*`, and the changeset config's
  `updateInternalDependencies: patch` covers them. No version-file edits.
- **Gates not run** (no toolchain on the host, per CLAUDE.md): Go (`go vet`, `go test`; also
  known red, #17), Java (`mvn -B verify`) and Swift (`swift test`). Android has no gate. Each
  change gets a careful language review instead, and the PR body records the gap.

## 4. Goal & success criteria

No SDK in `packages/` other than Rust (already fixed) can send a request for a flag key outside
the key rule, and each builds the key segment with its platform's encoder.

- In TS, Node, Python (sync and async), Go, Java, iOS and Android, a test evaluates keys
  containing `/`, `?`, `#`, and the keys `.` and `..`. It asserts that no request was made and
  that the SDK's failed-lookup result came back.
- The TS, Node, Python, Go and Java tests also check that a valid key (e.g. `new-checkout.v2_b`)
  still reaches the expected path. The Swift and Android tests do the same where the existing test
  harness records the request URL.
- The pnpm gate and the Python gate pass with the new tests counted.

## 5. Scope

**In:**
- the flag-key check and segment encoding in
  - `packages/typescript/src/client.ts`
  - `packages/node-server/src/client.ts`
  - `packages/python-server/src/otto_flags/{client,async_client}.py`, with one shared helper
  - `packages/go-server/client.go`
  - `packages/java-server/src/main/java/com/savvagent/ottoflags/sdk/FlagClient.java`
  - `packages/ios-sdk/Sources/OttoFlagsSDK/OttoFlagsClient.swift`
  - `packages/android-sdk/src/main/kotlin/com/savvagent/ottoflags/sdk/OttoFlagsClient.kt`
- tests beside each SDK's existing tests
- two changesets

**Out:**
- `packages/rust-server` (done in #95)
- the server (#105)
- fixing the endpoint paths and `value`/`enabled` (#112)
- base-URL trailing-slash handling in the string-prefix SDKs
- examples
- version bumps and publishing (Rob)

## 6. Architecture

Each SDK gets a private `isValidFlagKey` (or the local idiom for the name) next to its client, and
a guard at the top of `evaluate` (and of iOS and Android `getVariation`). The URL expression
changes to encode the key segment. Public API changes:

- iOS gains `OttoFlagsError.invalidFlagKey`. This is additive, but a host that switches over the
  enum exhaustively must add the case; the enum is not `@frozen` in a library-evolution build.
  Recorded as a risk.
- Python gains `InvalidFlagKeyError` (a subclass of the SDK's base exception in
  `otto_flags/exceptions.py`), exported from `otto_flags`. Additive.
- Go gains an exported `ErrInvalidFlagKey` sentinel, wrapped with the key detail. Additive.
- Nothing else is public.

Python puts the helper in `otto_flags/_flag_key.py`, shared by both clients.

The iOS client gains an `internal init(config:session:)`, and the public `init(config:)` calls it
with the default session. This lets the tests, through `@testable import`, inject a `URLSession`
configured with `MockURLProtocol`. Today the client hard-codes its session, and the network tests
are commented out because nothing can be injected. This is not public API.

## 7. Invariants impact

- **SDKs never throw into the host app on a flag lookup:** holds.
  - TS, Node, Python, Go and Java return the default and do not raise.
  - iOS `evaluate` and Android `evaluate` already report failure through `throws` and `Result`.
    Their invalid-key path uses the same channel, so a host sees no new kind of error behaviour.
- **SDK REST contract stable / additive:** holds. Valid keys produce byte-identical URLs. Invalid
  keys produce no request.
- **Client keys are public:** the fix stops a crafted key from steering the `sdk_` key to another
  endpoint on the flags host.
- **Licensing:** all changes are under `packages/` (MIT), and no server code is copied in. The key
  rule is restated from the guide, not copied from server code.

## 8. Migration

None.

## 9. Out-of-band

- Changesets: added in this PR (agent).
- Publishing: none. Not automated, and no `@otto-flags/*` package is on npm (Rob's call).
- No secrets, config keys or scopes.

## 10. Error handling & edge cases

- **Non-string keys** (JS callers without types): `typeof key !== 'string'` is invalid.
- **Length** counts characters. Every valid character is ASCII, so this is also the byte count;
  for invalid keys only the 64-character prefix in the message depends on it.
- **The empty key** is invalid.
- **`...`** is valid by the rule: it is not a dot segment, and the server accepts it.
- **Python `on_error`:** when it is unset, the SDK only returns the default. That matches the
  existing failure path, which calls `on_error` only if it is set.
- **Telemetry:** no evaluation event is recorded for an invalid key. The early return precedes it.

## 11. Testing approach

Per SDK, failing test first.

| SDK | Test file | Gate |
|---|---|---|
| TypeScript | `packages/typescript/tests/client.test.ts`, mocking `global.fetch` | pnpm gate |
| Node | `packages/node-server/src/client.test.ts`, existing fetch mock | pnpm gate |
| Python | `packages/python-server/tests/test_client.py` and `test_async_client.py` with `respx`/`pytest-httpx` (whichever the file uses) | Python gate |
| Go | new `packages/go-server/flagkey_test.go` with an `httptest.Server` that counts requests | not run (no toolchain; known red #17) |
| Java | new `FlagKeyTest.java` with `MockWebServer` (`getRequestCount`, `takeRequest().getPath()`) | not run (no toolchain) |
| iOS | new `FlagKeyValidationTests.swift` with the existing `MockURLProtocol`, through the internal session initializer | not run (no toolchain) |
| Android | new `OttoFlagsClientFlagKeyTest.kt`: `MockWebServer` request counts for invalid keys; the valid-key path through the internal `flagUrl` (a network round trip would reach `org.json`, a stub in local unit tests) | no gate |

## 12. Risks & open questions

- Go, Java, Swift and Kotlin changes are verified only by review. Compile errors there would go
  unnoticed until someone runs those toolchains.
- Adding a case to the iOS `OttoFlagsError` enum can break a host's exhaustive `switch`. The
  alternative was to reuse `invalidURL`, but that would hide the cause. Accepted, because the SDK
  is unpublished.
- The existing Go `client_test.go` and Java `FlagClientTest.java` do not compile against the
  current SDK API. They use config fields that no longer exist; known-red #17 covers only Go's
  missing `go.sum`. The new tests use the real APIs in new files, but Go compiles every
  `_test.go` in a package together and Maven fails the whole module on a test-compile error, so
  neither suite can run until that is fixed (#113). The Android network tests may
  already be red on a plain JVM, because they use `org.json` and `android.util.Log` without
  Robolectric. The new Android test turns logging off, and the guard avoids `org.json`.
- Until #112, Go, Java, iOS and Android still call endpoints the server does not serve, so for
  valid keys this fix changes nothing observable there.
