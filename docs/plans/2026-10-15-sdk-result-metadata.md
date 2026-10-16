# SDK result metadata Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** The Python, Node, Go and Java SDKs put the `version` and `scope` the server sends into
the evaluation result's metadata, on evaluated and cached results.

**Architecture:** Each SDK reads `version` (integer only) and `scope` (string only) leniently from
the evaluate response, adds them to its metadata type and its cache entry, and returns them on a
cache hit. Old metadata fields stay, documented as never sent by the server.

**Tech Stack:** Python (httpx, respx, pytest, mypy), TypeScript (jest), Go (net/http, httptest),
Java 11 (OkHttp, Gson, MockWebServer, JUnit 5).

**Ticket:** savvagent/otto-flags#124

**Spec:** `docs/specs/2026-10-15-sdk-result-metadata-design.md`

## Global Constraints

- Worktree: `/home/robhicks/dev/otto-flags/.claude/worktrees/fix-124-sdk-result-metadata`,
  branch `fix/124-sdk-result-metadata`. Worktree only; trunk changes only through the PR.
- Gates:
  - Python (worktree root; `/tmp` is a nearly full tmpfs): `mkdir -p .tmp && export
    TMPDIR=$PWD/.tmp && v=$PWD/.tmp/venv && python3 -m venv "$v" && "$v/bin/pip" install -q -e
    'packages/python-server[dev]' && cd packages/python-server && "$v/bin/pytest" -p
    no:cacheprovider && "$v/bin/mypy" --cache-dir=/dev/null src` (create the venv once, then rerun
    pytest and mypy directly).
  - pnpm (worktree root): `pnpm install --frozen-lockfile && pnpm lint && pnpm build && pnpm
    test`; quick loop `cd packages/node-server && npx jest --forceExit` (jest can hang on exit on
    existing open handles).
  - Go (`go vet`, `go test`), Java (`mvn -B verify`): no toolchain; do NOT install one; not run
    (Go also known red #17; test files that do not compile, #113). Write code you are confident compiles.
- Evidence: pass/fail/skip counts per suite; skipped tests are not a pass.
- Invariants: SDK REST contract unchanged; SDKs never throw into the host app on a flag lookup (a
  malformed metadata field never fails a lookup); public APIs change additively only; MIT
  packages.
- Commit format: `fix(sdk): <imperative, lower-case>`, body explains why, ends `Refs #124` (the
  last commit of the PR ends `Fixes #124`). No attribution of any kind.
- Out-of-band: none. Changeset only for `@otto-flags/node-server`.

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `packages/python-server/src/otto_flags/types.py` | `FlagMetadata`, `CacheEntry` gain `version`, `scope` |
| **Modify.** `packages/python-server/src/otto_flags/_response.py` | `response_metadata(data)` |
| **Modify.** `packages/python-server/src/otto_flags/cache.py` | `set(..., version=, scope=)` keyword-only |
| **Modify.** `packages/python-server/src/otto_flags/client.py`, `async_client.py` | build metadata on miss and hit |
| **Modify.** `packages/python-server/tests/test_client.py`, `test_async_client.py`, `README.md` | tests, docs |
| **Modify.** `packages/node-server/src/types.ts`, `cache.ts`, `client.ts`, `client.test.ts`, `README.md` | the same in Node |
| **Create.** `.changeset/node-server-result-metadata.md` | patch changeset |
| **Modify.** `packages/go-server/types.go`, `cache.go`, `client.go`, `README.md`; **Create.** `packages/go-server/metadata_test.go` | the same in Go |
| **Modify.** `packages/java-server/src/main/java/com/savvagent/ottoflags/sdk/FlagEvaluationResult.java`, `FlagClient.java`, `packages/java-server/README.md`; **Create.** `.../src/test/java/com/savvagent/ottoflags/sdk/MetadataTest.java` | the same in Java |

## Task Order & Rationale

One task per SDK; they touch disjoint packages. Python and Node have gates and set the pattern.

Test matrix (each SDK, against its mock server returning the server's exact shape:
`{"key":"checkout","enabled":true,"scope":"application","variation":null,"configuration":null,"timestamp":1700000000,"version":3,"context":{"environment":"production"}}`):

- **Evaluated:** metadata `version == 3`, `scope == "application"`; `flag_id`/`flagId`/`FlagID`,
  `description`, `variant` unset.
- **Cached:** the same context again → reason `cached`, same `version` and `scope`.
- **Malformed:** `"version": "3"`, `"scope": 7` → value `true`, reason `evaluated`, metadata
  `version` and `scope` unset, no `on_error`/`onError` call. Also `"version": true` and
  `"version": 1.5` unset (Python, Node; Go and Java where easy).

---

### Task 1: Python SDK

**Model:** sonnet

- [ ] **Step 1: failing tests.** In `tests/test_client.py` and `tests/test_async_client.py`, a
  new class (e.g. `TestResultMetadata`) with the matrix, using `mock_api`/respx and
  `server_body` from `conftest.py` (extend the body dict per test for the malformed cases).
- [ ] **Step 2: run the Python gate; see them fail.**
- [ ] **Step 3: implement.** `types.py`: `FlagMetadata` gains `version: int | None = None` and
  `scope: str | None = None` after the existing fields; docstrings: `flag_id`, `description`,
  `variant` are "not sent by the otto-flags server; always None from it, kept for
  compatibility"; `version` "the flag version that produced this result", `scope`
  `"application"` or `"enterprise"`. `CacheEntry` gains `version: int | None = None`, `scope:
  str | None = None`. `_response.py`: `def response_metadata(data: dict[str, Any]) -> tuple[int
  | None, str | None]` (version only if `isinstance(v, int) and not isinstance(v, bool)`; scope
  only if `isinstance(s, str)`). `cache.py` (both classes): `set` gains keyword-only `version:
  int | None = None, scope: str | None = None` stored on the entry. `client.py` and
  `async_client.py`: on a miss compute `version, scope = response_metadata(data)`, pass them to
  `cache.set(..., version=version, scope=scope, context_key=ctx_key)` and to `FlagMetadata`; on a
  hit return `metadata=FlagMetadata(flag_id=cached_entry.flag_id, version=cached_entry.version,
  scope=cached_entry.scope)`. README: document `metadata.version` and `metadata.scope` where the
  result shape is described.
- [ ] **Step 4: Python gate passes.**
- [ ] **Step 5: commit** `fix(sdk): report the server's version and scope in python results`.

### Task 2: Node server SDK

**Model:** sonnet

- [ ] **Step 1: failing tests** in `src/client.test.ts`, a new `describe('result metadata
  (#124)')`, with the client built `enableRealtime: false, enableTelemetry: false` and
  `global.fetch` mocked to return the matrix bodies.
- [ ] **Step 2: `cd packages/node-server && npx jest --forceExit`; see them fail.**
- [ ] **Step 3: implement.** `types.ts`: `FlagEvaluationResult.metadata` gains `version?:
  number` and `scope?: string` (JSDoc: the old three are never sent by the otto-flags server);
  `CacheEntry` gains `version?: number; scope?: string`. `cache.ts`: `set(key, value, flagId?,
  configuration?, variation?, contextKey = '', metadata?: { version?: number; scope?: string })`
  storing both. `client.ts`: response type gains `version?: unknown; scope?: unknown`; `const
  version = typeof data.version === 'number' && Number.isInteger(data.version) ? data.version :
  undefined; const scope = typeof data.scope === 'string' ? data.scope : undefined;`; pass them
  to `cache.set` and `metadata`; the cached path returns `metadata: { flagId:
  cachedEntry.flagId, version: cachedEntry.version, scope: cachedEntry.scope }`. README: the
  result shape. `.changeset/node-server-result-metadata.md`: `"@otto-flags/node-server": patch`
  — "Evaluation results carry the `version` and `scope` the server sends in `metadata`, on
  evaluated and cached results."
- [ ] **Step 4: pnpm gate passes.**
- [ ] **Step 5: commit** `fix(sdk): report the server's version and scope in node results`.

### Task 3: Go SDK

**Model:** sonnet

- [ ] **Step 1: tests (cannot run).** `metadata_test.go` in package `ottoflags`, using
  `newFlagKeyTestClient` (`flagkey_test.go`) and an `httptest.Server`; the server-shaped body
  as in `evaluate_contract_test.go`. Matrix: evaluated, cached, malformed (`"version":"3"`,
  `"scope":7`, and `"version":1.5`).
- [ ] **Step 2: implement.** `types.go`: `Metadata` gains `Version int64
  \`json:"version,omitempty"\`` (comment: the flag version that produced the result; 0 when the
  server sent none) and `Scope string \`json:"scope,omitempty"\``; comments on `FlagID`,
  `Description`, `Variant`: not sent by the otto-flags server. `cacheEntry` gains `Version
  int64`, `Scope string`. `cache.go`: `Set(key, ctxKey string, value bool, configuration
  map[string]interface{}, variation string, version int64, scope string)`. `client.go`: the
  response struct gains `Version json.RawMessage \`json:"version"\`` and `Scope json.RawMessage
  \`json:"scope"\``; after the `enabled` check, `var version int64; if len(response.Version) > 0
  { if json.Unmarshal(response.Version, &version) != nil { version = 0 } }` and the same for
  `scope` into a `string`; pass them to `Set` and `Metadata`; the cached path returns `Metadata:
  &Metadata{Version: cachedEntry.Version, Scope: cachedEntry.Scope}`. README: the result shape.
- [ ] **Step 3: self-check compile by reading** (every `Set` call site; imports).
- [ ] **Step 4: commit** `fix(sdk): report the server's version and scope in go results`.

### Task 4: Java SDK

**Model:** sonnet

- [ ] **Step 1: tests (cannot run).** `MetadataTest.java` modeled on `EvaluateContractTest.java`
  (MockWebServer, the same client construction). Matrix: evaluated, cached, malformed.
- [ ] **Step 2: implement.** `FlagEvaluationResult.Metadata` gains `private Long version;
  private String scope;` with `getVersion/setVersion`, `getScope/setScope`; Javadoc on the old
  getters: not sent by the otto-flags server. `FlagClient`: after the existing metadata reads,
  `JsonElement version = presentOrNull(data.get("version")); if (version != null &&
  version.isJsonPrimitive() && version.getAsJsonPrimitive().isNumber()) { BigDecimal d =
  version.getAsBigDecimal(); if (d.stripTrailingZeros().scale() <= 0) try {
  metadata.setVersion(d.longValueExact()); } catch (ArithmeticException ignored) {} }` (or an
  equivalent integral check), and `scope` only if `isJsonPrimitive() &&
  getAsJsonPrimitive().isString()`. `CacheEntry` gains `final Long version` and `final String scope` (not the mutable `Metadata`);
  `setCachedValue` takes them; the cached path returns a new `Metadata` with only those two set
  instead of `null`. Make sure
  the existing old-field reads cannot throw on a non-string value either (`getAsString` on an
  object throws): guard each with `isJsonPrimitive()`. README: the result shape.
- [ ] **Step 3: self-check compile by reading** (imports: `java.math.BigDecimal`).
- [ ] **Step 4: commit** `fix(sdk): report the server's version and scope in java results`.

### Task 5: Record as shipped

- [ ] Spec `**Status:** IMPLEMENTED (#<PR>)`, plan boxes ticked; commit `docs(sdk): record the
  result metadata as shipped`, body ending `Fixes #124`.
