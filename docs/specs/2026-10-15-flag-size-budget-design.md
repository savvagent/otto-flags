# Bound a flag's size and an app's SDK flag list

**Status:** IMPLEMENTED (#135)
**Issue:** #123 (part 2 of 2: the SDK list bound; part 1, flag history retention, shipped in #131)

## 1. Brief

From #123 (found in the security review of #122):

> `GET /api/sdk/flags` and each SSE change event carry a flag's configuration plus every
> variation's. Since #115 each configuration is capped at 32 KiB, so one flag is bounded at about
> 21 x 32 KiB (~672 KiB), but the list response has no bound on the number of flags per app, is
> built in memory on every request with a public `sdk_` key, and an SDK that applies an
> evaluate-sized read cap to it fails on a large app. Separately, `flag_versions` keeps every
> snapshot with no retention, so a writer looping `update_flag` on a large flag grows it by the
> flag's size per (metered) write on the small shared database.
>
> **Acceptance criteria**
> - Decide and implement a bound for the SDK list response (flags per app, pagination, or a
>   per-flag total), documented in `docs/SDK-DEVELOPER-GUIDE.md`.
> - Decide on retention or a per-flag cap for `flag_versions`, compatible with `rollback_flag`
>   and `flag_history`.
>
> **Out of scope:** the per-configuration cap itself (#115).

Part 1 (#131, `docs/specs/2026-10-15-flag-history-retention-design.md`) met the second
criterion: flag versions are kept for 90 days and at least the newest 100 per flag. This spec
meets the first and closes #123.

## 2. Premise corrections

1. **One flag is bounded by more than 21 configurations.** Its `environments` also carry up to 50
   targeting rules per environment (`MAX_RULES`), each with up to 500 values
   (`MAX_RULE_VALUES`), in up to 20 environments (`MAX_ENVIRONMENTS`); rule values have no
   length cap of their own. The list and stream events carry `environments` (rules stripped for
   a client key, kept for a server key), so the per-flag bound must count them too.
2. **Neither a flag-count cap nor pagination alone bounds the list.** A count cap leaves each
   flag up to ~672 KiB plus rules, so any useful count gives a list of tens to hundreds of MiB.
   Pagination would be an additive query parameter, but every existing SDK reads one page and
   would silently drop the rest; it also does not bound the work a request causes, only its
   unit. The bound has to be on bytes.

## 3. Assumptions

- **Two byte budgets, enforced on write:**
  - **A flag's size is at most `MAX_FLAG_BYTES = 128 KiB` (131 072 bytes).** A flag's size is the
    byte length of its `environments`, `variations`, and `configuration` as stored (Postgres's
    text form of each `jsonb`, which is never shorter than the compact JSON the server sends;
    section 9). 128 KiB holds three maximal configurations with room for their environments
    (#115's tests store a flag and a variation configuration of 32 KiB each, about 65.6 KiB as
    stored, so 64 KiB would have refused what #122 accepts), or thousands of rule values; real
    flags are a few hundred bytes. It also bounds each history snapshot (part 1 keeps 90 days of versions and at
    least the newest 100), so the 100 kept indefinitely are at most about 12.8 MiB per flag.
  - **An app's active flags total at most `MAX_APP_FLAG_BYTES = 1 MiB` (1 048 576 bytes)**,
    counting each active flag's size plus `FLAG_OVERHEAD_BYTES = 256` (covering the per-flag
    fields the list adds: a key of up to 128 characters, `enabled`, `scope`, a 10-digit
    `version`, the `environments`/`variations`/`configuration` names, `null` for empty
    variations and configuration, braces, and the comma between flags: about 252 bytes worst
    case; a unit test pins a maximal empty `SdkFlag` at 256 bytes or fewer). The overhead
    also caps the number of active flags at 4 096 per app. With typical flags (~300-500 bytes
    plus overhead) an app holds about 1 500-2 000 flags. Archived flags do not count (the list
    leaves them out); restoring one is a write that counts.
  - So `GET /api/sdk/flags` is at most about 1 MiB plus a fixed ~150-byte envelope, and a stream
    event carries at most one flag (128 KiB plus its envelope).
- **A write is refused only if it grows past a budget.** A write is refused (`invalid_argument`,
  naming the size, the limit, and what to do) when the flag's new size exceeds `MAX_FLAG_BYTES`
  and is larger than before, or when the app's new active total exceeds `MAX_APP_FLAG_BYTES`
  and is larger than before. So a write that shrinks a flag, archives one, or leaves sizes as
  they were always lands, even on an app over budget (flags written before the limit, or a race;
  section 9). Every write path is checked: `create_flag`, `update_flag`, `set_flag_environment`,
  `archive_flag`/`restore_flag`, and `rollback_flag`.
- **Turning an environment off is never refused.** An environment patch that only sets
  `enabled = false` (the kill switch, and every auto-rollback) skips the check: `"false"` is one
  byte longer than `"true"`, and the incident path must not fail on a byte. The growth is small
  and bounded: `apply_env_patch` re-serializes the one environment through `EnvConfig`, and
  for an environment added to the app after the flag was created it inserts a fresh
  `{"enabled": false}` entry; one environment per write, at most 20 environments.
- **`rollback_flag` is checked**, unlike #115's per-configuration cap. That cap exempted
  rollback because a rollback can only restore a configuration that was itself written, so it
  never exceeds the cap. An app budget is different: other flags may have grown since the
  version being restored, so an unchecked rollback could take the app past the budget,
  repeatably. A refused rollback names the kill switch (`set_flag_environment` with
  `enabled=false`), which is the incident path and is never refused.
- **Measured in SQL, checked before the write.** One `SELECT` computes the new state's size from
  the values about to be written (bound as `jsonb` parameters), the flag's current size and
  status, and the sum over the app's other active flags. The check runs before the `INSERT` or
  `UPDATE`, so a refusal leaves the transaction clean (a caller that continues after an error
  never commits a refused write). The sum converts each of up to 4 096 active flags' `jsonb` to
  text, up to about 1 MiB of TOAST reads and text conversion per write; writes are metered MCP
  calls, so this is not an unauthenticated cost.
- **No lock for the app budget; the overshoot is bounded.** Two concurrent writes to different
  flags of one app each see the other's old state and can both pass. Serializing them would need
  an app-level lock in every flag write, including the rollback checker's, and new lock ordering
  between flag rows and that lock. Instead the overshoot is bounded: each concurrent write adds
  at most `MAX_FLAG_BYTES + FLAG_OVERHEAD_BYTES`, at most one per open transaction (5 pool
  connections per machine), and once an app is over budget every growing write is refused. So
  an app's active total stays under about `1 MiB + machines x 5 x 128.25 KiB` (`fly.toml` sets
  no machine count; Fly runs one or two `app` machines today, about 1.6-2.3 MiB). The SDK guide's
  recommended 4 MiB read cap leaves room for several machines.
- **No migration, no constraint.** Checks are in code. The previous release, during rollout or
  after a rollback, writes without checking; this release refuses only growth beyond the
  budgets, so state the previous release wrote never blocks shrinking it. The #115 production
  check (2026-10-15, SELECT-only) found 0 rows in `feature_flags`, so no app is over either
  budget.
- **The SDK REST contract is unchanged in shape.** No field, route, or parameter is added; the
  guide gains the documented bound and a recommended read cap.

## 4. Goal & success criteria

The SDK flag list and each stream event have a documented upper bound in bytes, enforced on
every write that could exceed it.

- A write that takes a flag's size over 131 072 bytes is refused with `invalid_argument`; a
  flag of exactly 131 072 bytes is accepted.
- A write that takes an app's active total over 1 048 576 bytes is refused with
  `invalid_argument`, from each write path (create, update, set environment, restore, rollback).
- A write that shrinks a flag, an archive, and an environment patch that only disables are
  accepted on an app already over budget.
- For an app at the budget, `GET /api/sdk/flags` (server key) returns a body of at most
  1 048 576 + 256 bytes (envelope).
- `docs/SDK-DEVELOPER-GUIDE.md` states both bounds and recommends list and stream read caps.
- No migration file is added or changed.

## 5. Scope

**In:** `crates/flags-core/src/flags.rs` (constants, size check, its calls),
`crates/flags-core/tests/db.rs`, a list-size test in `crates/flags-server/tests/e2e.rs`, the MCP
tool argument and description text that names limits (`crates/flags-mcp/src/tools/flags.rs`),
`docs/SDK-DEVELOPER-GUIDE.md`.

**Out:** pagination; SDK changes (their read caps are SDK work; the Rust SDK caps only the
evaluate response); a flag-count cap separate from the overhead; streaming the list response
from the database; per-org budgets; changing #115's per-configuration cap.

## 6. Architecture

- `flags.rs` constants: `pub const MAX_FLAG_BYTES: i64 = 128 * 1024;`,
  `pub const MAX_APP_FLAG_BYTES: i64 = 1024 * 1024;`, `pub const FLAG_OVERHEAD_BYTES: i64 = 256;`
  each with its reasoning as a doc comment.
- A SQL fragment for a flag's size, used in one place:
  `octet_length(environments::text) + octet_length(variations::text) +
  coalesce(octet_length(configuration::text), 0)`.
- `async fn check_size(tx, app_id, flag: Option<FlagId>, new: &FeatureFlag-ish) -> Result<()>`:
  one query
  ```sql
  SELECT octet_length($3::jsonb::text) + octet_length($4::jsonb::text)
           + coalesce(octet_length($5::jsonb::text), 0)                  AS new_size,
         (SELECT <size> FROM feature_flags WHERE org_id = $1 AND id = $6) AS old_size,
         (SELECT status = 'active' FROM feature_flags WHERE org_id = $1 AND id = $6) AS was_active,
         (SELECT coalesce(sum(<size> + 256), 0) FROM feature_flags
           WHERE org_id = $1 AND app_id = $2 AND status = 'active'
             AND id IS DISTINCT FROM $6)                                 AS others
  ```
  then in Rust: `old_total = others + (was_active ? old_size + 256 : 0)`, `new_total = others +
  (new active ? new_size + 256 : 0)`; refuse per section 3. Messages:
  - flag: `flag {key:?} would be {n} bytes (its environments, variations and configurations as
    stored); the limit is 131072 bytes. Remove rules, variations, or configuration`
  - app: `app {app:?}'s active flags would total {n} bytes; the limit is 1048576 bytes (each
    flag's size plus 256). Archive flags you no longer need, or shrink their configuration`;
    for a rollback the message adds `to stop a bad change now, turn the environment off with
    set_flag_environment enabled=false`.
- Calls: `create_flag` before its `INSERT` (no old flag); `commit_change` before its `UPDATE`,
  unless the caller passes the disable-only exemption. `commit_change` gains a parameter
  `check: SizeCheck` (`Enforce` | `Rollback` | `DisableOnly`; `Rollback` is `Enforce` whose refusals
  name the kill switch, passed by `rollback_flag`); `apply_env_patch` passes `DisableOnly` when
  the patch is `enabled: Some(false)` with every other field `None`, else `Enforce`; every other
  caller passes `Enforce`. The flag row lock is already held when `commit_change` runs, so
  `old_size` is the state being replaced.
- No change to `sdk.rs` or `stream.rs`: the bound is a property of what is stored.
- Guide: in "List All Flags" and "Real-time Updates", the two bounds, that the server measures
  them on the stored form, that an app can exceed the total briefly under concurrent writes
  (bounded), and a recommended read cap of 4 MiB for the list and 1 MiB per stream event; the
  Flag Evaluation paragraph that says "the list has no limit on the number of flags" is
  rewritten.

## 7. Invariants impact

- **SDK REST contract stable, additive:** no shape change; a documented bound is added.
- **Client keys are public:** unchanged; rules are still stripped for a client key. The budget
  counts rules, so a server key's list is the larger one and is the one bounded.
- **The database is small and shared:** one extra indexed `SELECT` per write, summing at most
  4 096 rows of one app. No new table. Active flags per app are now bounded; archived flags and
  their 100-version history floor (#131) are not, and are bounded in #139.
- **Tenant isolation:** the query runs in the caller's `Tx` with `org_id` in every predicate.
- **No SQL outside flags-core; runtime queries only:** yes.
- **Database error text never reaches a tool response:** refusals are built in code.
- **Every flag change is versioned / notify:** a refused write writes nothing.
- **Metering:** a refused write rolls back its usage row with the transaction, as for other
  validation errors.
- **Migrations:** none.

## 8. Out-of-band

None.

## 9. Error handling & edge cases

- **Measure vs. what is sent.** Postgres's `jsonb` text form puts a space after `:` and `,`; the
  server sends compact JSON. Numbers can print differently (`jsonb` keeps the decimal text it was
  given, e.g. `0.0000001`; the server sends `1e-7`), and the server's form is never longer for
  any value the API accepts. A test asserts the list body is at most the budget plus envelope.
- **Exactly at a limit** is accepted (`>` refuses).
- **Archived flags** count toward the per-flag cap when written (a write to an archived flag is
  checked like any other) but not the app total; restoring adds them back and is checked.
- **Over budget already** (legacy or race): shrinking, archiving, and disabling land; nothing
  that grows does.
- **Concurrent writes** may overshoot the app total, bounded as in section 3.
- **The rollback checker's auto-rollback** is a disable-only patch, so it never fails on size.

## 10. Testing

- DB tests (`crates/flags-core/tests/db.rs`):
  - `a_flag_is_capped_at_128_kib`: a flag grown with variations and configuration to exactly
    131 072 bytes is accepted; one byte more is refused `invalid_argument` naming `131072`; refused from
    `create_flag`, `update_flag`, `set_environment`.
  - `an_apps_active_flags_are_capped_at_1_mib`: fill an app near the budget with flags of known
    size; the next `create_flag` that crosses it is refused naming `1048576`; archiving one then
    lets the create succeed; restoring the archived one is refused; a rollback that would cross
    is refused with the kill-switch hint.
  - `writes_that_do_not_grow_land_over_budget`: put an app over budget directly in SQL (as a
    previous release could); a shrinking `update_flag`, `archive`, and a disable-only
    `set_environment` succeed, an enabling one that grows is refused.
- E2E (`crates/flags-server/tests/e2e.rs`): an app filled to the budget, with flags carrying
  non-ASCII strings, a fractional `rolloutPercentage`, and a long number in a configuration,
  lists with a body of at most `MAX_APP_FLAG_BYTES + 256` bytes.
- Unit (`flags.rs`): a maximal empty `SdkFlag` (128-character key, version `i32::MAX`)
  serializes, with its trailing comma, to at most `FLAG_OVERHEAD_BYTES`.
- Gates: workspace test, clippy, fmt, migrations-unchanged, `podman build`.

## 11. Risks & open questions

- 1 MiB per app may be small for an app with thousands of rule-heavy flags. None exist; the
  constant is one line to raise, and raising it is compatible with SDKs whose list read cap
  stays above it.
- The bounds are measured on stored text, which an agent cannot see directly; refusal messages
  give the computed size.
