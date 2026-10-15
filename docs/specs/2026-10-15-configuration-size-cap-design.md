# Cap the size of a flag configuration on write

**Status:** DRAFT
**Issue:** #115

## 1. Brief

From #115 (found while working #111):

> The server puts no size limit on a flag's `configuration` (or on a variation's configuration).
> `crates/flags-core` caps descriptions, reasons, rules and variations, but a configuration is
> bounded only by the MCP request body limit. Every SDK evaluate response carries it, so one
> large configuration costs every lookup, and SDKs that bound the response they read (the Rust
> SDK reads at most 1 MiB since #111) fall back to the default for that flag.
>
> **Acceptance criteria**
> - `create_flag`, `update_flag` and `set_flag_environment` refuse a configuration whose
>   serialized JSON exceeds a documented limit (for example 32 KiB), with a clear `Invalid`
>   message. A test covers the boundary.
> - The limit is documented in `docs/SDK-DEVELOPER-GUIDE.md` so SDKs can size their response cap
>   above it.
> - Existing stored configurations over the limit keep evaluating (no migration rewrites data);
>   the check applies to writes only.
>
> **Out of scope:** SDK changes.

## 2. Premise corrections

1. **`set_flag_environment` takes no configuration.** Its arguments (`enabled`,
   `rolloutPercentage`, `rules`, `defaultVariation`) never set a flag's or a variation's
   configuration (`crates/flags-mcp/src/tools/flags.rs`, `FlagsExt::set_environment` in
   `crates/flags-core/src/flags.rs`). The configuration writers are `create_flag` and
   `update_flag`: each sets the flag's `configuration` and, through `variations`, every
   variation's `configuration`. Those two get the check; `set_flag_environment` needs none.
2. **`rollback_flag` also writes configuration**, by restoring a stored snapshot. It is not
   checked (section 3): it can only restore a state that was itself written, so it never
   introduces a configuration that a capped write refused, and a rollback (the incident path)
   must not fail on a configuration stored before the cap existed.

## 3. Assumptions

- **Limit: 32 KiB (32 768 bytes) per configuration, inclusive**, measured as the length in bytes
  of its compact `serde_json` serialization, the form the server emits it in (jsonb may
  normalize some numbers between request and storage; the difference is negligible). It applies separately to the flag's configuration and to each variation's configuration.
  An evaluate response carries one configuration (the variation's, else the flag's), so its
  configuration part is at most 32 KiB, far below the Rust SDK's 1 MiB read cap.
- **Per configuration, not per flag.** The AC names "a configuration". The list endpoint and the
  SSE notification carry a flag's configuration and all of its variations (at most 20,
  `MAX_VARIATIONS`), so one flag is at most about 21 × 32 KiB there. That is a bound where there
  was none; a tighter combined budget is not asked for and is left out.
- **No migration and no CHECK constraint.** The previous release (no cap) runs against the same
  schema during rollout and after a rollback; a constraint would make its writes fail. Validation
  in code covers every writer except rollback (section 2).
- **Writes of other fields are unaffected.** `update_flag` checks only the fields its patch
  carries: renaming a flag whose stored configuration predates the cap succeeds. Re-submitting an
  over-limit configuration (or a `variations` set containing one) is refused, as for any new
  write.
- **Existing data:** checked 2026-10-15 SELECT-only (read-only transaction) against production
  `otto_flags`: `feature_flags` and `flag_versions` hold 0 rows, so nothing exceeds the cap and no
  data change is needed or handed to Rob.

## 4. Goal & success criteria

No write through MCP can store a configuration whose JSON exceeds 32 KiB, and the limit is part
of the documented SDK contract.

- `create_flag` and `update_flag` refuse a flag configuration of 32 769 bytes with
  `invalid_argument`, naming the size and the limit; 32 768 bytes is accepted.
- The same for a variation's configuration, naming the variation.
- A flag whose stored configuration is over the limit still evaluates (returns it) and can be
  renamed through `update_flag`.
- `docs/SDK-DEVELOPER-GUIDE.md` states the limit; the `create_flag`/`update_flag` tool argument
  docs state it.
- No migration file added or changed.

## 5. Scope

**In:** `crates/flags-core/src/eval.rs` (constant, size helper, check in `validate_variations`),
`crates/flags-core/src/flags.rs` (check in `validate_configuration`), unit and DB tests, the MCP
tool argument docs in `crates/flags-mcp/src/tools/flags.rs`, `docs/SDK-DEVELOPER-GUIDE.md`.

**Out:** SDK changes; rollback; any change to stored data; a combined per-flag budget; limits on
`environments` (already bounded by `MAX_RULES`/`MAX_RULE_VALUES`).

## 6. Architecture

- `eval.rs`: `pub const MAX_CONFIGURATION_BYTES: usize = 32 * 1024;` and
  `pub(crate) fn json_len(v: &Value) -> usize`, which serializes `v` with `serde_json::to_writer`
  into a byte-counting `io::Write` (no allocation of the serialized text). A serialization error
  (not possible for a `Value`) yields `usize::MAX`, so it fails closed.
  `pub(crate) fn check_configuration_size(what: &str, c: &Value) -> Result<(), String>` returns
  `"{what} is {n} bytes as JSON; the limit is {MAX_CONFIGURATION_BYTES} bytes"` when over (built
  from the constant, so raising it is one line).
- `validate_variations` (`eval.rs:458`): after parsing, for each variation with a configuration,
  `check_configuration_size(&format!("variation {name:?} configuration"), c)?`.
- `validate_configuration` (`flags.rs:192`): after the NUL check,
  `eval::check_configuration_size("configuration", c).map_err(Error::Invalid)?`. JSON `null`
  (which `update_flag` treats as "clear") is 4 bytes and passes.
- MCP docs: `CreateFlagArgs::configuration`/`variations` and `UpdateFlagArgs::configuration`/
  `variations` mention "at most 32 KiB as JSON" per configuration. Tool schema description text
  only.
- `docs/SDK-DEVELOPER-GUIDE.md` Flag Evaluation: a note that a configuration (flag or variation)
  is at most 32 KiB as JSON when written, that flags stored before the limit may exceed it, and
  that SDKs should read responses well above it (the Rust SDK reads 1 MiB).

No public interface is added; the SDK REST contract is unchanged (no field, route, or shape).
The MCP contract narrows accepted input, which is the fix.

## 7. Invariants impact

- Append-only / rollout-compatible migrations: no migration.
- SDK REST contract: unchanged; the guide gains a documented bound.
- Database error text never reaches a tool response: the refusal is built in code before SQL.
- Metering: as for other validation failures, the tool's usage row rolls back with its
  transaction; a refused write is not billed.
- Versioning/notify: a refused write commits nothing, so no version and no notification.

## 8. Out-of-band

None.

## 9. Error handling & edge cases

- Exactly 32 768 bytes: accepted. 32 769: refused.
- Non-ASCII text counts its UTF-8 bytes; control characters count their escaped form (`\n` is 2
  bytes, U+0001 is 6), matching what the server sends.
- The NUL check runs first, so a NUL-carrying configuration keeps its NUL message.
- A variations set with several over-limit configurations reports the first in name order
  (`BTreeMap`).

## 10. Testing

- Unit (`eval.rs` tests): `json_len` equals `serde_json::to_vec(v).len()` for assorted values;
  `validate_variations` accepts a variation configuration of exactly the limit and refuses one
  byte more with a message naming the variation.
- DB (`crates/flags-core/tests/db.rs`): `create_flag` with a 32 768-byte configuration succeeds,
  32 769 fails `invalid_argument`; `update_flag` likewise for `configuration` and for
  `variations`; a flag whose configuration is set over the limit directly in SQL still evaluates
  to it through `eval::evaluate` and can be renamed with `update_flag`.
- Gates: workspace test, clippy, fmt, migrations-unchanged check, `podman build`.

## 11. Risks & open questions

- The list endpoint and SSE events are bounded per flag only by 21 × 32 KiB, and the list is
  unbounded in flag count (pre-existing). Not in scope.
- 32 KiB may be small for a client that stores large configuration blobs; none exist in
  production, and the constant is one line to raise (raising is compatible with every SDK whose
  read cap stays above it).
