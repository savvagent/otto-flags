# Fence the usage shipper's claims and require the maintenance marker on the usage outbox

**Status:** IMPLEMENTED (#72)
**Issues:** #67, #68 (one PR: both change `ship_once` in `crates/flags-core/src/usage.rs` and the
`usage_outbox` tables)

## 1. Brief

From #67 (found by the security review of #64 (#63); the behaviour predates it):

> `ship_once` (`crates/flags-core/src/usage.rs`) claims due rows by pushing `next_attempt_at`
> forward by `claim_lease` (120 s by default), then calls the platform. If that call (or the work
> after it) outlasts the lease, a second shipper (another machine, or the final flush on shutdown)
> can claim the same rows. Both then hold overlapping ids: each ships them, and each deletes them
> after its receipt. The platform's `event_id` dedupe limits the billing impact (the second
> delivery counts as a duplicate), but a rejected event can be copied into `usage_outbox_rejected`
> twice, and since #64 the loser's insert copies nothing and logs a warning.
>
> **Acceptance criteria**
> - [ ] A shipper whose claim lapsed cannot act on rows another shipper has since claimed: for
>   example, the final insert/delete also matches the `attempts` value this shipper set when it
>   claimed (fencing), or the lease is guaranteed to exceed the platform client's request timeout
>   plus the transaction.
> - [ ] A test covers two overlapping claims of the same row (e.g. a lease of 0 and two
>   `ship_once` calls) and shows the row is recorded once.

From #68 (found by the database review of #64 (#63); the behaviour predates it):

> `usage_outbox` and `usage_outbox_rejected` (`0001_baseline.sql`) still use the baseline policy
> `org_id = current_org() OR current_org() IS NULL`, so any unpinned transaction reads and writes
> every org's rows. `0003_unpinned_sweep_reads.sql` (#27) closed that for the retention tables by
> also requiring the transaction-local `app.maintenance = 'on'` marker that only
> `flags_core::maintenance::begin` sets, because a request path that forgot to pin is unpinned too.
> The usage shipper (`ship_once`, `reschedule`) uses `db.pool()` directly, outside
> `maintenance::begin`, which the CLAUDE.md invariant ("Unpinned cross-org access goes only through
> `flags_core::maintenance::begin`") does not allow.
>
> **Acceptance criteria**
> - [ ] The shipper's unpinned reads and writes on `usage_outbox` and `usage_outbox_rejected` run
>   inside `maintenance::begin` (or a documented equivalent that sets the marker).
> - [ ] A new, additive migration requires the marker for unpinned access to both tables,
>   compatible with the previous release during rollout and after a rollback (the previous
>   release's shipper must keep working, or the gap must be named and accepted).
> - [ ] `tenant_isolation_is_enforced_on_every_tenant_table` (or a sibling test) shows an unpinned
>   transaction without the marker sees no `usage_outbox` rows.

## 2. Premise corrections

- **#67: since #64 a sequential lapse no longer double-records a rejection, but a lapsed shipper
  still acts on the new holder's rows.** With the insert scoped to the batch's ids and the delete
  in the same transaction, whichever shipper commits first copies and deletes; the other copies
  nothing. What remains: (a) the lapsed shipper deletes, and records as rejected, rows the new
  holder has claimed and is still shipping, so the new holder's receipt is applied to rows that are
  gone; (b) its `reschedule` overwrites the new holder's `next_attempt_at`/`last_error`; and (c)
  two final transactions running *concurrently* can both copy the same rejected row, because each
  `INSERT … SELECT` reads the row before either `DELETE` commits (read committed). (c) is the
  remaining double-record; fencing has to close it too, not only the sequential case.
- **#68: in production the new policy changes nothing today.** Production's pool connects as the
  superuser attach role (`docs/deploy/fly.md`), which RLS does not apply to; only pinned tenant
  transactions drop into `otto_app` (`SET LOCAL ROLE`, in `otto_tenant::Db::begin`), and
  `maintenance::begin` does not. So both the previous release's shipper and this one keep working
  in production whatever the policy says. The marker matters under a non-exempt pool role (the
  deployment shape `0003` was written for, #12/#27), exactly as `0003`'s policies do. This is the
  same scope `0003` accepted; making background transactions drop into `otto_app` is out of scope
  (Section 5).

## 3. Assumptions

- **Fence on `attempts`, no new column.** Every claim increments `attempts` and nothing else
  changes it, so `(id, attempts)` as returned by the claim identifies this claim. A second claim of
  the same row changes `attempts`; the first shipper's fence then matches nothing. A claim-token
  column would need a migration for no gain. (`reschedule` reads `attempts` for backoff but does
  not write it.)
- **Fence every write after the claim**: the final delete-and-record and `reschedule`. A lapsed
  shipper's receipt or failure is about a delivery that the new holder repeats; the new holder's
  outcome is the one recorded.
- **Delete first, record from what was deleted, in one statement.** The final step becomes one
  statement: a fenced `DELETE … RETURNING` in a CTE feeding the `INSERT INTO
  usage_outbox_rejected`. A row is recorded as rejected only if this shipper's fenced delete
  removed it, and a row can be deleted once, so a rejection is recorded at most once even when two
  final statements race (closes premise (c)). Under read committed a `DELETE` blocked by a
  concurrent claim's row lock re-checks its `WHERE` (the fence) on the new row version, so it
  skips a row claimed in the meantime.
- **`ship_once` still returns how many rows it claimed** (its documented contract, and what
  `run` compares with `cfg.batch`); rows lost to a newer claim are logged, not subtracted.
- **`maintenance::begin` returns `sqlx::Result`** instead of `crate::error::Result`, so the shipper
  (whose error type is `ShipError::Db(sqlx::Error)`) can use it without a lossy mapping. It can only
  fail with a `sqlx::Error`; existing callers keep using `?` into `crate::Error` through its
  `From<sqlx::Error>`.
- **Accepted gap (#68 rollout and rollback):** under a non-exempt pool role, the previous
  release's shipper (unpinned, no marker) would see no `usage_outbox` rows once `0005` is applied,
  during the rollout overlap and after a rollback, until a release with this change runs. Usage
  then accumulates in `usage_outbox` (written by pinned tool transactions, which the new policy
  does not affect) and ships late; nothing is lost, and the platform dedupes on `event_id`. Under
  production's exempt pool role there is no gap at all (Premise corrections). Enforcement, when on,
  counts unshipped rows on top of the platform's count, so a backlog does not let an org exceed
  its plan.
- Run Profile values all come from the section; none assumed. No status labels exist, so no label
  step runs.

## 4. Goal and success criteria

A shipper acts only on rows it currently holds, and nothing unpinned reaches the usage outbox
without the maintenance marker.

- Two overlapping `ship_once` calls with a lease of 0 on one rejected row: the first (lapsed)
  shipper's completion leaves the row in `usage_outbox` and records nothing; the second records it
  in `usage_outbox_rejected` exactly once and deletes it.
- `ship_once`'s claim, final delete-and-record, and `reschedule` each run in a
  `maintenance::begin` transaction; no `db.pool()` query remains in the shipper.
- Under `otto_app` with no org pinned: without the marker, `usage_outbox` and
  `usage_outbox_rejected` read 0 rows, `UPDATE`/`DELETE` affect 0, `INSERT` is refused; with the
  marker, all rows are visible. Pinned transactions are unchanged (own org only).
- `ship_once` on a pool whose connections run as `otto_app` ships and records rows (proves the
  shipper sets the marker).
- Existing e2e, `usage_logs`, and `db.rs` tests pass unchanged.

## 5. Scope

**In:** `ship_once` and `reschedule` in `crates/flags-core/src/usage.rs`; `maintenance::begin`'s
return type; new migration `0005_usage_outbox_maintenance.sql`; tests; the RLS paragraph and
rollback note in `docs/deploy/fly.md`; module docs in `maintenance.rs` and `usage.rs`.

**Out:** making background (maintenance) transactions `SET LOCAL ROLE otto_app` so the marker is
enforced in production too (a change to every sweep; separate decision); the claim lease length
(fencing makes it a liveness setting, not a safety one); `usage_outbox_rejected` retention; the
`Meter::charge` path (already pinned); the platform client's timeout.

## 6. Architecture

`crates/flags-core/src/maintenance.rs`: `begin` returns
`sqlx::Result<Transaction<'static, Postgres>>`. Its doc lists the usage shipper among its callers.

`crates/flags-core/src/usage.rs`, `ship_once`:

1. **Claim** (unchanged SQL, plus `attempts` in `RETURNING`) in a `maintenance::begin`
   transaction, committed before the platform call, so no connection is held across it. `Row`
   gains `attempts: i32`. The fence is two parallel arrays, `ids: Vec<i64>` and
   `attempts: Vec<i32>`, from the sorted rows.
2. **Platform call and receipt checks** unchanged (count check, rejected-ids subset check from #64).
   `refuse_receipt` and the platform-error path call the fenced `reschedule`.
3. **Finish** in one `maintenance::begin` transaction, one statement:

   ```sql
   WITH gone AS (
     DELETE FROM usage_outbox o
      USING unnest($1::int8[], $2::int4[]) AS c(id, attempts)
      WHERE o.id = c.id AND o.attempts = c.attempts
     RETURNING o.event_id, o.org_id, o.user_id, o.tool, o.billable, o.occurred_at),
   moved AS (
     INSERT INTO usage_outbox_rejected
       (event_id, org_id, user_id, tool, billable, occurred_at, reason)
     SELECT g.event_id, g.org_id, g.user_id, g.tool, g.billable, g.occurred_at, r.reason
       FROM gone g JOIN unnest($3::uuid[], $4::text[]) AS r(event_id, reason)
         ON r.event_id = g.event_id
     RETURNING 1)
   SELECT (SELECT count(*) FROM gone), (SELECT count(*) FROM moved)
   ```

   `$3`/`$4` are the receipt's rejected ids and their stored reasons
   (`telemetry::stored(reason, MAX_REJECTED_REASON)`). The per-event `error!`/`debug!` logs for
   rejected events keep their shape (fixed `reason = "rejected"` above `debug`, the platform's
   reason at `debug` only, #44) and no longer promise the move: "the platform refused a usage
   event; recording it in usage_outbox_rejected if this claim still holds it". Then, with the
   two counts converted to `usize` once:
   - `deleted < ids.len()`: `warn!` with counts only (`claimed`, `lost`): "usage rows this
     shipper claimed are no longer held by its claim (reclaimed after it lapsed, or purged); left
     as they are". Usually another shipper reclaimed them after this claim lapsed; the
     org.deleted purge removing an org's rows is the other way.
   - `moved < rejected.len()`: `warn!` with counts only (`rejected`, `recorded`, no platform
     text): "rejected usage events are no longer held by this claim; left as they are".
   The previous per-event `INSERT` loop and the separate `DELETE` are replaced by this statement.
   `maintenance::begin` adds one `SET LOCAL` round trip to each transaction (the claim goes from
   one autocommit statement to BEGIN, SET, UPDATE, COMMIT, including the empty poll every 5 s);
   the statement count per batch no longer grows with the number of rejections. Accepted: the
   helper is the single place the marker is set, and the extra round trips are cheap next to a
   5 s poll.

`reschedule(db, ids, attempts, reason, cfg)`: same backoff SQL, fenced with
`FROM unnest($1::int8[], $2::int4[]) AS c(id, attempts) WHERE o.id = c.id AND o.attempts =
c.attempts`, in a `maintenance::begin` transaction. Binds: `$1` ids, `$2` attempts, `$3` the
stored reason (`last_error`), `$4`/`$5` the base and maximum backoff in seconds. Failures (begin,
statement, commit) are logged and swallowed as today (the claim lease still holds the rows back),
with a `rows` count field.

`crates/flags-core/migrations/0005_usage_outbox_maintenance.sql` (Section 8).

**Public interface:** `ship_once`, `run`, `ShipperConfig`, `ShipError` signatures unchanged.
`maintenance::begin` is `pub(crate)`; its return type change is internal. No MCP, REST, or
config change.

## 7. Invariants impact

- **Request paths open transactions with `begin_live`; unpinned cross-org access only through
  `maintenance::begin`:** restored for the shipper (the violation #68 names).
- **Tenant isolation is RLS under `otto_app`, proven:** strengthened for the two usage tables; the
  boot-time proof (`verify_tenant_isolation`) is unaffected (permissive and restrictive policies
  both count as policies; RLS stays enabled and forced).
- **Append-only migrations; compatible with the previous release during rollout and after a
  rollback:** a new file `0005`; policy-only, additive. The previous release's pinned writes
  (`Meter::charge`, the org.deleted purge) satisfy it; its unpinned shipper is the named gap in
  Assumptions (none under production's exempt pool role).
- **Metering:** a usage event is recorded as rejected at most once and never deleted by a shipper
  that no longer holds it.
- **The database is small and shared:** the finish step is one data statement whatever the
  rejected count (plus the marker's `SET LOCAL`); no connection held across the platform call; the fence arrays are bounded by
  `ShipperConfig::batch`.
- **No SQL outside flags-core; runtime `sqlx::query` only:** holds (new SQL is in `usage.rs`).
- **Platform text stays out of logs above `debug` (#44)** and **database error text never reaches
  a tool response:** new logs carry counts only.

## 8. Migration

New file `crates/flags-core/migrations/0005_usage_outbox_maintenance.sql`:

```sql
SET LOCAL lock_timeout = '3s';

CREATE POLICY usage_outbox_maintenance ON usage_outbox
  AS RESTRICTIVE FOR ALL
  USING (current_org() IS NOT NULL OR current_setting('app.maintenance', true) = 'on')
  WITH CHECK (current_org() IS NOT NULL OR current_setting('app.maintenance', true) = 'on');
CREATE POLICY usage_outbox_rejected_maintenance ON usage_outbox_rejected
  AS RESTRICTIVE FOR ALL
  USING (current_org() IS NOT NULL OR current_setting('app.maintenance', true) = 'on')
  WITH CHECK (current_org() IS NOT NULL OR current_setting('app.maintenance', true) = 'on');
```

A restrictive policy ANDs with the baseline's permissive `*_tenant_isolation` policy: pinned
access is still `org_id = current_org()` (the restrictive policy is true when pinned); unpinned
access now also needs the marker. With a header comment in `0003`'s style explaining why (#68),
why restrictive, the lock timeout, and the rollout/rollback gap.

Rollout: `CREATE POLICY` takes an `ACCESS EXCLUSIVE` lock briefly; `lock_timeout` fails the
migration fast rather than queueing behind a tool transaction, crash-looping the new machine while
the previous release keeps serving (recovery per `docs/deploy/fly.md`). The previous release boots
against `0005` (`MIGRATOR` ignores unknown versions).

## 9. Out-of-band

- Migration `0005` applies at boot (agent: confirm `applying migrations` in the boot logs).
- No new secret, config key, scope, registration, or recurring cost. Nothing human-only.

## 10. Error handling and edge cases

- **All of a batch lost to a newer claim (or purged by org.deleted):** the finish statement deletes
  and moves nothing; `warn` with counts; `ship_once` returns `Ok(claimed)`. The newer holder ships and records them.
- **Partly lost:** rows still held are deleted (and recorded if rejected); the rest are left.
- **A row deleted by the newer holder before the lapsed shipper finishes:** the fence matches
  nothing; same as above.
- **Lapsed shipper's platform failure or bad receipt:** its fenced `reschedule` matches nothing, so
  the newer holder's schedule and `last_error` stand.
- **Concurrent final statements:** row locks serialize the two deletes; the later one re-checks
  the fence and skips the row, and it records only what it deleted.
- **`maintenance::begin` fails in `ship_once`:** `ShipError::Db`, as a pool error does today.

## 11. Testing approach

Gates: test, lint, format, extra (no modified migration), and the `crates/**` podman build.

- `crates/flags-core/tests/db.rs`, new `usage_outbox_tables_need_the_maintenance_marker_unpinned`
  (sibling of `tenant_isolation_is_enforced_on_every_tenant_table`): rows for two orgs in both
  tables (inserted on the superuser pool). As `otto_app` unpinned without the marker: `count(*)`
  is 0 on both tables, `UPDATE` and `DELETE` affect 0 rows, an `INSERT` fails with SQLSTATE
  `42501`. With the marker (`as_app_maintenance`): every row is visible. Pinned to org A
  (`db.begin(a)`): only A's rows, and inserting A's row succeeds (the `Meter::charge` path). Each
  expected failure in its own transaction.
- `crates/flags-server/tests/e2e.rs`:
  - The mock platform gains a usage gate: when set, each `/internal/usage` request sends a
    `oneshot::Sender<()>` to the test over an unbounded channel and waits for it before answering
    (the std mutex is not held across the wait).
  - New `a_lapsed_usage_claim_leaves_the_row_to_the_newer_one` (#67): one row; `reject_with` set;
    `ShipperConfig { claim_lease: 0, .. }`. Spawn A, wait for its request; spawn B, wait for its
    request. Release A: `Ok(1)`; the row is still in `usage_outbox` and `usage_outbox_rejected` is
    empty. Release B: `Ok(1)`; exactly one rejected row for the event; `usage_outbox` empty.
    Without fencing, A deletes and records the row (fails the first assertion).
  - New `a_lapsed_claim_does_not_reschedule_the_newer_one` (#67): the mock gains `fail_usage`
    (every batch answered 502). One row, lease 0; A and B both claim it and wait at the gate.
    With `fail_usage` set, A is released and fails (`http_status`); the row keeps `attempts = 2`,
    `last_error IS NULL`, and the `next_attempt_at` B's claim set. Then B is released with
    `fail_usage` cleared: `Ok(1)`, both tables empty. Without the fence on `reschedule`, A
    overwrites B's schedule and `last_error`.
  - New `a_lapsed_claim_is_fenced_row_by_row` (#67): two rows, the first inserted with
    `attempts = 5` (so the claim pairs 6 with it and 1 with the second), both rejected. A claims
    both; the test then bumps only the second row's `attempts`, as a newer claim would. Releasing
    A returns `Ok(2)`: the first row moves to `usage_outbox_rejected`, the second stays in
    `usage_outbox`. Proves the ids and attempts arrays pair row by row.
  - New `the_usage_shipper_sets_the_maintenance_marker` (#68): a second pool on the test database
    whose connections run `SET ROLE otto_app` after connect (built in the test itself; the
    `db.rs` helpers live in another test binary). An unmarked unpinned `count(*)` on
    that pool is 0 (the role is subject to RLS). Two rows, one rejected by the mock via a fixed
    receipt (`accepted: 1, rejected: [second]`): `ship_once` on a `Db` from that pool returns 2;
    `usage_outbox` is empty and `usage_outbox_rejected` holds the one row (read on the superuser
    pool).
- Existing `a_rejected_usage_event_keeps_a_capped_reason`,
  `a_receipt_naming_events_outside_the_batch_moves_nothing`, the shipper path in e2e, and
  `usage_logs.rs` keep passing unchanged (log shape for rejected events unchanged).

## 12. Risks and open questions

- **The marker is not enforced in production** while background transactions run as the exempt
  pool role (Premise corrections). Same standing as `0003`; noted, not widened here.
- **Lock timeout during deploy:** a busy tool transaction could hold `usage_outbox` past 3 s and
  fail the migration; the deploy then fails visibly and is re-run (Rob's call), as for `0003`.
- **Test timing:** the overlap test relies on lease 0 making a just-claimed row due again; the
  claim's `now()` is the transaction start, and B's transaction starts after A's commit, so B's
  `now()` is never earlier. Deterministic.
