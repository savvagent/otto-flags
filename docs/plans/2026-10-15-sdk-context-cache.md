# SDK per-context cache Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** Every non-Rust SDK (Python, Node, TypeScript browser, Go, Java, iOS, Android) caches an
evaluation by flag key and the context it sent, so one context's result is never served to another.

**Architecture:** Each SDK's cache becomes `flag key -> (context key -> entry)`, where the context
key is the canonical (sorted-keys) JSON of the context object put in the request body, computed
before the cache lookup. A per-flag bound (`MAX_CONTEXTS_PER_FLAG = 10 000`, Rust's sweep rule)
caps memory. Context-free writers (list fetch, mobile poll and stream) stop writing the cache.

**Tech Stack:** Python (httpx, respx, pytest, mypy), TypeScript (jest, tsup, eslint), Go
(net/http, httptest), Java 11 (OkHttp, Gson, MockWebServer, JUnit 5), Swift (URLSession, XCTest,
`MockURLProtocol`), Kotlin (OkHttp, org.json, MockWebServer, JUnit 4).

**Ticket:** savvagent/otto-flags#127

**Spec:** `docs/specs/2026-10-15-sdk-context-cache-design.md`

## Global Constraints

- Worktree: `/home/robhicks/dev/otto-flags/.claude/worktrees/fix-127-sdk-context-cache`, branch
  `fix/127-sdk-context-cache`. Worktree only; trunk changes only through the PR.
- Gates:
  - Python (from the worktree root; `/tmp` is a nearly full tmpfs):
    `mkdir -p .tmp && export TMPDIR=$PWD/.tmp && v=$(mktemp -d)/venv && python3 -m venv "$v" && "$v/bin/pip" install -q -e 'packages/python-server[dev]' && cd packages/python-server && "$v/bin/pytest" -p no:cacheprovider && "$v/bin/mypy" --cache-dir=/dev/null src`
    (`.tmp/` is gitignored. To reuse a venv within a task, create it once at
    `v=$PWD/.tmp/venv` with the first two commands, then run `"$v/bin/pytest" -p no:cacheprovider`
    and `"$v/bin/mypy" --cache-dir=/dev/null src` from `packages/python-server`.)
  - pnpm (worktree root): `pnpm install --frozen-lockfile && pnpm lint && pnpm build && pnpm test`.
    For a quick loop inside one package: `cd packages/<pkg> && pnpm test` (jest).
  - Go (`go vet ./...`, `go test ./...`), Java (`mvn -B verify`), Swift (`swift test`): no
    toolchain on the host. Do NOT install one. Not run; record "not run: no toolchain" in the
    report. Go is known red (#17, and #113: other test files do not compile), Java's test sources
    do not compile (#113), iOS does not build (#119). Android has no gate. For these, write code
    you are confident compiles: follow the file's existing idioms and imports exactly.
- Evidence: report pass/fail/skip counts per suite (pytest, each jest project); skipped tests are
  not a pass.
- Invariants: the SDK REST contract is unchanged (same endpoints, same request bodies); SDKs never
  throw into the host app on a flag lookup (server SDKs return the default; iOS `throws` and
  Android `Result` keep their existing failure shapes); client keys never see targeting rules (no
  local evaluation); no end-user context persisted (in-memory only, bounded); `packages/` is MIT,
  no server code copied.
- Do not fix #113, #119, #124 (metadata fields) or #125 (mobile endpoint paths); do not make them
  worse (Go's `cache_test.go` already does not compile; leave it alone).
- Commit format: `fix(sdk): <imperative, lower-case>`; body explains why and ends `Refs #127`
  on intermediate commits; the last commit of the PR ends `Fixes #127` (the PR body carries
  `Fixes #127` too). No attribution of any kind (no Co-Authored-By, no "Generated
  with", no credit markers), in commits, code, comments or docs.
- Out-of-band: none. No version bumps. Changesets only for `@otto-flags/node-server` and
  `@otto-flags/sdk` (Tasks 2 and 3).

## File Structure

| File | Responsibility |
|---|---|
| **Create.** `packages/python-server/src/otto_flags/_context_key.py` | canonical context key |
| **Modify.** `packages/python-server/src/otto_flags/cache.py` | two-level map, bound, `context_key` kwarg |
| **Modify.** `packages/python-server/src/otto_flags/client.py`, `async_client.py` | prepare context first, key the cache, `track_error`, no list seeding |
| **Modify.** `packages/python-server/tests/test_cache.py`, `test_client.py`, `test_async_client.py` | tests |
| **Create.** `packages/node-server/src/context-key.ts`, `context-key.test.ts` | canonical context key |
| **Modify.** `packages/node-server/src/cache.ts`, `cache.test.ts`, `client.ts`, `client.test.ts` | two-level cache, evaluate, `trackError`, no list seeding |
| **Create.** `.changeset/node-server-context-cache.md`, `.changeset/sdk-context-cache.md` | patch changesets |
| **Create.** `packages/typescript/src/context-key.ts`, `packages/typescript/tests/context-key.test.ts` | canonical context key |
| **Modify.** `packages/typescript/src/cache.ts`, `src/client.ts`, `tests/cache.test.ts`, `tests/client.test.ts` | two-level cache, evaluate, no list seeding |
| **Modify.** `packages/go-server/cache.go`, `client.go` | two-level cache, evaluate on a copy |
| **Create.** `packages/go-server/context_cache_test.go` | httptest per-context tests |
| **Modify.** `packages/java-server/src/main/java/com/savvagent/ottoflags/sdk/FlagClient.java` | two-level cache, canonical key |
| **Create.** `packages/java-server/src/test/java/com/savvagent/ottoflags/sdk/ContextCacheTest.java` | MockWebServer per-context tests |
| **Modify.** `packages/ios-sdk/Sources/OttoFlagsSDK/OttoFlagsClient.swift` | two-level cache, sorted-keys key, poll/stream invalidate |
| **Create.** `packages/ios-sdk/Tests/OttoFlagsSDKTests/ContextCacheTests.swift` | MockURLProtocol per-context tests |
| **Modify.** `packages/android-sdk/src/main/kotlin/com/savvagent/ottoflags/sdk/OttoFlagsClient.kt` | two-level cache, canonical key, poll/stream invalidate, `flagUpdates` |
| **Create.** `packages/android-sdk/src/test/kotlin/com/savvagent/ottoflags/sdk/OttoFlagsClientContextCacheTest.kt` | MockWebServer per-context tests |
| **Modify.** each SDK's `README.md` where it describes caching | say what the cache is keyed by |
| **Modify.** `docs/SDK-DEVELOPER-GUIDE.md` (Suggested Cache Key Format) | key by flag and full context |

## Task Order & Rationale

Python first (full gate, sets the pattern and the test matrix), then the two TypeScript packages
(full gate), then Go, Java, iOS, Android (no gate; each mirrors the proven pattern), then the
guide. Tasks touch disjoint packages, so each is independently reviewable.

The common test matrix (each SDK, against its mock server; the mock answers by the request
body's `user_id` or attribute):

- **A/B:** evaluate flag `checkout` for user `a` (server: true), `b` (server: false), `a`, `b`:
  values true, false, true, false; reasons evaluated, evaluated, cached, cached; exactly 2 requests.
- **Attribute:** contexts equal except `attributes.plan` = `pro` vs `free` each reach the server
  and get their own answer.
- **Order:** the same attributes inserted in two orders → second call is `cached`, 1 request
  (where the map type preserves insertion order: Python dict, JS object, Java `LinkedHashMap`,
  Kotlin `linkedMapOf`; not for Go and Swift, whose maps have no insertion order and whose
  encoders sort keys).
- **Invalidate:** after A/B, invalidating `checkout` makes both users re-request (4 requests).
- **Bound** (server SDKs, unit test on the cache with the bound lowered): inserting more distinct
  context keys than the bound never holds more than the bound for that flag, and the newest entry
  is retrievable.

---

### Task 1: Python SDK

**Model:** sonnet

**Files:** create `src/otto_flags/_context_key.py`; modify `src/otto_flags/cache.py`,
`client.py`, `async_client.py`, `tests/test_cache.py`, `tests/test_client.py`,
`tests/test_async_client.py`, `README.md` (all under `packages/python-server/`).

- [ ] **Step 1: failing tests.**
  - `tests/test_client.py`, new class `TestContextCache` using `mock_api` (respx) and
    `server_body` from `conftest.py`: a route on
    `POST https://otto-flags.savvagent.com/api/flags/checkout/evaluate` with a `side_effect` that
    reads `json.loads(request.content)["context"]` and returns `server_body("checkout", user_id ==
    "a")` (or keyed on `attributes["plan"]`). Cover A/B, Attribute, Order (`{"plan": "pro",
    "region": "eu"}` vs `{"region": "eu", "plan": "pro"}`), Invalidate (`client.invalidate_cache
    ("checkout")`), "list fetch does not seed" (mock `GET /api/sdk/flags` returning one flag
    `checkout` `enabled: true`; call `get_all_flags("production")`, then `evaluate("checkout",
    FlagContext(user_id="b"))` → server answer false, reason `evaluated`), and `track_error`
    (enable telemetry is off in the fixture: patch `client._telemetry.track_error` with a
    recorder; after evaluating `a` (true) and `b` (false), `track_error("checkout", err,
    FlagContext(user_id="b"))` records `flag_enabled=False` and for `a` `True`).
    Also: evaluating with a caller `FlagContext` leaves the caller's object unmodified
    (`application_id` and `environment` still `None`).
  - `tests/test_async_client.py`: the same A/B, Invalidate and list-seed tests for
    `AsyncFlagClient` (follow the file's existing async fixtures and markers).
  - `tests/test_cache.py`: `get_entry("f", context_key="x")` misses after `set("f", True,
    context_key="y")`; `invalidate("f")` drops all contexts of `f` only; `keys()` lists a flag
    once however many contexts; bound: construct `FlagCache(ttl=60)`, set
    `cache._max_contexts_per_flag = 4`, insert 10 distinct context keys for `f`, assert
    `len(cache._cache["f"]) <= 4` after each insert and the last one is retrievable; same for
    `AsyncFlagCache`. Existing tests (no `context_key`) keep passing unchanged.
- [ ] **Step 2: run the Python gate; see the new tests fail** (A/B returns `cached` true for `b`;
  cache tests fail on the unknown kwarg).
- [ ] **Step 3: implement.**
  - `_context_key.py`: `def context_key(context: dict[str, Any]) -> str | None` returning
    `json.dumps(context, sort_keys=True, separators=(",", ":"), ensure_ascii=False)`, `None` on
    `TypeError` or `ValueError`. Docstring: why (cache correctness, #127; order-independent).
  - `cache.py`, both classes: `self._cache: dict[str, dict[str, CacheEntry]]`;
    `MAX_CONTEXTS_PER_FLAG = 10_000` module constant, copied to `self._max_contexts_per_flag` in
    `__init__`; `get`, `get_entry`, `set` take keyword-only `context_key: str = ""` (`set`'s
    existing positional parameters unchanged); `get_entry` deletes an expired entry (and an empty
    inner dict); `set` applies the bound: if the flag's inner dict has `>= max` entries and
    `context_key` is new, drop expired entries; if more than `max // 2` remain, clear the inner
    dict; then insert. `invalidate(key)` pops the flag's inner dict; `keys()` returns flags with at
    least one live entry; `FlagCache.__len__` counts live entries (`AsyncFlagCache` has no
    `__len__`; do not add one). Class docstrings say the cache is keyed by
    flag and context key.
  - `client.py`: add `_prepare_context(self, context) -> FlagContext` returning
    `dataclasses.replace(context)` if given else `FlagContext()`, then the existing fill-ins
    (`application_id` from config if unset; `environment` from config if unset). In `evaluate`,
    before the cache lookup: `eval_context = self._prepare_context(context)`,
    `ctx_key = context_key(eval_context.to_dict())`; look up only when `ctx_key is not None`
    (`self._cache.get_entry(flag_key, context_key=ctx_key)`); on success `set(...,
    context_key=ctx_key)` only when `ctx_key is not None`. Send `eval_context.to_dict()` as today.
    `track_error`: `ctx_key = context_key(self._prepare_context(context).to_dict())`;
    `flag_enabled = (self._cache.get(flag_key, context_key=ctx_key) if ctx_key is not None else
    None) or False`. `get_all_flags`: delete the cache-seeding loop and its comment. Telemetry
    calls keep passing what they pass today.
  - `async_client.py`: the same, with its own fill-ins (it fills only `application_id`; do not add
    `environment`) and `await`s.
  - `README.md`: in the features/cache text, one sentence: results are cached per flag and per
    evaluation context (the context as sent), for `cache_ttl` seconds.
- [ ] **Step 4: run the Python gate; all tests pass, mypy clean.**
- [ ] **Step 5: commit** `fix(sdk): cache python evaluations per flag and context` (body: why;
  `Refs #127`).

### Task 2: Node server SDK

**Model:** sonnet

**Files (under `packages/node-server/`):** create `src/context-key.ts`, `src/context-key.test.ts`;
modify `src/cache.ts`, `src/cache.test.ts`, `src/client.ts`, `src/client.test.ts`, `README.md`;
create `/.changeset/node-server-context-cache.md` (worktree root `.changeset/`).

- [ ] **Step 1: failing tests.**
  - `src/client.test.ts`, new `describe('context cache (#127)')`: `global.fetch` mocked with an
    implementation that parses `init.body` and returns `{ ok: true, json: async () => ({ key:
    'checkout', enabled: <by user_id or attributes.plan>, scope: 'application', variation: null,
    configuration: null, timestamp: 1700000000, version: 3 }) }`. Cover A/B, Attribute, Order,
    Invalidate (`client.invalidateCache('checkout')`), list-seed (a `GET /api/sdk/flags` mock
    returning `{ flags: [{ key: 'checkout', enabled: true, scope: 'application', environments:
    {}, version: 1 }], count: 1 }`, then `evaluate('checkout', { user_id: 'b' })` → false,
    `evaluated`), `trackError` reports the value cached for its own context (spy on the private
    telemetry's `trackError` via `jest.spyOn((client as any).telemetry, 'trackError')`). Build
    every test client with `enableTelemetry: false, enableRealtime: false`, so request counts
    include only evaluate calls.
  - `src/context-key.test.ts`: key order independence at depth (nested attribute objects),
    `undefined` fields dropped (equal to omitted), different values differ, a cyclic object
    returns `null`.
  - `src/cache.test.ts`: per-context miss, `invalidate(key)` drops all contexts, bound (lower
    via `(cache as any).maxContextsPerFlag = 4`, insert 10, inner map size ≤ 4, newest
    retrievable). Existing tests unchanged.
- [ ] **Step 2: run `cd packages/node-server && pnpm test`; see the new tests fail.**
- [ ] **Step 3: implement.**
  - `src/context-key.ts`: `export function contextKey(value: unknown): string | null` —
    `JSON.parse(JSON.stringify(value))` (drops `undefined`, applies `toJSON`), recursively
    rebuild plain objects with keys sorted (arrays keep order), `JSON.stringify` the result;
    `null` if anything throws or the first stringify returns `undefined`. Doc comment: why.
    Not exported from `index.ts`.
  - `src/cache.ts`: `Map<string, Map<string, CacheEntry>>`; `MAX_CONTEXTS_PER_FLAG = 10_000`
    constant, `private maxContextsPerFlag = MAX_CONTEXTS_PER_FLAG`; `get(key, contextKey = '')`,
    `getEntry(key, contextKey = '')`, `set(key, value, flagId?, configuration?, variation?,
    contextKey = '')` with the bound rule (spec §3); `invalidate(key?)` deletes the flag's inner
    map; `clear()` unchanged. Class doc says keyed by flag and context.
  - `src/client.ts`: `private prepareContext(context?: FlagContext): FlagContext` returning the
    existing `evalContext` expression. In `evaluate`, compute `evalContext` and `const ctxKey =
    contextKey(evalContext)` before the cache lookup; look up / set only when `ctxKey !== null`;
    send `evalContext` as today. `trackError`: `const ctxKey = contextKey(this.prepareContext
    (context)); const flagEnabled = (ctxKey === null ? null : this.cache.get(flagKey, ctxKey)) ??
    false;`. `getAllFlags`: remove the cache-seeding loop and comment.
  - `README.md`: one sentence on per-context caching near `cacheTtl`.
  - `.changeset/node-server-context-cache.md`: `"@otto-flags/node-server": patch` — "Cache each
    evaluation per flag and evaluation context, so one context's result is never returned for
    another; `getAllFlags` no longer fills the evaluation cache."
- [ ] **Step 4: run the pnpm gate from the worktree root; all pass.**
- [ ] **Step 5: commit** `fix(sdk): cache node evaluations per flag and context` (`Refs #127`).

### Task 3: TypeScript browser SDK

**Model:** sonnet

**Files (under `packages/typescript/`):** create `src/context-key.ts`,
`tests/context-key.test.ts`; modify `src/cache.ts`, `src/client.ts`, `tests/cache.test.ts`,
`tests/client.test.ts`, `README.md`; create `.changeset/sdk-context-cache.md` at the worktree
root.

- [ ] **Step 1: failing tests** in `tests/client.test.ts`, new `describe('context cache (#127)')`
  following the file's `flag key validation (#107)` block (save/restore `global.fetch`, client
  with `enableRealtime: false, enableTelemetry: false`). Mock as in Task 2 (the browser client
  reads `data.enabled`). Cover A/B via the `context` argument, A/B via `client.setUserId('a')` /
  `setUserId('b')` with no context argument, Attribute, Order, Invalidate (`clearCache()` and the
  realtime path is not testable here: use `clearCache`), list-seed (`getAllFlags('production')`
  then `evaluate` for `b` → `evaluated`). Note `fetchWithRetry`: only return `ok: true`
  responses so no retry delays. `tests/context-key.test.ts` and `tests/cache.test.ts` as in Task 2
  (this cache: `get(key, contextKey = '')`, `set(key, value, flagId?, contextKey = '')`,
  `keys()` = flags with at least one live entry).
- [ ] **Step 2: `cd packages/typescript && pnpm test`; see them fail.**
- [ ] **Step 3: implement** as Task 2, adapted: `evaluate` calls `this.buildContext(context)`
  before the cache lookup (after the overrides and `authFailed` checks, which stay first) and keys
  on it; a hit returns `{ key, value, reason: 'cached' }` as today; `getAllFlags` stops seeding;
  `getCachedFlags()` unchanged in signature. Own copy of `context-key.ts` (packages share no
  code). README: one sentence under Cache Management. Changeset `"@otto-flags/sdk": patch`, same
  wording adapted.
- [ ] **Step 4: pnpm gate from the worktree root; all pass.**
- [ ] **Step 5: commit** `fix(sdk): cache browser evaluations per flag and context` (`Refs #127`).

### Task 4: Go SDK

**Model:** sonnet

**Files (under `packages/go-server/`):** modify `cache.go`, `client.go`, `README.md`; create
`context_cache_test.go`. Leave `cache_test.go` alone: it already does not compile (it calls a
two-argument `Set`), so updating it is #113's job.

- [ ] **Step 1: tests (cannot be run: no Go toolchain).** `context_cache_test.go` in package
  `ottoflags`, modeled on `evaluate_contract_test.go` (reuse `newFlagKeyTestClient` from
  `flagkey_test.go` and the `serverBody` format from `evaluate_contract_test.go`): an `httptest.Server` that decodes `{"context": {...}}` and answers by
  `user_id` / `attributes.plan`, counting requests with `atomic.Int32`/`int32`. Cover A/B,
  Attribute, Invalidate (`client.InvalidateCache("checkout")`), and that the caller's `*Context`
  is not modified (ApplicationID stays empty when the client config sets one: construct that
  client with `NewClient(Config{...})` as `newFlagKeyTestClient` does, plus `ApplicationID`).
  Cache unit tests in the same new file: per-context miss, `Invalidate(flag)` drops all
  contexts, bound (set `c.maxContextsPerFlag = 4`, insert 10, `len(c.entries["f"]) <= 4`, newest
  present).
- [ ] **Step 2: implement.** `cache.go`: `entries map[string]map[string]cacheEntry`,
  `const maxContextsPerFlag = 10000` and a `maxContextsPerFlag int` field set in `newCache`;
  `Get(key, ctxKey string)`, `GetEntry(key, ctxKey string)`, `Set(key, ctxKey string, value
  bool, configuration map[string]interface{}, variation string)` with the bound rule;
  `Invalidate(key)` deletes the inner map. `client.go` `Evaluate`: before the cache lookup,
  `prepared := Context{}; if ctx != nil { prepared = *ctx }`, fill `prepared.ApplicationID`,
  `ctxJSON, err := json.Marshal(prepared)` (map keys sorted by encoding/json); on error return
  the default exactly as the later marshal error path does. Use `string(ctxJSON)` as the key for
  `GetEntry`/`Set`, and build the request body from `prepared` (the body bytes:
  `json.Marshal(map[string]interface{}{"context": prepared})` as today). README: one sentence on
  per-context caching near `CacheTTL`.
- [ ] **Step 3: self-check compile by reading:** every non-test call site of `Get`, `GetEntry`,
  `Set` updated; `gofmt`-style formatting (tabs).
- [ ] **Step 4: commit** `fix(sdk): cache go evaluations per flag and context` (`Refs #127`).

### Task 5: Java SDK

**Model:** sonnet

**Files:** modify `packages/java-server/src/main/java/com/savvagent/ottoflags/sdk/FlagClient.java`,
`packages/java-server/README.md`; create
`packages/java-server/src/test/java/com/savvagent/ottoflags/sdk/ContextCacheTest.java`.

- [ ] **Step 1: tests (cannot be run).** `ContextCacheTest` modeled on `EvaluateContractTest`
  (same imports, MockWebServer with a `Dispatcher` that parses the request body with Gson and
  answers by `context.userId`/attribute — note Gson sends Java field names, e.g. `userId`, as
  today). Cover A/B (`getRequestCount() == 2`), Attribute, Order (two `LinkedHashMap`s with the
  same entries in different insertion orders via `FlagContext.setAttributes`), Invalidate.
- [ ] **Step 2: implement.** `cache` becomes `ConcurrentHashMap<String,
  ConcurrentHashMap<String, CacheEntry>>`; `MAX_CONTEXTS_PER_FLAG = 10_000` (package-private
  static final). In `evaluate`, move "Prepare context" above the cache lookup; then
  `JsonElement contextJson = gson.toJsonTree(context)`; `String ctxKey = canonicalJson
  (contextJson)` inside a try that sets `ctxKey = null` on `RuntimeException`. Lookup/store only
  when `ctxKey != null`. The request body adds the same `contextJson` (`requestBody.add
  ("context", contextJson)`). `private static String canonicalJson(JsonElement e)`: objects →
  new `JsonObject` with `new TreeSet<>(obj.keySet())` order, recursively; arrays → new
  `JsonArray` recursively; primitives/null as is; return `toString()`. `getCachedEntry(flagKey,
  ctxKey)` and `setCachedValue(flagKey, ctxKey, …)` apply TTL and the bound (sweep under
  `synchronized (inner)`); `invalidateCache(flagKey)` removes the inner map. README: one sentence
  near `cacheTtl`.
- [ ] **Step 3: self-check compile by reading** (imports: `JsonArray`, `TreeSet`; Java 11 — no
  records, no `var` in fields).
- [ ] **Step 4: commit** `fix(sdk): cache java evaluations per flag and context` (`Refs #127`).

### Task 6: iOS SDK

**Model:** sonnet

**Files:** modify `packages/ios-sdk/Sources/OttoFlagsSDK/OttoFlagsClient.swift`,
`packages/ios-sdk/README.md`; create
`packages/ios-sdk/Tests/OttoFlagsSDKTests/ContextCacheTests.swift`.

- [ ] **Step 1: tests (cannot be run).** Modeled on `EvaluateContractTests.swift` (client built
  with `init(config:session:)` and `MockURLProtocol`, polling and WebSocket off as that file
  does). The handler parses the request body (`httpBody` or `httpBodyStream`, as that file reads
  it) and answers by `context.user_id`/attribute, counting requests. Cover A/B (reasons and a
  request count of 2), Attribute, Invalidate via `setConfigOverride`/`clearConfigOverride` (both
  users re-request). The cache write becomes `queue.sync` (Step 2), so no sleeps are needed.
- [ ] **Step 2: implement.** `flagCache: [String: [String: CacheEntry]]`;
  `private static let maxContextsPerFlag = 10_000`. In `evaluate`: build `evaluateBody`, run the
  existing `isValidJSONObject` guard, then `let contextKey = (evaluateBody["context"]).flatMap {
  try? JSONSerialization.data(withJSONObject: $0, options: [.sortedKeys]) }.flatMap { String(data:
  $0, encoding: .utf8) }` — all before the cache lookup. Lookup `flagCache[flagKey]?[contextKey]`
  only when non-nil. Cache write, now `queue.sync` (spec §6: an evaluate that returns has stored
  its result); the override setters' and `clearAllOverrides`' cache removals also become
  `queue.sync` (an evaluate after a setter returns must not see the old entry): if the flag's map has
  `>= maxContextsPerFlag` entries and the key is new, clear it; then insert. Override setters and
  `clearAllOverrides` keep their removal (now of the inner map / all). `handleWebSocketMessage`
  and `refreshFlags`: remove `flagCache[flagKey]` (all contexts) instead of writing an entry; keep
  their parsing guards (only the key is needed). Do not change endpoint paths (#125), the
  `session.data(for:)` calls (#119) or `track`. README Caching section: one sentence.
- [ ] **Step 3: self-check compile by reading** (optional chaining types; `[String: Any]` cast of
  `evaluateBody["context"]`).
- [ ] **Step 4: commit** `fix(sdk): cache ios evaluations per flag and context` (`Refs #127`).

### Task 7: Android SDK

**Model:** sonnet

**Files:** modify
`packages/android-sdk/src/main/kotlin/com/savvagent/ottoflags/sdk/OttoFlagsClient.kt`,
`packages/android-sdk/README.md` (if it describes caching); create
`packages/android-sdk/src/test/kotlin/com/savvagent/ottoflags/sdk/OttoFlagsClientContextCacheTest.kt`.

- [ ] **Step 1: tests (cannot be run).** Modeled on `OttoFlagsClientNetworkTest.kt` (its
  MockWebServer setup, client construction with polling/WebSocket off, `runTest`). A
  `Dispatcher` answering by the body's `context.user_id`/attribute. Cover A/B (`requestCount ==
  2`), Attribute, Order (`linkedMapOf` in two orders), Invalidate via
  `setVariationOverride`/`clearVariationOverride`, and `flagUpdates.value["checkout"]` equals
  the last evaluation's value. Also a pure test of `contextKey` (internal) for order independence.
  These depend on `org.json` working in local JVM unit tests, the existing setup gap (#113): do
  not add a test dependency for it.
- [ ] **Step 2: implement.** `flagCache = ConcurrentHashMap<String,
  ConcurrentHashMap<String, CacheEntry>>()`; `MAX_CONTEXTS_PER_FLAG = 10_000` in the companion.
  `internal fun contextKey(body: JSONObject): String` = `canonicalJson(body.get("context"))`
  with a private recursive `canonicalJson(value: Any?)` (spec §6). In `evaluate`: build `json =
  evaluationBody(userContext)` and `val ctxKey = contextKey(json)` before the cache lookup; lookup
  `flagCache[flagKey]?.get(ctxKey)`; store with the bound (synchronized on the inner map: if size
  `>= MAX` and key new, clear) then `_flagUpdates.value = _flagUpdates.value + (flagKey to
  enabled)`. Delete `updateFlagFlow()` and its other calls. `handleWebSocketMessage` and
  `refreshFlags`: `flagCache.remove(key)` per flag instead of writing; no flow publish. Override
  setters and `clearAllOverrides` unchanged (`remove`/`clear` now drop inner maps). Do not change
  endpoint paths (#125) or test setup (#113).
- [ ] **Step 3: self-check compile by reading.**
- [ ] **Step 4: commit** `fix(sdk): cache android evaluations per flag and context` (`Refs #127`).

### Task 8: SDK guide

**Model:** haiku

**Files:** modify `docs/SDK-DEVELOPER-GUIDE.md`.

- [ ] **Step 1:** under "Suggested Cache Key Format", replace the `flag:{org_id}:{flag_key}:
  {user_id}:{version}` guidance with: key a cached evaluation by the flag key and the full context
  sent in the request body (canonical JSON, keys sorted), never by flag key alone or by `user_id`
  alone (attributes and other fields change the answer); bound entries per flag; invalidate all of
  a flag's entries on an SSE event for it. Mention that the list endpoint's `enabled` is the
  environment switch, not a context's result, so it must not fill the evaluation cache.
- [ ] **Step 2:** commit `docs(sdk): describe the per-context evaluation cache key`, body ending
  `Fixes #127` (last commit of the implementation).
