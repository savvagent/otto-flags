# Refuse the flag keys `.` and `..`

**Status:** IMPLEMENTED (#110)
**Issue:** #105

## 1. Brief

From #105 (found while working #95):

> `validate_key` in `crates/flags-core/src/flags.rs` accepts any 1-128 character key of ASCII
> letters, digits, `-`, `_` and `.`, so `.` and `..` are valid flag keys. Neither can be evaluated
> over REST. `POST /api/flags/{key}/evaluate` carries the key as a path segment, and URL parsers
> treat `.`/`..` (and `%2e`/`%2e%2e`) as dot segments. The `url` crate also drops them from
> `path_segments_mut`, so the request reaches a different path. The Rust SDK refuses these two
> keys from #95 on. Other SDKs may send them and hit the wrong route.
>
> **Acceptance criteria**
> - `create_flag` (and any other path that sets a key) rejects `.` and `..` with the existing
>   `invalid_key` error.
> - A test pins the rejection.
> - Keys already stored are not migrated. A SELECT-only production check confirms none exist, or
>   lists them for Rob.
>
> **Out of scope:** changing the key charset otherwise.

## 2. Premise corrections

1. **There is no `invalid_key` error for flag keys.** `invalid_key` is the SDK API's code for a
   missing or unknown `sdk_`/`srv_` key (`crates/flags-api/src/error.rs`), an authentication
   failure (401). `validate_key` reports a malformed flag key as `Error::Invalid`, whose code is
   `invalid_argument` (MCP) and whose HTTP mapping is 400. "The existing error" is read as the one
   `validate_key` already returns for every other malformed key: `Error::Invalid`
   (`invalid_argument`). Reusing `invalid_key` would tell an agent its credentials are wrong.
2. **`create_flag` is the only path that sets a key.** `validate_key` has one caller
   (`FlagsExt::create_flag`), and no statement in `flags-core` updates
   `feature_flags.key` (keys "cannot be changed later", per the `create_flag` tool doc). Restore,
   rollback, and update address an existing flag by key and never set one.

## 3. Assumptions

- Only the exact keys `.` and `..` (after the existing trim) are refused. `...` and longer runs of
  dots, and keys that merely contain dots (`checkout.v2`), are not dot segments under RFC 3986 or
  the WHATWG URL standard and stay valid. `%2e` cannot occur: `%` is outside the charset.
- No migration and no CHECK constraint. A constraint would be redundant with code validation, and
  the previous release (which accepts these keys) would trip it after a rollback. Validation in
  code is enough because `create_flag` is the only writer.
- Production holds no such keys (checked 2026-10-15, SELECT-only, see section 8), so no data
  change is needed or handed off.
- App keys (`apps.rs`, same charset) are out of scope: SDKs never put an app key in a URL path
  (an SDK request names its app through its `sdk_`/`srv_` key).

## 4. Goal & success criteria

`create_flag` refuses `.` and `..` with `invalid_argument` and a message that says why, before
touching the database.

- `validate_key(".")` and `validate_key("..")` (also with surrounding whitespace) return
  `Error::Invalid`; `...`, `a.b`, `.a`, `a.` stay valid.
- `create_flag` with key `..` fails with code `invalid_argument` and inserts nothing.
- The `create_flag` MCP tool's `key` doc names the restriction.
- No migration file is added or changed.

## 5. Scope

**In:** `validate_key`, its unit test, one `flags-core` DB test through `create_flag`, the
`CreateFlagArgs::key` doc comment, one note in `docs/SDK-DEVELOPER-GUIDE.md`.

**Out:** any other charset change; app keys; variation names; SDK changes (the Rust SDK already
refuses these keys, #95; other SDKs are covered by the server no longer creating them); any
change to stored data.

## 6. Architecture

- `crates/flags-core/src/flags.rs` `validate_key`: after the charset check, refuse
  `.` and `..` with `Error::Invalid`: "flag key \".\" is a URL dot segment, which SDKs cannot
  evaluate over REST; choose another key". The check runs on the trimmed key, as the stored
  key is the trimmed key.
- `crates/flags-mcp/src/tools/flags.rs` `CreateFlagArgs::key` doc: add "not '.' or '..'". This
  changes the tool's JSON-schema description text only (no field, type, or name changes).
- `docs/SDK-DEVELOPER-GUIDE.md`: one additive note under Flag Evaluation that a flag key is never
  `.` or `..`, so an SDK may refuse them locally (as the Rust SDK does since #95).

Both REST routes that carry the key as a path segment are affected the same way:
`POST /api/flags/{key}/evaluate` and `POST /api/evaluate/{key}` (`crates/flags-api/src/lib.rs`).

No public interface is added. The MCP contract change is a narrower accepted input, which is the
point of the fix; the SDK REST contract is untouched.

## 7. Invariants impact

- Append-only migrations / rollout compatibility: no migration. The previous release still
  accepts these keys during rollout or after a rollback; that is the status quo, and production
  has none.
- SDK REST contract: unchanged (no route, field, or response shape changes).
- Database error text never reaches a tool response: the refusal is built in code before any SQL.
- Metering: the `create_flag` tool records its usage row before calling `FlagsExt::create_flag`
  (`crates/flags-mcp/src/tools/flags.rs`), so a refused key's usage row is rolled back with the
  tool's transaction, as for every other validation failure today: not billed. An over-quota
  caller sees the quota error before the key error.

## 8. Existing data

SELECT-only check run on 2026-10-15 against production `otto_flags` (read-only transaction):

```sql
SET default_transaction_read_only = on;
SELECT org_id, app_id, id, key, status FROM feature_flags WHERE key IN ('.', '..');
```

Result: no rows (production holds no flags yet). Nothing to migrate or hand to Rob. If it is
ever re-run and returns rows, those flags stay readable by MCP; the remedy (archive, recreate
under a new key) is Rob's call.

## 9. Out-of-band

None: no secret, config key, scope, migration, or registration.

## 10. Error handling & edge cases

- `" .. "` trims to `..` and is refused (consistent with what would have been stored).
- `""` and over-length keys keep their existing message (checked first).
- A key with a disallowed character keeps the charset message (checked before the dot rule).

## 11. Testing

- Unit test beside `validate_key` (`flags.rs` `mod tests`): refused and accepted cases above.
- `crates/flags-core/tests/db.rs`: `create_flag` with `..` returns `invalid_argument`, and
  `list_flags` (archived included) afterwards is empty.
- Gates: workspace test, clippy, fmt, migrations-unchanged check, `podman build`.

## 12. Risks & open questions

- Existing SDKs other than Rust may still send `.`/`..` for a key the server never created; they
  get a 404 or another route's response, which they already handle as "flag not found" and fall
  back to defaults. Not a regression.
