# Non-Rust server and mobile SDKs follow the evaluate contract

**Status:** DRAFT
**Issue:** #112

## 1. Brief

From #112:

> The SDK developer guide (`docs/SDK-DEVELOPER-GUIDE.md`, Flag Evaluation) defines
> `POST /api/flags/{key}/evaluate`, with the result in `enabled`. Several non-Rust SDKs do not
> follow it. These SDKs cannot evaluate a flag against the server; every lookup ends in the
> default:
>
> - **Wrong path**
>   - `packages/go-server/client.go`: `/api/evaluate/{key}`
>   - `packages/java-server/.../FlagClient.java`: `/api/evaluate/{key}`
>   - `packages/ios-sdk/.../OttoFlagsClient.swift`: `/v1/flags/{key}/evaluate` and `/v1/flags/{key}/variation`
>   - `packages/android-sdk/.../OttoFlagsClient.kt`: `/v1/flags/{key}/evaluate` and `/v1/flags/{key}/variation`
> - **Reads `value` instead of `enabled`**
>   - `packages/python-server` (`client.py`, `async_client.py`)
>   - `packages/node-server/src/client.ts`
>   - `packages/go-server/client.go`
>   - `packages/java-server/.../FlagClient.java`
>
> **Acceptance criteria**
> - Each listed SDK calls `POST /api/flags/{key}/evaluate` and reads `enabled`, as the guide
>   documents.
> - Each has a test that pins the path and the response field.
> - Any variation call either uses an endpoint the server serves or is removed and its removal
>   recorded.
>
> **Out of scope**
> - The Rust SDK (#92), and the TypeScript browser SDK, which already follows the contract.

## 2. Premise corrections

- **The server does serve `/api/evaluate/{key}`.** `crates/flags-api/src/lib.rs` routes it to the
  same handler as `/api/flags/{key}/evaluate`, as an undocumented alias. Go and Java therefore
  reach the server today; they fail only because they read `value`. They still move to the
  documented path (the AC), so they do not depend on an alias the guide does not list.
- **iOS and Android already read `enabled`,** but send the context unwrapped (the body is
  `{"userId": …, "attributes": …}`), where the server reads `{"context": {"user_id": …}}`
  (`EvaluateBody` and `flags_core::eval::Context`). The server ignores the unknown top-level
  fields and evaluates with an empty context: no user, so rollouts and targeting rules never match.
  Moving them to the documented path without the documented body would still leave them broken,
  so the body is fixed here too.
- **Java and Android break on the server's `null` fields.** The server always sends `variation`
  and `configuration`, as `null` when absent. Java calls `getAsString()` on a `JsonNull` (throws),
  Android calls `getJSONObject` on `JSONObject.NULL` (throws) and `getString` (returns the string
  `"null"`). Every flag without a variation or configuration would fail. These reads are fixed
  with the field.
- **The existing tests mock the wrong body.** Python's `mock_flag_response`, Node's
  `client.test.ts` and the Java and Go key tests answer with `{"value": …}` (Node sends both), so
  the bug never showed. The mocks move to the server's real response shape.

## 3. Assumptions

- **A 200 whose body has no boolean `enabled` is a failed lookup,** as in the Rust SDK (#109):
  the SDK's failure path (the default with reason `"error"`, or iOS's throw and Android's
  `Result.failure`), the host's error callback where one exists, and nothing cached. Today Python,
  Node and Java substitute the configured default and cache it as if it were real, and Go reads a
  missing field as `false` and caches it. A string `"true"` or a number is not a boolean.
- **The request body follows the guide** in every SDK touched: `{"context": {…}}` with snake_case
  context fields. Python, Node, Go and Java already do this. iOS and Android build it from
  `UserContext` as `{"context": {"user_id", "environment", "attributes"}}`, with `environment` from
  the client config (both configs have one, defaulting to `production`; neither sends it today).
  `UserContext.toJSON()`/`toJson()` stays as it is: the `track` event payload uses it.
- **Mobile `getVariation<T>` stays, rebuilt on `evaluate`.** The server has no variation endpoint,
  and the variation name is part of the evaluate response. Removing the method would break a
  public API for no gain, so it now delegates to `evaluate` (one request, the same cache) and
  returns:
  - when `defaultValue` is a `String`: the variation name, or `defaultValue` when the flag has none;
  - when `defaultValue` is a `Bool`/`Boolean`: whether the flag is enabled;
  - otherwise, and on any failure: `defaultValue`.

  This is the same name the existing `getVariationDetails` returns, minus its `"control"`
  fallback. The doc comment says so. The old numeric case (`getVariation("max-items", ctx, 10)`)
  has no server equivalent and returns the default; its Android test changes accordingly.
- **Metadata stays as is.** Python, Node, Go and Java fill `flagId`/`description`/`variant` from
  fields the server never sends, so they are always empty. Filling them from `version` and `scope`
  (as Rust #109 did) is a separate API change, filed as a follow-up.
- **Other iOS and Android endpoints are out of scope.** Both also call `/v1/events`, `/v1/stream`
  (WebSocket) and `/v1/flags`, which the server does not serve either (it has
  `/api/telemetry/*`, SSE `/api/flags/stream` and `/api/sdk/flags`). The issue names only evaluate
  and variation; the rest is filed as a follow-up.
- **Changesets:** `@otto-flags/node-server` gets a patch changeset. Python, Go, Java, iOS and
  Android are not pnpm packages and are unpublished; no version-file edits.
- **Gates not run** (no toolchain on the host, per CLAUDE.md): Go (`go vet`, `go test`; known red
  #17, and the package's tests do not compile, #113), Java (`mvn -B verify`; test-compile fails,
  #113), Swift (`swift test`; the package does not build, #119). Android has no gate. These get a
  careful language review instead, and the PR body records the gap.

## 4. Goal & success criteria

Every non-Rust server and mobile SDK evaluates a flag the way the guide documents, so a flag that
is on reads as on.

- Python (sync and async), Node, Go, Java, iOS and Android post to `/api/flags/{key}/evaluate`.
- Each reads `enabled`; a body without a boolean `enabled` is a failed lookup and is not cached.
- Java and Android accept `null` `variation` and `configuration`.
- iOS and Android send `{"context": {"user_id", "environment", "attributes"}}`.
- Each SDK has a test against a mock server that answers with the server's response shape and
  pins the path and the field; the Python and Node tests fail before the change and pass after.

## 5. Scope

**In:**
- `packages/python-server/src/otto_flags/{client,async_client}.py` and their tests
  (`tests/conftest.py`, `tests/test_client.py`, `tests/test_async_client.py`)
- `packages/node-server/src/client.ts` and `client.test.ts`
- `packages/go-server/client.go` and `flagkey_test.go` (path pin) plus a new
  `evaluate_contract_test.go`
- `packages/java-server/.../FlagClient.java`, `FlagKeyTest.java` (path pin) plus a new
  `EvaluateContractTest.java`
- `packages/ios-sdk/Sources/OttoFlagsSDK/OttoFlagsClient.swift`, `FlagKeyValidationTests.swift`,
  `OttoFlagsClientNetworkTests.swift`
- `packages/android-sdk/.../OttoFlagsClient.kt`, `OttoFlagsClientFlagKeyTest.kt`,
  `OttoFlagsClientNetworkTest.kt`
- one changeset

**Out:** the server (it is the ground truth and is not changed), Rust and TypeScript SDKs,
metadata fields, other mobile endpoints, iOS/Android "never throw" behaviour (their `evaluate`
already reports failure by `throws`/`Result`), the compile failures in #113 and #119, examples,
version bumps and publishing.

## 6. Architecture

Per SDK, in the evaluate request/response code only:

| SDK | Path | Field | Other |
|---|---|---|---|
| Python sync + async | unchanged (already documented path) | `data["enabled"]`, must be `bool`, else raise `OttoFlagsError` into the existing `except` (default, `on_error`, not cached) | — |
| Node | unchanged | `typeof data.enabled === 'boolean'`, else throw into the existing `catch` | response type gains `enabled`, drops `value` |
| Go | `/api/flags/{key}/evaluate` | `Enabled *bool \`json:"enabled"\``; nil is an error returned with the default result | — |
| Java | `HttpUrl` segments `api`, `flags`, key, `evaluate` | `enabled` must be a JSON boolean primitive, else `IOException` into the existing catch | `variation`/`configuration` read as absent when `JsonNull` |
| iOS | `/api/flags/{key}/evaluate` | already `enabled as? Bool` | body `{"context": …}`; `getVariation<T>` delegates to `evaluate` |
| Android | `flagUrl` builds `api/flags/{key}/evaluate` | `opt("enabled") as? Boolean`, else failure | `isNull` checks for `variation`/`configuration`; body `{"context": …}`; `getVariation<T>` delegates to `evaluate` |

Public API: no signature changes. Behaviour changes, all toward the contract: Go/Java/iOS/Android
call a different path; mobile sends a different body; mobile `getVariation<T>` returns the
variation name/enabled state instead of a `value` field no server sends; a response without
boolean `enabled` is now a failure in Python, Node, Go and Java. Android's `flagUrl(flagKey,
action)` is `internal` and becomes `evaluateUrl(flagKey)`.

## 7. Invariants impact

- **SDK REST contract stable / additive:** the SDKs move onto the documented contract; the server
  is unchanged.
- **SDKs never throw into the host app on a flag lookup:** Python, Node, Go and Java still return
  the default (Go with an error value, as for every other failure). iOS and Android keep their
  existing failure channels (`throws`, `Result.failure`); `getVariation<T>` never throws.
- **Client keys are public:** no change to what is sent with the key, beyond the context fields
  the guide documents.
- **Licensing:** changes are under `packages/` (MIT); no server code copied.

## 8. Migration

None.

## 9. Out-of-band

- Changeset: added in this PR (agent).
- Publishing: none (not automated, nothing on npm; Rob's call).
- No secrets, config keys or scopes. SDK-only: the post-merge deploy redeploys unchanged server
  code, verified for health only.

## 10. Error handling & edge cases

- Body not JSON, JSON not an object, `enabled` missing, `null`, a string or a number: failed
  lookup, not cached.
- `variation: null`, `configuration: null`: absent, not an error.
- Cached hits are unchanged: only real `enabled` values are cached.
- Go: `json.Unmarshal` of `"enabled": null` into `*bool` leaves it nil, so it is a failure too.

## 11. Testing approach

- **Python:** `mock_flag_response` answers with the server's shape (`key`, `enabled`, `scope`,
  `variation`, `configuration`, `timestamp`, `version`, `context`); the key-path tests answer with
  `enabled`. New sync and async tests: `enabled: true` → `value is True`, reason `evaluated`; a
  body with only `value: true` and no `enabled` → the default with reason `error`, `on_error`
  called, and a second call makes a second request (not cached). Gate: the python-server gate.
- **Node:** mocks drop `value`; new tests for `enabled` without `value`, a body with only `value`
  (failure, not cached), and the request URL. Gate: the pnpm gate.
- **Go, Java:** the key tests' path pin moves to `/api/flags/{key}/evaluate`; a new test file per
  SDK answers with the server's shape and asserts the path, `enabled` read, null variation, and
  missing `enabled` → default + error, not cached. Not run (no toolchain; #113).
- **iOS, Android:** existing path pins move; new assertions for the request body
  (`context.user_id`, `context.environment`) and `getVariation` returning the variation name.
  Not run (no toolchain / no gate; #119, #113).

## 12. Risks & open questions

- Go, Java, Swift and Kotlin changes are unverified by a compiler here. Mitigation: language
  review by the matching reviewer; changes kept small and in the style of the surrounding code.
- Mobile `getVariation<T>`'s new meaning is a behaviour change for any caller relying on a `value`
  field; no server ever sent one, so no caller can have depended on a working result.
