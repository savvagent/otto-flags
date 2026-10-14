# Accept SDK telemetry text containing NUL

**Status:** DRAFT
**Issue:** #38

## 1. Brief

From #38 (found by the database review of #37 (#23); the behaviour predates it):

> SDK error telemetry text (`error_type`, `error_message`, `stack_trace`, `environment`) can contain
> `\0` (NUL), which is a valid character in a JSON string and in a Rust `String`. Postgres `text`
> rejects it (`invalid byte sequence for encoding "UTF8": 0x00`). So one event with a NUL fails the
> whole error batch, and the SDK's request returns an error.
>
> `sdk_` client keys are public, so anyone can send such a batch. The cost is small: one failed
> statement in the request's own transaction, and nothing else is affected. But a misbehaving SDK
> loses all the telemetry in that batch, and the endpoint answers with an error instead of a receipt.
>
> **Acceptance criteria**
> - [ ] `flags_core::telemetry` removes or replaces NUL in every SDK-reported text field before
>   binding it. Evaluation fields (`environment`, `variation`) are included, since they reach
>   `record_evaluations`, which has the same exposure.
> - [ ] A DB test records an error batch and an evaluation batch whose fields contain `\0` and
>   asserts they are accepted and stored without the NUL.

## 2. Premise corrections

- **`flag_key` is SDK-reported text too, and the brief does not list it.** Both `record_evaluations`
  and `record_errors` bind every event's `flag_key` into the key lookup
  (`flag_ids`, `crates/flags-core/src/telemetry.rs`, `key = ANY($3)`), before any insert. A NUL in
  one event's `flag_key` fails that lookup and with it the whole batch, so "every SDK-reported text
  field" must include it. It is handled differently from the stored fields (§6): a flag key can
  never contain NUL (`validate_key` in `crates/flags-core/src/flags.rs` allows only ASCII letters,
  digits, `-`, `_`, `.`), so such an event names no flag of this app and is counted as unknown,
  exactly as any other unknown key is.

## 3. Assumptions

- **Replace, not remove.** NUL becomes U+FFFD (the Unicode replacement character), not the empty
  string. Removing it would fold `"prod\0"` into `"prod"` (and `"a\0b"` into `"ab"`), silently
  merging a malformed value into a real environment or variation bucket; replacing keeps the
  malformed value distinct and visibly marked. The issue allows either.
- **Replace and truncate in one pass.** Every stored text field already goes through `truncate`
  (character count). NUL and U+FFFD are both one `char`, so replacing before or after truncating
  gives the same result; one helper does both, so no stored field can take one step without the
  other.
- **Only NUL.** Other control characters are valid in Postgres `text` and stored as reported;
  `inert` already flattens them when `flag_health` shows the text to an agent.
- **Only SDK telemetry.** Other request text that reaches a query (the evaluate endpoint's path
  key, MCP tool arguments) is outside #38; the evaluate path is filed separately (#45).
- Every Run Profile value was read from the Development Workflow section; none assumed.

## 4. Goal and success criteria

A telemetry batch whose text fields contain NUL is recorded like any other, and the SDK gets its
receipt.

- `record_errors` accepts events whose `environment`, `error_type`, `error_message`, or
  `stack_trace` contain NUL and stores each with every NUL replaced by U+FFFD.
- `record_evaluations` accepts events whose `environment` or `variation` contain NUL and buckets
  them under the replaced value.
- An event whose `flag_key` contains NUL, in either batch, is counted in `unknown_flags`; the rest
  of the batch is recorded.
- Truncation caps, `sane` timestamps, `reporter`, `Receipt`, and `Recorded` are otherwise unchanged;
  every existing telemetry test passes unchanged.
- `POST /api/telemetry/errors` and `/api/telemetry/evaluations` with NUL in those fields answer
  200 with the receipt.

## 5. Scope

**In:** `crates/flags-core/src/telemetry.rs` (the text helper, `flag_ids`'s key list); tests in
`crates/flags-core/tests/db.rs`, a unit test beside the helper, and one endpoint test in
`crates/flags-server/tests/e2e.rs`.

**Out:** the API handlers (`crates/flags-api/src/telemetry.rs`), schema, retention, `health`'s
display (`inert`), non-telemetry inputs.

## 6. Architecture

In `crates/flags-core/src/telemetry.rs`:

- `truncate(s, max)` becomes `stored(s, max)`: the first `max` characters of `s`, with every `'\0'`
  replaced by `'\u{FFFD}'`. Its doc comment says why (Postgres `text` cannot hold NUL; replace, not
  drop, so a malformed value never merges into a real one). All six existing call sites
  (`record_evaluations`: environment, variation; `record_errors`: environment, error_type,
  error_message, stack_trace) use it unchanged otherwise.
- `flag_ids` drops keys containing `'\0'` from the list it binds (with a comment: no flag key can
  contain NUL, so such a key is unknown, and binding it would fail the whole batch). Callers already
  count a key missing from the returned map as unknown; their code does not change.

No public interface changes: `EvalEvent`, `ErrorEvent`, `Receipt`, `Recorded`, and the function
signatures are unchanged; the REST contract (`docs/SDK-DEVELOPER-GUIDE.md`) is unchanged except
that a request it used to fail now succeeds.

## 7. Invariants impact

- **SDK-reported text is untrusted:** still bound as parameters, never interpolated; `inert` still
  flattens it for agents. U+FFFD is not a control character and passes through `inert` as-is.
- **SDK REST contract stable / additive:** a previously failing request now returns its receipt;
  nothing that succeeded before changes.
- **Tenant isolation, no SQL outside flags-core, runtime `sqlx::query` only, small shared
  database:** unchanged (no new statement; the lookup binds the same or fewer keys).
- **Evaluations counted, not stored; errors raw and truncated:** unchanged; U+FFFD is one character
  against the same caps (three bytes in UTF-8 instead of one, bounded by the request-body limit).

## 8. Migration

None.

## 9. Out-of-band

None. Deploy is CI-auto. The SDK telemetry endpoints are writes, so production verification is the
standard health and boot-log checks plus a read-only `flag_health` call on an existing flag; the
NUL path itself is proven by the e2e test, not exercised against production.

## 10. Error handling and edge cases

- A field that is only NUL becomes one U+FFFD (not empty); for errors, an environment of `"\0"` is
  therefore a named (if malformed) environment, not NULL.
- NUL at the truncation boundary: replaced within the kept characters; the cap is unchanged.
- All events in a batch have NUL keys: the lookup binds an empty array, returns nothing, every event
  is unknown, no insert runs.
- Any other database error still fails the batch, as before.

## 11. Testing approach

- Unit test in `telemetry.rs`'s `tests` module: `stored` replaces every NUL with U+FFFD and caps by
  characters.
- `crates/flags-core/tests/db.rs` (`#[sqlx::test(migrator = "flags_core::MIGRATOR")]`): one test
  recording an error batch with NUL in all four text fields plus an event with key `"f\0"`, and an
  evaluation batch with NUL in `environment` and `variation` plus a `"f\0"` event; asserts receipts
  (`accepted`, `unknown_flags`), and that the stored values equal the inputs with U+FFFD in place of
  NUL.
- `crates/flags-server/tests/e2e.rs`: POST both endpoints with `\u0000` in those JSON fields; both
  answer 200 with the expected receipt.
- Gates: test (with `DATABASE_URL`), clippy, fmt, the migrations guard, and `podman build`.

## 12. Risks and open questions

- The SDK evaluate endpoint (`crates/flags-api/src/sdk.rs`, `find_flag` with the path key) binds a
  request-supplied key the same way; a `%00` in it likely fails the query with an error instead of
  a 404. Out of scope for #38; filed as #45.
