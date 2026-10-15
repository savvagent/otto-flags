# SDK evaluate contract Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [x]`) syntax for tracking.

**Goal:** Python, Node, Go, Java, iOS and Android SDKs evaluate flags through
`POST /api/flags/{key}/evaluate` and read `enabled`, as `docs/SDK-DEVELOPER-GUIDE.md` documents.

**Architecture:** Each SDK's evaluate request/response code changes in place: the path, the
`enabled` read (a body without a boolean `enabled` is a failed lookup, not cached), null-safe
`variation`/`configuration` reads (Java, Android), the `{"context": …}` request body (iOS,
Android), and mobile `getVariation<T>` rebuilt on `evaluate`.

**Tech Stack:** Python (httpx, respx, pytest), TypeScript (jest), Go (net/http, httptest), Java
(OkHttp, Gson, MockWebServer, JUnit 5), Swift (URLSession, XCTest), Kotlin (OkHttp, org.json,
MockWebServer, JUnit 4).

**Ticket:** savvagent/otto-flags#112

**Spec:** `docs/specs/2026-10-15-sdk-evaluate-contract-design.md`

## Global Constraints

- Gates:
  - Python: `v=$(mktemp -d)/venv && python3 -m venv "$v" && "$v/bin/pip" install -q -e 'packages/python-server[dev]' && cd packages/python-server && "$v/bin/pytest" -p no:cacheprovider && "$v/bin/mypy" --cache-dir=/dev/null src`
    (TMPDIR inside the worktree: `/tmp` is a nearly full tmpfs)
  - Node: `pnpm install --frozen-lockfile && pnpm lint && pnpm build && pnpm test`
  - Go (`go vet`, `go test`), Java (`mvn -B verify`), Swift (`swift test`): no toolchain on the
    host; not run, recorded in the PR body (Go known red #17; Go/Java tests do not compile #113;
    iOS does not build #119). Android has no gate.
- Evidence: report pass counts per test binary/suite; skipped tests are not a pass.
- Invariants: the SDK REST contract (the guide) is the target; SDKs never throw into the host app
  on a flag lookup (Python/Node/Go/Java return the default; iOS/Android keep `throws`/`Result`);
  `packages/` is MIT, no server code copied.
- Worktree only (`.claude/worktrees/fix-112-sdk-evaluate-contract`); trunk changes only through
  the PR.
- Commit format: `fix(sdk): <imperative, lower-case>`, body explains why and ends `Fixes #112`;
  no attribution of any kind.
- Python gate env: `export TMPDIR=$PWD/.tmp` (created in the worktree, never committed) before
  `mktemp -d`.

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `packages/python-server/src/otto_flags/client.py`, `async_client.py` | read `enabled`, fail without it |
| **Create.** `packages/python-server/src/otto_flags/_response.py` | the one `enabled` reader both clients use |
| **Modify.** `packages/ios-sdk/README.md`, `packages/android-sdk/README.md` | `getVariation` examples match the new rules |
| **Modify.** `packages/python-server/tests/conftest.py`, `test_client.py`, `test_async_client.py` | server-shaped mocks, contract tests |
| **Modify.** `packages/node-server/src/client.ts`, `client.test.ts`, `dynamic-config.test.ts`, `config-overrides.test.ts` | read `enabled`, fail without it; tests and mocks |
| **Create.** `.changeset/node-server-evaluate-enabled.md` | patch changeset |
| **Modify.** `packages/go-server/client.go`, `flagkey_test.go`, `dynamic_config_test.go`, `override_test.go`; **Create.** `evaluate_contract_test.go` | path, `enabled`; tests and mocks |
| **Modify.** `packages/java-server/.../FlagClient.java`, `FlagKeyTest.java`; **Create.** `EvaluateContractTest.java` | path, `enabled`, nulls; tests |
| **Modify.** `packages/ios-sdk/Sources/OttoFlagsSDK/OttoFlagsClient.swift`, `Tests/.../FlagKeyValidationTests.swift`, `OttoFlagsClientNetworkTests.swift`; **Create.** `Tests/.../EvaluateContractTests.swift` | path, body, `getVariation`; tests |
| **Modify.** `packages/android-sdk/.../OttoFlagsClient.kt`, `OttoFlagsClientFlagKeyTest.kt`, `OttoFlagsClientNetworkTest.kt` | path, body, nulls, `getVariation`; tests |

## Task Order & Rationale

The gated SDKs first (Python, Node), so the contract tests that can run prove the shape before
the same shape is applied, unverified by a compiler, to Go, Java, Swift and Kotlin.

---

### Task 1: Python reads `enabled`

Model: sonnet

- [x] Change `mock_flag_response` in `tests/conftest.py` to answer with the server's shape
  (`key`, `enabled`, `scope`, `variation`, `configuration`, `timestamp`, `version`, `context`);
  the key-path tests in `test_client.py` and the async tests answer with `enabled`.
- [x] Add sync and async tests: `enabled: true` with no `value` → `value is True`, reason
  `evaluated`, path `/api/flags/{key}/evaluate`; a body `{"value": true}` (no `enabled`) → the
  default, reason `error`, `on_error` called once, and a second call makes a second request.
- [x] Run the Python gate: the new tests fail.
- [x] In `client.py` and `async_client.py`, read `data["enabled"]`; when `data` is not a dict or
  `enabled` is not a `bool`, raise `OttoFlagsError` (caught by the existing `except`).
- [x] Run the Python gate: all pass; mypy clean.
- [x] Commit `fix(sdk): read enabled from the python sdk evaluate response`.

### Task 2: Node reads `enabled`

Model: sonnet

- [x] In `client.test.ts`, `dynamic-config.test.ts` and `config-overrides.test.ts`, evaluate
  mocks answer `enabled` instead of `value`. In `client.test.ts` add tests: `{enabled: true}` → true
  and the fetch URL ends `/api/flags/test-flag/evaluate`; `{value: true}` → default, reason
  `error`, `onError` called, not cached (second call fetches again).
- [x] Run the pnpm gate: the new tests fail.
- [x] In `client.ts`, type the body with `enabled?: unknown`; throw when it is not a boolean.
- [x] Run the pnpm gate: all pass.
- [x] Add `.changeset/node-server-evaluate-enabled.md` (`"@otto-flags/node-server": patch`).
- [x] Commit `fix(sdk): read enabled from the node sdk evaluate response`.

### Task 3: Go uses the documented path and `enabled`

Model: sonnet

- [x] `flagkey_test.go`: the valid-key path pin becomes `/api/flags/<key>/evaluate`.
- [x] `dynamic_config_test.go`, `override_test.go`: evaluate mocks answer `enabled` instead of
  `value` (they do not compile until #113; this keeps them from failing on the contract once
  they do). `client_test.go` is left to #113: its whole API surface is stale.
- [x] Create `evaluate_contract_test.go`: an `httptest` server answering the server's shape
  (`"variation": null`, `"configuration": null`) → `Value` true, reason `evaluated`, path pinned;
  a body without `enabled` → default + non-nil error, reason `error`, `OnError` called, a second
  call makes a second request.
- [x] `client.go`: endpoint `"%s/api/flags/%s/evaluate"`; response field `Enabled *bool
  \`json:"enabled"\``; nil → error result; cache and return `*response.Enabled`.
- [x] Gate not run (no Go toolchain; #17, #113).
- [x] Commit `fix(sdk): call the documented evaluate path from the go sdk`.

### Task 4: Java uses the documented path, `enabled`, and null fields

Model: sonnet

- [x] `FlagClientTest.java` is left to #113 (it does not compile; its whole API surface is
  stale).
- [x] `FlagKeyTest.java`: mocks answer `{"enabled":true}`; the path pin becomes
  `/api/flags/new-checkout.v2_b/evaluate`.
- [x] Create `EvaluateContractTest.java` (MockWebServer, JUnit 5): server-shaped body with null
  `variation`/`configuration` → true, `evaluated`, null variation; no `enabled` → default,
  `error`, `onError` once, not cached; `"enabled":"true"` (string) → `error`.
- [x] `FlagClient.java`: path segments `api`, `flags`, key, `evaluate`; `enabled` must be a
  boolean `JsonPrimitive`, else `IOException`; `variation`/`configuration` absent when
  `JsonNull`.
- [x] Gate not run (no Maven; #113).
- [x] Commit `fix(sdk): call the documented evaluate path from the java sdk`.

### Task 5: iOS uses the documented path and body; `getVariation` on `evaluate`

Model: sonnet

- [x] `FlagKeyValidationTests.swift`: path pins become `/api/flags/new-checkout.v2_b/evaluate`;
  the variation test expects the evaluate path and the variation name from a body with
  `"variation": "on"`.
- [x] Create `EvaluateContractTests.swift`: the request body (read from `httpBodyStream` when
  `httpBody` is nil) is `{"context": {"user_id", "environment", "attributes"}}`;
  `getVariation` returns the variation name, `defaultValue` for a null variation, and the
  enabled state for a `Bool` default.
- [x] `OttoFlagsClientNetworkTests.swift`: path and mock bodies follow the server shape.
- [x] `OttoFlagsClient.swift`: path `/api/flags/{key}/evaluate`; body from a new internal
  `evaluationBody(for:)`; `getVariation<T>` delegates to `evaluate`; its doc comment states the semantics (variation
  name for a `String` default, enabled state for a `Bool` default, else the default; cache and
  overrides apply). Update the stale `/v1/flags/a` comment in `FlagKeyValidationTests.swift`.
- [x] Gate not run (no Swift toolchain; #119).
- [x] Commit `fix(sdk): call the documented evaluate endpoint from the ios sdk`.

### Task 6: Android uses the documented path and body; nulls; `getVariation` on `evaluate`

Model: sonnet

- [x] `OttoFlagsClientFlagKeyTest.kt`: `evaluateUrl(key).encodedPath` is
  `/api/flags/<key>/evaluate`.
- [x] `OttoFlagsClientNetworkTest.kt`: path and bodies follow the server shape; the request body
  has `context.user_id` and `context.environment`; `getVariation` returns the variation name;
  the numeric case returns its default; a null `configuration`/`variation` is not an error.
- [x] `OttoFlagsClient.kt`: `flagUrl(key, action)` → `evaluateUrl(key)`; body
  `{"context": {...}}`; `enabled` via `opt("enabled") as? Boolean`; `isNull` checks;
  `getVariation<T>` delegates to `evaluate`, with the same doc comment as iOS.
- [x] Gate: none (no Gradle wrapper); recorded. The network tests reach `org.json`, a stub on a
  plain JVM (#113), so only the URL-builder test is meaningful without Robolectric; recorded in
  the PR body.
- [x] Commit `fix(sdk): call the documented evaluate endpoint from the android sdk`.

### Task 7: Follow-ups and PR body

Model: haiku

- [x] File two issues (type `bug`, no attribution): non-Rust SDKs fill metadata from fields the
  server never sends; iOS and Android call `/v1/events`, `/v1/stream` and `/v1/flags`, which the
  server does not serve. (Filed as #124 and #125; review added #127, context-blind caching.)
- [x] The PR body names both, and records the gates not run (Go, Java, Swift) and Android's lack
  of a gate.
