# Python, Node, Go and Java SDKs report the result metadata the server sends

**Status:** DRAFT
**Issue:** #124

## 1. Brief

From #124:

> The Python, Node, Go and Java SDKs fill the evaluation result's metadata from `flagId`/`flag_id`,
> `description` and `variant`. The server's evaluate response (`docs/SDK-DEVELOPER-GUIDE.md`, Flag
> Evaluation; `crates/flags-api/src/sdk.rs` `EvaluateResponse`) sends none of these, so the
> metadata is always empty. It does send `version` and `scope`.
>
> The Rust SDK had the same gap and now fills `version` and `scope` (#109), keeping the old fields
> documented as always empty.
>
> **Acceptance criteria**
> - Each SDK's evaluation metadata carries the `version` and `scope` the server sends, as additive
>   fields.
> - The old fields stay (for compatibility) and are documented as never set by the server.
> - Each SDK has a test against the server's response shape.
>
> **Out of scope**
> - The Rust SDK (#109), and the TypeScript browser SDK.

## 2. Premise corrections

- **Cached results carry no metadata at all** in these SDKs (Python sync and async return
  `metadata=None` on a hit, Node omits it, Go returns `Metadata: nil`, Java passes `null`). The
  Rust SDK caches the metadata and returns it on a hit (`packages/rust-server/src/client.rs:284`).
  Filling only the evaluated path would make `version`/`scope` vanish on the next call for the
  same context, so the cache entries keep them too.
- `EvaluateResponse.version` is an `i32` and `scope` is `"application"` or `"enterprise"`
  (`crates/flags-api/src/sdk.rs:63-72`).

## 3. Assumptions

- Every Run Profile value comes from the Development Workflow section; none assumed.
- **Metadata is lenient.** `version` is used only when it is a JSON integer (not a boolean, not a
  float with a fraction), `scope` only when it is a JSON string; anything else leaves the field
  unset. A malformed metadata field never fails a lookup whose `enabled` is valid (the result is
  the server's; metadata is informational). Java's existing old-field reads call `getAsString()`,
  which throws on an object or array and would fail the lookup; they get the same guard.
- **Types:** Python `version: int | None`, `scope: str | None` on `FlagMetadata`; Node
  `version?: number`, `scope?: string`; Go `Version int64` (`json:"version,omitempty"`, 0 means
  not sent: the server's versions start at 1) and `Scope string` (`json:"scope,omitempty"`);
  Java `Long getVersion()` / `String getScope()` (null when not sent) with setters, like the
  existing fields.
- **Old fields stay and keep reading what they read today** (`flagId`/`flag_id`, `description`,
  `variant`), so a server or proxy that does send them still reaches the caller; their docs say
  the otto-flags server never sends them.
- **Cached results return the metadata of the evaluation they cache** (version and scope), as
  Rust does: a hit now returns a metadata object where it returned none. Of the old fields, a hit
  carries only what the entry already stores (Python and Node: `flag_id`/`flagId`; Go and Java:
  none); the server never sends them, so this differs only for a non-otto server. Java stores
  `version` and `scope` as entry fields and builds a fresh `Metadata` per hit (the class is
  mutable). Error results keep no metadata.
- An integral float such as `3.0` (which the server never sends) may be read as a version in
  Node and Java and not in Python and Go; accepted, and no test pins it.
- The exported cache classes stay source-compatible: Python `FlagCache.set`/`AsyncFlagCache.set`
  gain keyword-only `version` and `scope` (default `None`); Node `FlagCache.set` gains a trailing
  optional `metadata?: { version?: number; scope?: string }`; `CacheEntry` gains optional fields.
- **Changeset:** `@otto-flags/node-server` patch. No version bumps elsewhere.

## 4. Goal and success criteria

An evaluated or cached result's metadata carries the `version` and `scope` of the server response
that produced it.

- Against the server's exact response shape (`{"key","enabled","scope","variation",
  "configuration","timestamp","version","context"}`), each SDK's result has `version == 3` and
  `scope == "application"` on the evaluated call and on the cached call that follows.
- The old fields are unset (`None`/`undefined`/`""`/`null`) for that response.
- A response whose `version` is a string or `scope` a number still returns the server's value,
  with those metadata fields unset.

## 5. Scope

In: Python (`types.py`, `cache.py`, `client.py`, `async_client.py`, tests, README), Node
(`types.ts`, `cache.ts`, `client.ts`, tests, README, changeset), Go (`types.go`, `cache.go`,
`client.go`, a new test file, README), Java (`FlagEvaluationResult.java`, `FlagClient.java`, a new
test class, README).

Out: Rust (#109); the TypeScript browser SDK; iOS and Android (their results have no metadata
type); removing the old fields.

## 6. Architecture

Per SDK, on a successful evaluate response:

1. Read `version` (integer only) and `scope` (string only) next to the existing reads.
2. Build the metadata with them (old fields as today).
3. Store them in the cache entry for that context.
4. On a cache hit, build the same metadata from the entry.

- **Python.** `FlagMetadata` gains `version: int | None = None` and `scope: str | None = None`
  (after the old fields, so positional construction is unchanged). `CacheEntry` gains the same.
  `_response.py` gains `response_metadata(data) -> tuple[int | None, str | None]` (integer: `int`
  and not `bool`). Both clients build `FlagMetadata(flag_id=..., description=..., variant=...,
  version=..., scope=...)` on a miss, pass `version`/`scope` to `cache.set`, and on a hit return
  `metadata=FlagMetadata(flag_id=entry.flag_id, version=entry.version, scope=entry.scope)`.
- **Node.** `FlagEvaluationResult.metadata` gains `version?: number` and `scope?: string`;
  `CacheEntry` gains `version?`, `scope?`. `client.ts` reads `Number.isInteger(data.version)` and
  `typeof data.scope === 'string'`; the cached path returns `metadata: { flagId, version, scope }`
  from the entry.
- **Go.** `Metadata` gains `Version int64` and `Scope string`; `cacheEntry` gains `Version`,
  `Scope`; `cache.Set` gains `version int64, scope string` parameters (the cache is unexported).
  To stay lenient without failing `json.Unmarshal` on a wrong type, the response struct gets
  `Version json.RawMessage` and `Scope json.RawMessage`, each decoded separately into `int64` /
  `string` and ignored on error.
- **Java.** `Metadata` gains `Long version` and `String scope` with getters and setters.
  `FlagClient` reads them with `isJsonPrimitive()` + `isNumber()` (and an integral value) /
  `isString()`. `CacheEntry` gains `Long version` and `String scope`; the cached path returns a
  fresh `Metadata` with them (the class is mutable, so the entry never shares one).

Public API: additive only (new optional fields, new optional/keyword parameters, Go struct fields
and a new unexported-cache parameter). Behavior change: a cache hit returns a metadata object
(`version`, `scope`) instead of none.

## 7. Invariants impact

- **SDK REST contract:** unchanged; the SDKs now read two fields the contract already documents.
- **SDKs never throw into the host app on a flag lookup:** lenient reads; a bad metadata field
  never fails a lookup.
- **Licensing:** MIT packages, no server code copied.

## 8. Migration

None.

## 9. Out-of-band

None. SDK-only change: post-merge verification is server health only. Nothing is published. No
recurring-cost impact.

## 10. Error handling and edge cases

- `version` `true`/`1.5`/`"3"` → unset; `scope` `null`/`3` → unset.
- Error results keep no metadata.
- Overrides do not change metadata.

## 11. Testing approach

Gates: the Python gate (pytest + mypy) and the pnpm gate; tests fail before and pass after. Go
and Java: not run, no toolchain on the host (Go is also known red, #17; both test suites have
files that do not compile, #113); reported in the PR body. New tests next to each
package's evaluate-contract tests (`tests/test_client.py`, `tests/test_async_client.py`,
`src/client.test.ts`, `packages/go-server/metadata_test.go`, `MetadataTest.java`), using the
server-shaped body each package already has (`server_body` in Python's `conftest.py`,
`serverBody` in Go's `evaluate_contract_test.go`):

1. Evaluated result: `version == 3`, `scope == "application"`, old fields unset.
2. Cached result (same context again): the same `version` and `scope`.
3. Malformed metadata (`"version": "3"`, `"scope": 7`): value is the server's, metadata fields
   unset, no error.

## 12. Risks and open questions

- Go and Java are not compiled here. Mitigation: small, local changes; language-reviewer passes.
