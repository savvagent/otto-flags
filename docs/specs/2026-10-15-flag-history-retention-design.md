# Keep a flag's newest 100 versions

**Status:** DRAFT
**Issue:** #123 (part 1 of 2: flag history retention; the SDK list bound is part 2)

## 1. Brief

From #123 (found in the security review of #122):

> `GET /api/sdk/flags` and each SSE change event carry a flag's configuration plus every
> variation's. [...] Separately, `flag_versions` keeps every snapshot with no retention, so a
> writer looping `update_flag` on a large flag grows it by the flag's size per (metered) write on
> the small shared database.
>
> **Acceptance criteria**
> - Decide and implement a bound for the SDK list response (flags per app, pagination, or a
>   per-flag total), documented in `docs/SDK-DEVELOPER-GUIDE.md`.
> - Decide on retention or a per-flag cap for `flag_versions`, compatible with `rollback_flag`
>   and `flag_history`.
>
> **Out of scope:** the per-configuration cap itself (#115).

The two criteria are independent (different tables, code paths, and contracts), so they ship as
two PRs. This spec covers the second criterion; the list bound has its own spec and PR, which
closes #123.

## 2. Premise corrections

None. `flag_versions` (`crates/flags-core/migrations/0001_baseline.sql:138`) has no retention:
`record_version` (`crates/flags-core/src/flags.rs:672`) inserts one full snapshot per change and
nothing deletes them except the `ON DELETE CASCADE` from `feature_flags`, which only an org
deletion triggers (flags are archived, never deleted).

## 3. Assumptions

- **A per-flag count cap, not an age-based retention.** Keep the newest
  `MAX_RETAINED_VERSIONS = 100` versions of each flag. A count cap bounds the table per flag
  however fast it is written, which is the growth the issue describes (a loop of writes); an age
  limit would not (a fast loop stays inside any window) and would empty the history of a flag
  nobody has touched for a while, which is exactly the flag whose last known-good state matters.
- **100.** `flag_history` returns at most 200 rows (`limit.clamp(1, 200)`) and defaults to 20;
  agents roll back to the previous version or a recent one. 100 is five times the default view
  and keeps any rollback an incident plausibly needs, while bounding one flag's history at 100
  snapshots. A snapshot is the flag's name, description, status, environments, variations and
  configuration; with part 2's per-flag cap that is at most about 64 KiB, so 100 versions are at
  most about 6.4 MiB per flag before TOAST compression, and in practice a few hundred KB.
- **Pruned synchronously, in the writer's transaction.** `record_version` deletes the flag's
  versions older than the newest 100 right after inserting the new one. It runs in the same
  tenant transaction under the same flag row lock as the write, touches only that flag's rows
  through the existing `UNIQUE (flag_id, version)` index, and at steady state deletes one row.
  No background job, so no `flags_core::maintenance::begin` and no new cross-org access.
- **Existing long histories are pruned on the flag's next write**, not by a sweep. The #115
  production check (2026-10-15, SELECT-only) found 0 rows in `feature_flags` and
  `flag_versions`, so there is nothing to backfill; a sweep would be code that never has work.
- **No migration.** The retention is in code. The previous release, during rollout or after a
  rollback, keeps inserting without pruning; the next write by this release prunes. Nothing the
  previous release reads depends on old versions existing: its `rollback_flag` already reports
  `version_not_found` for a missing version.
- **`rollback_events.flag_version` may name a pruned version.** It records which version an
  auto-rollback created (no foreign key). After 100 further changes that version is gone from
  `flag_history`; the event still says which version it was. Acceptable: the event is the
  record of what happened, not a pointer that must resolve.

## 4. Goal & success criteria

A flag's history is bounded at its newest 100 versions, and `rollback_flag` and `flag_history`
behave predictably at the bound.

- After any write, a flag has at most 100 rows in `flag_versions`: versions
  `current - 99 ..= current` (fewer for a flag with fewer versions).
- `flag_history` returns the retained versions, newest first; its tool description states that
  only the newest 100 are kept.
- `rollback_flag` to a retained version works as before; to a pruned version it fails with
  `version_not_found`, whose message says only the newest 100 versions are kept and points to
  `flag_history`.
- The default rollback (previous version) always works for a flag with at least two versions.
- No migration file is added or changed.

## 5. Scope

**In:** `crates/flags-core/src/flags.rs` (constant, prune in `record_version`, module docs),
`crates/flags-core/src/error.rs` (`VersionNotFound` message), the `flag_history` and
`rollback_flag` tool descriptions (`crates/flags-mcp/src/tools/insight.rs`,
`crates/flags-mcp/src/tools/flags.rs`), DB tests.

**Out:** the SDK list bound and per-flag size cap (part 2); age-based retention; retention of
`rollback_events` or any other table; a configurable retention.

## 6. Architecture

- `flags.rs`: `pub const MAX_RETAINED_VERSIONS: i32 = 100;` with a doc comment giving the
  reasoning above.
- `record_version`, after its `INSERT`:
  ```sql
  DELETE FROM flag_versions
   WHERE org_id = $1 AND flag_id = $2 AND version <= $3 - MAX_RETAINED_VERSIONS
  ```
  bound with the org, the flag id, and the version just recorded (the constant is bound as a
  parameter, not interpolated). Both `create_flag` (version 1, deletes nothing) and
  `commit_change` (every other change, including `rollback` and `auto_rollback`) go through
  `record_version`, so every write path prunes. The statement uses the
  `UNIQUE (flag_id, version)` index; `org_id` keeps it a tenant query under RLS.
- `rollback_flag` is unchanged: it looks up the target snapshot and returns `VersionNotFound`
  when the row is absent, which is now also the pruned case. Its range check (`1 ..= current -
  1`) stays: a legacy flag whose history has not yet been pruned can still be rolled back to an
  old version until its next write.
- `error.rs`: `VersionNotFound`'s message becomes
  `flag {key:?} has no version {version}; only the newest 100 versions of a flag are kept. Call
  flag_history to see them`, the number formatted from the constant.
- Tool descriptions: `flag_history` "The newest versions of a flag (up to the 100 kept), newest
  first: ..."; `rollback_flag` mentions it can restore any version `flag_history` shows.
- Module docs of `flags.rs` state the retention.

No public interface is added. The MCP tools' output shapes are unchanged.

## 7. Invariants impact

- **Every flag change is versioned:** still exactly one new version per change, written with
  the change; pruning removes only versions at least 100 older than it.
- **The database is small and shared / no unbounded tables:** `flag_versions` becomes bounded
  per flag. One indexed `DELETE` per write; no fan-out, no extra connection.
- **Tenant isolation:** the `DELETE` runs in the caller's tenant `Tx` with `org_id` in the
  predicate; RLS applies. No new table.
- **Append-only / rollout-compatible migrations:** no migration.
- **Request paths use `begin_live`; background code uses `maintenance::begin`:** no new
  transaction is opened; pruning rides the write's existing transaction.
- **Metering:** unchanged; no new billable action.

## 8. Out-of-band

None.

## 9. Error handling & edge cases

- A write that fails after `record_version` rolls back the prune too (same transaction).
- `version - 100 <= 0` for the first 100 versions: the `DELETE` matches nothing.
- A rollback to version `v` where `v` is still retained but becomes pruned by the rollback's own
  new version (`v = current - 99`): the snapshot is read before the new version is recorded, so
  the rollback succeeds; afterwards `v` is gone from the history, and the new version holds the
  same state.
- Concurrent writers to one flag are serialized by the flag row lock, so the prune never races
  an insert of the same flag.

## 10. Testing

DB tests in `crates/flags-core/tests/db.rs`:

- `flag_history_keeps_the_newest_100_versions`: create a flag and make 104 further changes
  (version 105); `flag_versions` holds exactly 100 rows for it, versions 6..=105; `flag_history`
  with limit 200 returns 100 rows, first 105, last 6.
- `rollback_to_a_pruned_version_is_version_not_found`: on that flag, `rollback_flag` to 5 fails
  with `version_not_found` and a message naming the 100-version bound; rollback to 6 (the oldest
  retained) succeeds and restores version 6's state as version 106, after which version 6 is
  pruned and 7..=106 remain; the default rollback works.
- An existing test that counts `flag_versions` rows stays green (fewer than 100 versions).

Gates: workspace test (dedicated Postgres), clippy, fmt, migrations-unchanged check,
`podman build`.

## 11. Risks & open questions

- An agent that wants to restore a state more than 100 changes old cannot. That state is in no
  agent's working view (`flag_history` defaults to 20), and the constant is one line to raise.
- The prune makes each write slightly heavier (one indexed `DELETE`); negligible next to the
  write's own `UPDATE`, `INSERT`, and `pg_notify`.
