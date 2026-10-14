# Answer not-found for a flag key containing NUL

**Status:** IMPLEMENTED (#60)
**Issue:** #45

## 1. Brief

From #45 (found while working #38; the behaviour predates it):

> `POST /api/flags/{key}/evaluate` and `POST /api/evaluate/{key}` (`crates/flags-api/src/sdk.rs`, `evaluate`) passes the
> path's flag key straight to `find_flag`, which binds it into a query. A key containing NUL (`%00`
> in the path) is a valid Rust `String` but Postgres `text` rejects it
> (`invalid byte sequence for encoding "UTF8": 0x00`), so the request fails with a server error
> instead of the `flag_not_found` it should get. It also logs as a server-side failure.
>
> No flag key can contain NUL (`validate_key` in `crates/flags-core/src/flags.rs` allows only ASCII
> letters, digits, `-`, `_`, `.`), so such a key can never match. `sdk_` client keys are public, so
> anyone can trigger it; the cost is one failed query and a misleading error/log line.
>
> **Acceptance criteria**
> - [ ] A flag key that cannot be a valid key (at least: one containing NUL) gets the same
>   `flag_not_found` response as any unknown key, without a query that can fail on it.
> - [ ] An e2e test in `crates/flags-server/tests/e2e.rs` evaluates a key containing `%00` and gets
>   the not-found response.
> - [ ] Other SDK-path inputs that reach a query (e.g. the `environment` query parameter of the
>   flag list, if bound) are checked for the same exposure.

## 2. Premise corrections

- **Today's failure is a 503, not a 500.** The failed bind is an internal `Error::Db`, which the
  SDK API answers as `503 temporarily_unavailable` with `Retry-After: 5`, logged as a server-side
  failure. So an SDK treats a NUL key as a transient outage and retries it, rather than serving its
  default for an unknown flag.
- **The same lookup serves the MCP tools.** `find_flag` (`FlagsExt`, `crates/flags-core/src/flags.rs`)
  is also reached through `require_flag` by `get_flag`, `flag_history`, `evaluate_flag`,
  `flag_health`, and the rollback tools, and the flag writers look a flag up by key through the
  private `lock` in the same file. An agent passing a key with NUL gets an internal error today, not
  `flag_not_found`. Guarding the two lookups in `flags-core`, not the SDK handler, fixes every
  caller in one place.
- **No other SDK-path input reaches a query with NUL** (AC 3, checked):
  - `GET /api/sdk/flags?environment=`: the environment is used only in memory
    (`SdkFlag::from_flag`), never bound.
  - The evaluate body's `context` (environment, attributes): evaluated in memory
    (`flags_core::eval`), never bound.
  - `GET /api/flags/stream` and `/api/sdk/enterprise-flags` take no parameters.
  - The SDK key (`x-sdk-key` / `Authorization`): `keys::resolve` binds only its SHA-256 hash, and an
    HTTP header value cannot contain NUL.
  - Telemetry text and flag keys: fixed by #38 (`stored`, `flag_ids`).

## 3. Assumptions

- **Guard NUL only, not full key validity.** The AC says "at least: one containing NUL". Skipping
  the query for any key `validate_key` would reject would also save a query, but it would tie
  lookups to today's creation rules: if those rules ever tightened, existing flags would silently
  become unreachable. NUL is the one property no stored key can have, whatever path created it,
  because Postgres `text` cannot hold it. This matches `flag_ids` (#38).
- **One predicate, three callers.** A `fn unstorable(key: &str) -> bool` in `flags.rs`
  (`key.contains('\0')`) is used by `find_flag`, `lock`, and `telemetry::flag_ids` (which today
  inlines the same check), so the rule has one definition. It is `pub(crate)` because telemetry uses it.
- **Same answer as any unknown key.** `find_flag` returns `Ok(None)`, and `lock` returns
  `Error::FlagNotFound`, exactly as for a key that does not exist; the SDK path then answers
  `404 flag_not_found`.
- Every Run Profile value was read from the Development Workflow section; none assumed.

## 4. Goal and success criteria

A flag key containing NUL is an unknown key everywhere a flag is looked up by key, with no query
issued for it.

- `POST /api/flags/checkout%00/evaluate` and `POST /api/evaluate/checkout%00` answer `404` with
  code `flag_not_found`, as `checkout-missing` does.
- `get_flag` (MCP) with key `"checkout\u0000"` answers the same not-found error as an unknown key.
- `find_flag`, `lock`, and `flag_ids` share one NUL predicate.

## 5. Scope

**In:** `find_flag`, `lock`, `flag_ids` in `crates/flags-core`; tests.

**Out:** NUL in other MCP tool arguments (app names, environments, reasons) that reach a query.
Those come from authenticated agents, not public keys; a malformed argument failing as an internal
error is a usability issue, not exposure. Filed as #61.

## 6. Architecture

`crates/flags-core/src/flags.rs`:

- `pub(crate) fn unstorable(key: &str) -> bool { key.contains('\0') }`, with a doc comment: no
  stored key can contain NUL (Postgres `text` rejects it), so such a key names no flag, and binding
  it would fail the statement.
- `find_flag`: `if unstorable(key) { return Ok(None); }` before the query.
- `lock`: the same guard, returning `Error::FlagNotFound { app, key: key.trim() }` as the
  not-found branch does.

`crates/flags-core/src/telemetry.rs`: `flag_ids` uses `keys.retain(|k| !flags::unstorable(k))`;
its comment points at `unstorable`.

No public interface changes: the endpoints and tools answer an existing error instead of a 503 (SDK) or `internal_error` (MCP).

## 7. Invariants impact

- **SDK REST contract is stable:** a request that failed with 503 now gets the documented 404.
  Additive.
- **SDK path never calls the platform / no SQL outside flags-core:** unchanged; one fewer query.
- **Tenant isolation:** unchanged; no new statement.

## 8. Migration

None.

## 9. Out-of-band

None.

## 10. Error handling and edge cases

- A key that is only whitespace plus NUL: `unstorable` sees the NUL; not found.
- A key with NUL that would match after trimming: impossible, trimming removes only whitespace.
- `ApiError::flag_not_found` echoes the key with `{:?}`, so NUL shows as `\0`, not a raw byte.

## 11. Testing approach

- e2e (`crates/flags-server/tests/e2e.rs`), `evaluating_a_key_with_nul_is_not_found`: with the
  public client key, both evaluate routes with `checkout%00` answer `404` and
  `error == "flag_not_found"`, and `error_description` holds no raw NUL; the real `checkout` still evaluates (200); MCP `get_flag` with
  `"checkout\u0000"` answers an error, not an internal one: it carries the same not-found message an
  unknown key gets.
- DB (`crates/flags-core/tests/db.rs`), `a_key_with_nul_finds_no_flag`: `find_flag` returns `None`,
  and a writer (`update_flag`) returns `Error::FlagNotFound`, for `"f\0"` when flag `f` exists.

Gates: test, lint, format, extra, and the `crates/**` podman build.

## 12. Risks and open questions

- None material. The change only short-circuits lookups that could only fail.
