# Non-Rust SDKs cache evaluations per flag and context

**Status:** DRAFT
**Issue:** #127

## 1. Brief

From #127:

> The Python, Node, Go, Java, iOS and Android SDKs cache evaluate results by flag key alone,
> ignoring the evaluation context. Within the cache TTL, the first context's result is returned
> for every later context: a flag targeted at user A, or rolled out to a percentage, gives user B
> whatever A got. The Rust SDK had the same bug and fixed it in #103 (#91; spec
> `docs/specs/2026-10-15-rust-sdk-context-cache-design.md`).
>
> **Acceptance criteria**
> - In each SDK, two `evaluate` calls for the same flag with different contexts within the TTL
>   each get the server's answer for their own context.
> - A repeated identical context within the TTL still avoids a request, or the spec records why
>   caching is dropped.
> - Each SDK has a test for this against a mock server.
>
> **Out of scope**
> - The Rust SDK (#103), and server changes.

Caller's additions: the TypeScript browser SDK too if affected; do not fix #113 (Go/Java test
suites do not compile, Android unit-test setup) or #119 (iOS build), but do not make them worse.

## 2. Premise corrections

- **The TypeScript browser SDK (`packages/typescript`, `@otto-flags/sdk`) has the same bug.**
  `FlagClient.evaluate` checks `this.cache.get(flagKey)` before it builds the context, so a
  different `context` argument, or a `setUserId` / `setAnonymousId` between calls, still gets the
  first answer. The framework packages (`angular`, `react`, `vue`, …) evaluate through it; the
  Angular service already keys its own subjects by flag and context and is then defeated by the
  client's cache. It is fixed here as a seventh SDK.
- **Other paths write context-free values into the same cache**, which evaluate would then serve
  to every context — the same bug by another door:
  - `get_all_flags` / `getAllFlags` (Python sync and async, Node, TypeScript) seed the cache with
    each listed flag's `enabled`. The list's `enabled` is the flag's switch for the environment
    (`docs/SDK-DEVELOPER-GUIDE.md`, Get Application Flags), not any context's result: a flag on
    at 10 % rollout lists as `enabled: true`.
  - iOS and Android write the flag list poll and the WebSocket messages into the cache per flag.
    (Today both reach endpoints the server does not serve, so nothing arrives; #125 moves them.)
- **`track_error` / `trackError` (Python sync and async, Node) read the cache per flag** to fill
  the error event's `flag_enabled`. With a per-context cache there is no per-flag value.
- **The Python, Go and Java SDKs prepare the context by mutating the caller's object** (filling
  `application_id`, and in Python sync `environment`), and only on a cache miss.
- **The mobile caches have no TTL.** An entry lives until an override setter, the poll or a stream
  message replaces it.

## 3. Assumptions

- Every Run Profile value comes from the Development Workflow section; none assumed.
- **Decision: keep server-side evaluation and key the cache by (flag key, context as sent),** as
  the Rust SDK does (#103). Local evaluation over `/api/sdk/flags` would put targeting rules in
  the SDK, which a client key never sees (Invariants). The evaluate endpoint's answer depends only
  on the flag key, the API key (fixed per client) and the request body's context.
- **The context key is the canonical JSON of the context object the SDK puts in the request body,
  after its fill-ins** (`application_id` from config; `environment` where the SDK fills it; the
  TypeScript SDK's `user_id`, `anonymous_id` and `language`). Canonical: object keys sorted at
  every depth, so attribute insertion order does not matter. Every field sent takes part; a wider
  key only costs hit rate, a narrower one would silently break when the server reads another field.
- **If the context cannot be serialized, the call skips the cache** (reads and writes nothing) and
  goes on as before; the request then fails as it does today, through the SDK's failure path.
- **Per-flag bound, as in Rust:** at most `MAX_CONTEXTS_PER_FLAG = 10 000` entries per flag. When
  a flag's map is full and the key is new: drop that flag's expired entries; if more than half the
  bound is still live, clear the flag's map. Either way at least half the bound is free after, so
  the sweep runs at most once per 5 000 new contexts. Hitting the bound costs misses, never a
  wrong answer. The mobile caches (no TTL) have nothing to sweep, so a full flag map is cleared.
- **The list fetch no longer writes the evaluate cache** (Python, Node, TypeScript). It still
  returns the flags. The README use cases ("bootstrap", "offline") read the returned list; none
  depends on the seed, which only ever produced the bug above.
- **Mobile poll and stream messages invalidate instead of writing:** each listed or announced
  flag's cached entries (all contexts) are dropped, so the next evaluate asks the server. This is
  what the guide's Caching section prescribes ("Invalidate on SSE events"). The endpoints stay as
  they are (#125). Android's public `flagUpdates` flow publishes, per flag, the value of the most
  recent evaluation (any context); poll and stream no longer publish into it.
- **`track_error` / `trackError` look up `flag_enabled` for the prepared context passed to them**
  (same preparation as evaluate, without mutating the caller's context); no entry → `false`, as
  today for a flag never evaluated.
- **Context preparation moves before the cache lookup** and stops mutating the caller's context in
  Python and Go (a copy is prepared). Java keeps its existing mutation (a `FlagContext` has no copy
  constructor; the body it sends is unchanged), now also on a cache hit.
- **The exported cache classes stay source-compatible.** Python `FlagCache`/`AsyncFlagCache`,
  Node and TypeScript `FlagCache` are public exports: their methods gain an optional context-key
  argument (keyword-only `context_key: str = ""` in Python, a trailing optional `contextKey = ''`
  in TypeScript) that defaults to "no context", so existing callers keep their behavior. Go's,
  Java's, iOS's and Android's caches are private.
- **Changesets:** `@otto-flags/node-server` and `@otto-flags/sdk` get a patch changeset. No version
  bumps elsewhere (Python, Go, Java, iOS, Android are unpublished; CLAUDE.md > Ship > release).

## 4. Goal and success criteria

A cached result is only ever returned for the same flag and the same context that produced it.

- In every SDK, two evaluations of one flag with different `user_id`s within the TTL each reach
  the mock server and get their own answer; repeating each gets its own cached answer
  (`reason == "cached"`), two requests total.
- Contexts that differ only in one attribute value each get their own answer.
- A repeated identical context is served from the cache, including when its attributes were
  inserted in a different order (where the language's maps can differ in order).
- Invalidating a flag drops its entries for every context.
- A flag's cache holds at most `MAX_CONTEXTS_PER_FLAG` entries.

## 5. Scope

In:

- Python (`packages/python-server`): `cache.py` (two-level map, bound, optional `context_key`),
  `client.py`, `async_client.py`, a shared context-key helper, tests, README.
- Node (`packages/node-server`): `cache.ts`, `client.ts`, a context-key helper, tests, README.
- TypeScript browser (`packages/typescript`): `cache.ts`, `client.ts`, a context-key helper,
  tests, README.
- Go (`packages/go-server`): `cache.go`, `client.go`, `types.go` (unchanged entry type), a new
  test file, README.
- Java (`packages/java-server`): `FlagClient.java`, a new test class, README.
- iOS (`packages/ios-sdk`): `OttoFlagsClient.swift`, a new test file, README.
- Android (`packages/android-sdk`): `OttoFlagsClient.kt`, a new test file, README.
- `docs/SDK-DEVELOPER-GUIDE.md` > Suggested Cache Key Format: key by flag and the full context.
- Changesets for the two published pnpm packages.

Out: the Rust SDK; server changes; local evaluation; result metadata (#124); the mobile endpoint
paths (#125); the Go/Java/Android test setup (#113); the iOS build (#119); a TTL for the mobile
caches.

## 6. Architecture

Every SDK gets the same shape:

```
cache: flag key -> (context key -> entry)
evaluate(flag, ctx):
  prepared = prepare(ctx)                      # fill-ins, no caller mutation (except Java)
  ckey = canonical_json(prepared)              # None/null on serialization failure
  if ckey and (entry = cache.get(flag, ckey)): return cached result (overrides applied as today)
  body = {"context": prepared}; POST /api/flags/{key}/evaluate
  on success: if ckey: cache.set(flag, ckey, ...)
invalidate(flag)  -> drop the flag's whole inner map
invalidate(all)   -> clear
```

Per SDK:

- **Python.** `FlagCache` and `AsyncFlagCache` hold `dict[str, dict[str, CacheEntry]]`. `get`,
  `get_entry` and `set` take keyword-only `context_key: str = ""`. `set` applies the bound.
  `invalidate(key)` pops the inner dict. `keys()` returns flags with at least one live entry;
  `__len__` counts live entries. A new private module `_context_key.py` holds
  `context_key(ctx_dict) -> str | None` (`json.dumps(..., sort_keys=True, separators=(",", ":"))`,
  `None` on `TypeError`/`ValueError`). `FlagClient` and `AsyncFlagClient` get a private
  `_prepare_context(context) -> FlagContext` (`dataclasses.replace` copy, then fill-ins exactly as
  each client does today: sync fills `application_id` and `environment`, async only
  `application_id`), used by `evaluate` and `track_error`. `get_all_flags` stops calling
  `cache.set`.
- **Node.** `FlagCache` holds `Map<string, Map<string, CacheEntry>>`; `get(key, contextKey = '')`,
  `getEntry(key, contextKey = '')`, `set(key, value, flagId?, configuration?, variation?,
  contextKey = '')`; bound in `set`. New `src/context-key.ts`: `contextKey(value): string | null`
  (`JSON.stringify` → `JSON.parse` to drop `undefined` and apply `toJSON`, then a recursive
  key-sorted rebuild of plain objects, then `JSON.stringify`; `null` if anything throws). A
  private `prepareContext(context)` builds `evalContext` as today; `evaluate` and `trackError` use
  it. `getAllFlags` stops calling `cache.set`.
- **TypeScript browser.** Same `FlagCache` change (`get(key, contextKey = '')`,
  `set(key, value, flagId?, contextKey = '')`, `keys()` returns flags with a live entry) and its
  own `src/context-key.ts` (packages do not share code today). `evaluate` calls `buildContext`
  before the cache lookup and keys on its result. `getAllFlags` stops calling `cache.set`.
- **Go.** `cache.entries` becomes `map[string]map[string]cacheEntry`; `Get(key, ctxKey)`,
  `GetEntry(key, ctxKey)`, `Set(key, ctxKey, value, configuration, variation)`; `Invalidate`
  unchanged in signature; bound in `Set`. `Evaluate` copies `*ctx` (or a zero `Context`), fills
  `ApplicationID` on the copy, `json.Marshal`s the copy (map keys are sorted by `encoding/json`)
  as the context key, and sends that same copy.
- **Java.** `cache` becomes `ConcurrentHashMap<String, ConcurrentHashMap<String, CacheEntry>>`.
  `evaluate` prepares the context (existing mutation) before the cache lookup, builds
  `gson.toJsonTree(context)`, and keys on a canonical string from a private
  `canonicalJson(JsonElement)` (objects re-emitted with sorted keys, recursively); a
  serialization failure skips the cache. The bound is applied in `setCachedValue` under a lock on
  the flag's inner map.
- **iOS.** `flagCache: [String: [String: CacheEntry]]`. `evaluate` builds `evaluationBody(for:)`,
  validates it (existing `isValidJSONObject` guard, now before the cache lookup), and keys on
  `JSONSerialization.data(withJSONObject: body["context"], options: [.sortedKeys])` as UTF-8. The
  body is sent as today. Override setters remove the flag's inner map. `handleWebSocketMessage`
  and `refreshFlags` remove the flag's inner map instead of writing an entry. Bound: a full flag
  map is cleared before inserting a new key.
- **Android.** `flagCache: ConcurrentHashMap<String, ConcurrentHashMap<String, CacheEntry>>`. A
  `internal fun contextKey(body: JSONObject): String` renders the body's `context` canonically
  (recursive: `JSONObject` keys sorted, `JSONArray` in order, strings via `JSONObject.quote`,
  `JSONObject.NULL` as `null`, other values `toString()`). Override setters and poll/stream remove
  the flag's inner map. `flagUpdates` gets `flagKey -> value` after each successful evaluation
  (`_flagUpdates.update { it + (flagKey to enabled) }`); poll and stream no longer publish into it.
  Bound: a full flag map is cleared before inserting a new key.

Public API: unchanged except additive optional parameters on the exported cache classes
(Python, Node, TypeScript). Behavior changes: a cache hit requires an identical context; the list
fetch does not seed the cache; `track_error`'s `flag_enabled` is the value cached for that
context; mobile poll/stream invalidate; Android `flagUpdates` publishes evaluation results only.

## 7. Invariants impact

- **SDK REST contract:** unchanged; same endpoints, same request bodies.
- **SDKs never throw into the host app on a flag lookup:** unchanged; a context-key failure skips
  the cache and the existing failure path handles the request.
- **Client keys public / no targeting rules in the SDK:** preserved; this is why local evaluation
  is rejected.
- **No end-user context persisted:** the caches are in-process memory only, bounded and (server
  SDKs) TTL'd; they now key on context the SDK already held in memory per request. Nothing is
  written to disk.
- **Licensing:** MIT packages; no server code copied.

## 8. Migration

None.

## 9. Out-of-band

None. SDK-only change: post-merge verification is server health only; no SDK is published (npm
publishing is Rob's and not automated).

## 10. Error handling and edge cases

- `context = None` and an empty context produce the same prepared context, so they share an entry.
- Attribute order does not matter (sorted keys). Distinct but equal-valued JSON (e.g. `1` vs `1.0`
  in Python) may key differently; that costs a miss, not a wrong answer.
- Failed lookups (non-2xx, no boolean `enabled`, transport errors) are still not cached.
- Overrides: applied on both paths as today; override setters still invalidate the flag (all
  contexts).
- Realtime (SSE) invalidation drops all contexts of the announced flag.
- Concurrency: Python sync uses the existing `RLock`, async the existing `asyncio.Lock`, Go the
  existing `RWMutex` (expired entries are not deleted under `RLock`, as today), Java a
  `ConcurrentHashMap` per flag with the bound's sweep under `synchronized (inner)`, Android the
  same, iOS keeps its existing `queue` discipline (writes on `queue`; reads are unsynchronized
  today and stay so — not made worse).

## 11. Testing approach

Gates (Development Workflow > Gates):

- pnpm: `pnpm install --frozen-lockfile && pnpm lint && pnpm build && pnpm test` (Node, TypeScript
  browser). Tests fail before the change and pass after.
- Python: the `packages/python-server/**` gate (pytest + mypy). Tests fail before and pass after.
- Go (`go vet`, `go test`), Java (`mvn -B verify`), Swift (`swift test`): no toolchain on the
  host; not run, reported in the PR body. Go is known red (#17) and its other test files do not
  compile (#113); Java's `FlagClientTest` does not compile (#113); iOS does not build (#119).
  Android has no gate. New tests in these SDKs follow each package's existing newest test file
  (`evaluate_contract_test.go`, `EvaluateContractTest.java`, `EvaluateContractTests.swift`,
  `OttoFlagsClientNetworkTest.kt`) and get language-reviewer scrutiny instead.

Tests per SDK, against that SDK's mock server (respx, jest `fetch` mock, `httptest`,
`MockWebServer`, `MockURLProtocol`, OkHttp `MockWebServer`):

1. Users A, B, A, B on one flag; the mock answers by the body's `user_id` (A → true, B → false):
   values true, false, true, false; reasons evaluated, evaluated, cached, cached; two requests.
2. Two contexts differing only in one attribute value each reach the server.
3. Same attributes inserted in different orders hit the cache (Python, Node, TypeScript, Go,
   Java; mobile where the map type can differ in order).
4. `invalidate` of the flag drops every context's entry (both re-requested).
5. Unit tests on the cache (server SDKs): the per-flag bound (constructor or module constant
   lowered in the test) never exceeds the bound and still stores the new entry.
6. Python/Node/TypeScript: `get_all_flags` no longer makes a later `evaluate` return `cached`.
7. Python/Node: `track_error` reports the value cached for its own context.

## 12. Risks and open questions

- Hit rate drops for callers that put per-request values (`user_agent`, `ip_address`) in the
  context. Accepted: correctness first; those fields can change targeting.
- Go, Java, iOS and Android changes are not compiled or tested here. Mitigation: changes kept
  small and in the style of each file; language-reviewer passes; the PR body records the gates as
  not run.
- iOS reads `flagCache` off `queue` while writes go through `queue` (pre-existing race); the
  two-level map does not change that. Recorded, not fixed (out of scope).
- `anonymous_id` in the TypeScript SDK is per browser, so different browsers already keyed
  differently; in a non-browser runtime it is generated per client, unchanged.
