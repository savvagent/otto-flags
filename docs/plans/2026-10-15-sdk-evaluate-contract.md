# SDK evaluate contract Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

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
- Commit format: `fix(sdk): <imperative, lower-case>`, body explains why and ends `Refs #112`
  (the last commit before the PR: `Fixes #112` goes in the PR body); no attribution of any kind.

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `packages/python-server/src/otto_flags/client.py`, `async_client.py` | read `enabled`, fail without it |
| **Modify.** `packages/python-server/tests/conftest.py`, `test_client.py`, `test_async_client.py` | server-shaped mocks, contract tests |
| **Modify.** `packages/node-server/src/client.ts`, `client.test.ts` | read `enabled`, fail without it; tests |
| **Create.** `.changeset/node-server-evaluate-enabled.md` | patch changeset |
| **Modify.** `packages/go-server/client.go`, `flagkey_test.go`; **Create.** `evaluate_contract_test.go` | path, `enabled`; tests |
| **Modify.** `packages/java-server/.../FlagClient.java`, `FlagKeyTest.java`; **Create.** `EvaluateContractTest.java` | path, `enabled`, nulls; tests |
| **Modify.** `packages/ios-sdk/Sources/OttoFlagsSDK/OttoFlagsClient.swift`, `Tests/.../FlagKeyValidationTests.swift`, `OttoFlagsClientNetworkTests.swift`; **Create.** `Tests/.../EvaluateContractTests.swift` | path, body, `getVariation`; tests |
| **Modify.** `packages/android-sdk/.../OttoFlagsClient.kt`, `OttoFlagsClientFlagKeyTest.kt`, `OttoFlagsClientNetworkTest.kt` | path, body, nulls, `getVariation`; tests |

## Task Order & Rationale

The gated SDKs first (Python, Node), so the contract tests that can run prove the shape before
the same shape is applied, unverified by a compiler, to Go, Java, Swift and Kotlin.

---

### Task 1: Python reads `enabled`

Model: sonnet

- [ ] Change `mock_flag_response` in `tests/conftest.py` to answer with the server's shape
  (`key`, `enabled`, `scope`, `variation`, `configuration`, `timestamp`, `version`, `context`);
  the key-path tests in `test_client.py` and the async tests answer with `enabled`.
- [ ] Add sync and async tests: `enabled: true` with no `value` → `value is True`, reason
  `evaluated`, path `/api/flags/{key}/evaluate`; a body `{"value": true}` (no `enabled`) → the
  default, reason `error`, `on_error` called once, and a second call makes a second request.
- [ ] Run the Python gate: the new tests fail.
- [ ] In `client.py` and `async_client.py`, read `data["enabled"]`; when `data` is not a dict or
  `enabled` is not a `bool`, raise `OttoFlagsError` (caught by the existing `except`).
- [ ] Run the Python gate: all pass; mypy clean.
- [ ] Commit `fix(sdk): read enabled from the python sdk evaluate response`.

### Task 2: Node reads `enabled`

Model: sonnet

- [ ] In `client.test.ts`, drop `value` from evaluate mocks; add tests: `{enabled: true}` → true
  and the fetch URL ends `/api/flags/test-flag/evaluate`; `{value: true}` → default, reason
  `error`, `onError` called, not cached (second call fetches again).
- [ ] Run the pnpm gate: the new tests fail.
- [ ] In `client.ts`, type the body with `enabled?: unknown`; throw when it is not a boolean.
- [ ] Run the pnpm gate: all pass.
- [ ] Add `.changeset/node-server-evaluate-enabled.md` (`"@otto-flags/node-server": patch`).
- [ ] Commit `fix(sdk): read enabled from the node sdk evaluate response`.

### Task 3: Go uses the documented path and `enabled`

Model: sonnet

- [ ] `flagkey_test.go`: the valid-key path pin becomes `/api/flags/<key>/evaluate`.
- [ ] Create `evaluate_contract_test.go`: an `httptest` server answering the server's shape
  (`"variation": null`, `"configuration": null`) → `Value` true, reason `evaluated`, path pinned;
  a body without `enabled` → default + non-nil error, reason `error`, `OnError` called, a second
  call makes a second request.
- [ ] `client.go`: endpoint `"%s/api/flags/%s/evaluate"`; response field `Enabled *bool
  \`json:"enabled"\``; nil → error result; cache and return `*response.Enabled`.
- [ ] Gate not run (no Go toolchain; #17, #113).
- [ ] Commit `fix(sdk): call the documented evaluate path from the go sdk`.

### Task 4: Java uses the documented path, `enabled`, and null fields

Model: sonnet

- [ ] `FlagKeyTest.java`: mocks answer `{"enabled":true}`; the path pin becomes
  `/api/flags/new-checkout.v2_b/evaluate`.
- [ ] Create `EvaluateContractTest.java` (MockWebServer, JUnit 5): server-shaped body with null
  `variation`/`configuration` → true, `evaluated`, null variation; no `enabled` → default,
  `error`, `onError` once, not cached; `"enabled":"true"` (string) → `error`.
- [ ] `FlagClient.java`: path segments `api`, `flags`, key, `evaluate`; `enabled` must be a
  boolean `JsonPrimitive`, else `IOException`; `variation`/`configuration` absent when
  `JsonNull`.
- [ ] Gate not run (no Maven; #113).
- [ ] Commit `fix(sdk): call the documented evaluate path from the java sdk`.

### Task 5: iOS uses the documented path and body; `getVariation` on `evaluate`

Model: sonnet

- [ ] `FlagKeyValidationTests.swift`: path pins become `/api/flags/new-checkout.v2_b/evaluate`;
  the variation test expects the evaluate path and the variation name from a body with
  `"variation": "on"`.
- [ ] Create `EvaluateContractTests.swift`: the request body (read from `httpBodyStream` when
  `httpBody` is nil) is `{"context": {"user_id", "environment", "attributes"}}`;
  `getVariation` returns the variation name, `defaultValue` for a null variation, and the
  enabled state for a `Bool` default.
- [ ] `OttoFlagsClientNetworkTests.swift`: path and mock bodies follow the server shape.
- [ ] `OttoFlagsClient.swift`: path `/api/flags/{key}/evaluate`; body from a new internal
  `evaluationBody(for:)`; `getVariation<T>` delegates to `evaluate`.
- [ ] Gate not run (no Swift toolchain; #119).
- [ ] Commit `fix(sdk): call the documented evaluate endpoint from the ios sdk`.

### Task 6: Android uses the documented path and body; nulls; `getVariation` on `evaluate`

Model: sonnet

- [ ] `OttoFlagsClientFlagKeyTest.kt`: `evaluateUrl(key).encodedPath` is
  `/api/flags/<key>/evaluate`.
- [ ] `OttoFlagsClientNetworkTest.kt`: path and bodies follow the server shape; the request body
  has `context.user_id` and `context.environment`; `getVariation` returns the variation name;
  the numeric case returns its default; a null `configuration`/`variation` is not an error.
- [ ] `OttoFlagsClient.kt`: `flagUrl(key, action)` → `evaluateUrl(key)`; body
  `{"context": {...}}`; `enabled` via `opt("enabled") as? Boolean`; `isNull` checks;
  `getVariation<T>` delegates to `evaluate`.
- [ ] Gate: none (no Gradle wrapper); recorded.
- [ ] Commit `fix(sdk): call the documented evaluate endpoint from the android sdk`.
