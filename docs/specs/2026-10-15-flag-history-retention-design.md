# Retain flag history for 90 days and at least 100 versions

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

1. `flag_versions` (`crates/flags-core/migrations/0001_baseline.sql:138`) has no retention:
   `record_version` (`crates/flags-core/src/flags.rs:672`) inserts one full snapshot per change,
   and only the `ON DELETE CASCADE` from `feature_flags` removes them (flags are archived, never
   deleted).
2. **`flag_versions` is the audit trail of flag changes**, not only rollback storage:
   `crates/flags-core/src/audit.rs` leaves flag changes out of `audit_events` because each one is
   recorded here with its actor and reason. So retention must not be something a caller can
   drive. A count-only cap pruned on write (this spec's first draft) let anyone with
   `flags:write` erase the record of a change by making 100 more, which is a repudiation hole.
   Retention is therefore by age, in a background sweep.

## 3. Assumptions

- **A version is removed only when it is both older than `VERSION_RETENTION_DAYS = 90` days and
  not among its flag's newest `MIN_RETAINED_VERSIONS = 100` versions.**
  - The age condition is what makes the trail non-repudiable: no change younger than 90 days is
    ever removed, however many writes follow it. 90 days is the retention of rollback events,
    evaluation rollups, and rejected usage events (`telemetry::EVAL_RETENTION_DAYS`,
    `usage::REJECTED_RETENTION_DAYS`), and `flag_health`'s longest window, so a change and the
    health data it can be judged by age out together.
  - The count condition keeps rollback depth on quiet flags: a flag changed rarely keeps its
    newest 100 versions however old they are, so its last known-good state is never swept.
  - Removing old versions also removes their actor and reason. Accepted: older than 90 days and
    more than 100 changes back. `audit.rs`'s module doc says so.
- **Pruned by the hourly background sweep, never on a request path.** `flags::sweep_versions`
  runs in `flags-server`'s existing `sweep_loop` beside the telemetry and usage sweeps, through
  `flags_core::maintenance::begin` (unpinned, as `otto_app`, with the `app.maintenance` marker).
  No tool or SDK request deletes history.
- **Bounded batches.** Each maintenance transaction deletes at most 1 000 rows (chosen by `id`
  through a subquery with `LIMIT`); the sweep repeats until a batch is short. Short transactions
  on the small shared database; a backlog drains in one sweep run.
- **The growth bound this gives.** A flag's history is at most its newest 100 versions plus the
  versions of the last 90 days (plus up to an hour until the next sweep). Each version costs a
  billed MCP write, and part 2 of #123 (next PR) caps a flag's size at 64 KiB, so the table's
  growth is bounded by what writers pay for over 90 days, like the other retention tables.
  Until part 2 ships, a snapshot can be about 672 KiB (#115's 32 KiB per configuration, 21
  configurations) plus rules. Real flags are a few hundred bytes.
- **A migration is needed, and it is policy-only and additive.** `flag_versions` has only its
  tenant policy (`org_id = current_org()`), so an unpinned maintenance transaction sees no rows.
  `0006_flag_versions_retention.sql` adds, as 0003 did for the telemetry tables, a `FOR SELECT`
  and a `FOR DELETE` policy granting access only when no org is pinned **and** the transaction
  carries `app.maintenance = 'on'` (#27). Pinned transactions are unaffected; an unpinned
  transaction without the marker (a request path that forgot to pin) still sees nothing. The
  previous release never touches `flag_versions` unpinned, so during rollout and after a
  rollback it behaves as before (and simply does not sweep). `SET LOCAL lock_timeout = '3s'` as
  in 0003 and 0005.
- **Existing data:** the #115 production check (2026-10-15, SELECT-only) found 0 rows in
  `feature_flags` and `flag_versions`, so the first sweep deletes nothing.

## 4. Goal & success criteria

`flag_versions` is bounded by a documented, time-based retention that no caller can drive, and
`rollback_flag` and `flag_history` behave predictably at the bound.

- After a sweep, a flag keeps every version younger than 90 days and at least its newest 100;
  nothing else of it remains.
- A flag with more than 100 recent versions keeps all of them (writes cannot push out recent
  history).
- The sweep deletes across every org through `maintenance::begin`; an unpinned transaction
  without the marker can neither see nor delete `flag_versions` rows.
- `rollback_flag` to a removed version fails with `version_not_found`, whose message states the
  retention; `flag_history` and `rollback_flag` tool descriptions state it.
- One new migration, `0006`, policy-only; no existing migration changes.

## 5. Scope

**In:** `crates/flags-core/migrations/0006_flag_versions_retention.sql` (new);
`crates/flags-core/src/flags.rs` (constants, `sweep_versions`, module docs);
`crates/flags-core/src/error.rs` (`VersionNotFound` message); `crates/flags-core/src/audit.rs`
and `crates/flags-core/src/maintenance.rs` (module docs); `crates/flags-server/src/main.rs`
(`sweep_loop`); the `flag_history` and `rollback_flag` tool descriptions
(`crates/flags-mcp/src/tools/insight.rs`, `crates/flags-mcp/src/tools/flags.rs`); DB tests.

**Out:** the SDK list bound and per-flag size cap (part 2); pruning on write; retention of
`audit_events`; configurable retention.

## 6. Architecture

- `flags.rs`:
  - `pub const VERSION_RETENTION_DAYS: i32 = 90;` and `pub const MIN_RETAINED_VERSIONS: i32 =
    100;`, documented with the reasoning in section 3.
  - `pub async fn sweep_versions(db: &Db) -> Result<u64>`: loop
    ```sql
    DELETE FROM flag_versions WHERE id IN (
      SELECT v.id FROM flag_versions v
       WHERE v.created_at < now() - make_interval(days => $1)
         AND v.version <= (SELECT max(w.version) FROM flag_versions w
                            WHERE w.flag_id = v.flag_id) - $2
       LIMIT $3)
    ```
    with `VERSION_RETENTION_DAYS`, `MIN_RETAINED_VERSIONS`, `SWEEP_BATCH = 1000`, each batch in
    its own `maintenance::begin` transaction, committed; stop when a batch deletes fewer than
    the batch size; return the total. The newest version always equals the flag's current
    version (every change records one), so `max(version)` needs no read of `feature_flags`; it is
    served by `UNIQUE (flag_id, version)`.
- `0006_flag_versions_retention.sql`:
  ```sql
  SET LOCAL lock_timeout = '3s';
  CREATE POLICY flag_versions_retention_read ON flag_versions
    FOR SELECT USING (current_org() IS NULL
                      AND current_setting('app.maintenance', true) = 'on');
  CREATE POLICY flag_versions_retention ON flag_versions
    FOR DELETE USING (current_org() IS NULL
                      AND current_setting('app.maintenance', true) = 'on');
  ```
  Both permissive; they OR with the tenant policy, so pinned access is unchanged. No restrictive
  policy is needed: unlike 0001's telemetry retention policies, these already require the
  marker.
- `main.rs` `sweep_loop`: call `flags_core::flags::sweep_versions(&db)` with the same logging
  shape as its siblings (`"swept expired flag versions"`, `"flag-version sweep failed"`).
- `rollback_flag` is unchanged: a removed version is `VersionNotFound`, as any absent version
  was. `error.rs`: the message becomes `flag {key:?} has no version {version}; versions older
  than 90 days are removed once a flag has 100 newer ones. Call flag_history to see the
  versions kept`, numbers formatted from the constants.
- Tool descriptions: `flag_history` and `rollback_flag` state the retention in one sentence.

Interfaces: no new tool, route, field, or scope. Behavior narrowing of the MCP contract (not
additive): a version older than 90 days with 100 newer ones, which `rollback_flag` could restore
before, is now `version_not_found`. The SDK REST contract is untouched.

## 7. Invariants impact

- **Append-only, rollout-compatible migrations:** one new file, `0006`, additive policies only;
  the previous release does not read `flag_versions` unpinned, so nothing it does changes.
- **Tenant isolation:** the new policies admit only unpinned transactions with the maintenance
  marker, which only `maintenance::begin` sets; `tenant_isolation_is_enforced_on_every_tenant_
  table` and the boot isolation check stay green; a test proves an unmarked unpinned
  transaction sees and deletes nothing.
- **Background code uses `maintenance::begin`; request paths `begin_live`:** the sweep is the
  only deleter and runs in `sweep_loop`.
- **Every flag change is versioned:** unchanged; the sweep removes only versions past both
  bounds, never the current one (it is always among the newest 100).
- **The database is small and shared:** batches of 1 000 rows per short transaction, hourly; one
  pool connection at a time; the table becomes bounded by time and billed writes.
- **No SQL outside flags-core; runtime queries only:** the sweep is in `flags-core`;
  `main.rs` only calls it.

## 8. Out-of-band

- Migration `0006` applies at boot under the advisory lock (agent: confirm `applying migrations`
  in the boot logs after deploy). No secret, scope, or config key.

## 9. Error handling & edge cases

- A sweep failure is logged by `sweep_loop` and retried next hour, as its siblings.
- A flag with fewer than 101 versions: `max - 100 < every version`, nothing matches.
- A rollback concurrent with a sweep: the sweep never removes a version within the newest 100,
  and the rollback's target is either retained (it reads it) or already gone
  (`version_not_found`).
- The 3 s lock timeout on `CREATE POLICY`: if a long transaction holds `flag_versions`, the
  migration fails, rolls back, and the machine crash-loops until it applies (the documented
  migration failure path, `docs/deploy/fly.md`).

## 10. Testing

DB tests in `crates/flags-core/tests/db.rs`:

- `flag_versions_are_swept_by_age_beyond_the_newest_100`: org A, flag `f` with 105 versions,
  backdated (superuser pool) to 91 days for versions 1..=103; flag `recent` with 105 versions,
  all current; org B, flag `b` with 3 versions all backdated. `sweep_versions` returns 5: `f`
  keeps 6..=105 (version 6 is old but among the newest 100), `recent` keeps all 105, `b` keeps
  all 3. `rollback_flag` on `f` to 5 is `version_not_found` naming 90 and 100; to 6 succeeds.
  A second sweep returns 0.
- `flag_versions_need_the_maintenance_marker_unpinned`: as `otto_app` unpinned without the
  marker, `SELECT count(*)` sees 0 and `DELETE FROM flag_versions` deletes 0; with the marker
  it sees every org's rows; pinned still sees only its own.

Gates: workspace test (dedicated Postgres), clippy, fmt, migrations append-only check (prints
nothing: 0006 is an addition), `podman build`.

## 11. Risks & open questions

- History grows with writes for 90 days. Writes are billed, and part 2 caps a flag at 64 KiB;
  a hard per-flag ceiling would reintroduce caller-driven erasure, so none is set.
- An agent cannot restore a state older than 90 days that has 100 newer versions; the constants
  are one line each to raise.
