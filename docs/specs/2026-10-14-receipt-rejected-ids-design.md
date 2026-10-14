# Check the rejected event ids in the platform's usage receipt

**Status:** DRAFT
**Issue:** #63

## 1. Brief

From #63 (found by the security review of #62 (#51); the behaviour predates it):

> `ship_once` (`crates/flags-core/src/usage.rs`) checks that the platform's receipt accounts for as
> many events as were sent (`accepted + duplicates + rejected.len()`), but not which ones. Each
> rejected `event_id` is then copied with `INSERT INTO usage_outbox_rejected … SELECT … FROM
> usage_outbox WHERE event_id = $1`, which looks across the whole outbox, not just this batch:
>
> - A rejected `event_id` that is not in the batch (a row another shipper has claimed, or one not
>   yet due) is copied into `usage_outbox_rejected`, but the `DELETE` removes only this batch's
>   ids, so that row is later shipped too: recorded both as rejected and as delivered.
> - An `event_id` repeated in `rejected` creates duplicate `usage_outbox_rejected` rows (`event_id`
>   is not unique there), while the count check can still pass by offsetting `accepted`.
>
> Only the authenticated platform can send such a receipt, so this bounds the damage a misbehaving
> platform can do rather than closing an exposure.
>
> **Acceptance criteria**
> - [ ] A receipt whose rejected ids are not a duplicate-free subset of the batch's event ids is
>   treated like a miscounted receipt: nothing is moved or deleted, the rows are rescheduled, and
>   `ship_once` returns the `decode` failure.
> - [ ] The insert into `usage_outbox_rejected` is limited to the batch's rows (e.g.
>   `AND id = ANY($ids)`).
> - [ ] A test covers a receipt naming an event outside the batch and one repeating an event.

## 2. Premise corrections

None. The code at `crates/flags-core/src/usage.rs:326-358` matches the brief.

## 3. Assumptions

- **Validate before the transaction opens**, beside the count check, so a bad receipt costs no
  pooled connection and shares the miscount path (log, `reschedule` with `decode`, return
  `ShipError::Platform(Error::Decode)`).
- **The error log carries no platform-supplied id.** The message names only counts ("the platform
  rejected N events, M of them not in this batch or repeated"), like the miscount message. Event
  ids are UUIDs, not free text, but nothing needs them at `error`.
- **Keep the `AND id = ANY($3)` guard as well**, though validation makes it redundant: the insert
  then cannot reach outside the batch even if the check above it is later changed (defence in
  depth the issue asks for).
- Every Run Profile value was read from the Development Workflow section; none assumed.

## 4. Goal and success criteria

A receipt can only move rows of the batch it answers, each at most once.

- A receipt naming an event outside the batch: `ship_once` returns `decode`; both the batch row and
  the named row stay in `usage_outbox`; the batch row's `last_error` is `decode`;
  `usage_outbox_rejected` is empty.
- A receipt repeating a batch event (count still matching): same outcome for the batch's rows.
- A valid receipt behaves exactly as before (existing e2e and `usage_logs` tests pass unchanged).

## 5. Scope

**In:** `ship_once`'s receipt validation and its insert into `usage_outbox_rejected`.

**Out:** checking `accepted`/`duplicates` individually (the platform does not name those ids);
existing `usage_outbox_rejected` rows; any schema change (no unique constraint on
`usage_outbox_rejected.event_id`: a new constraint is a migration the previous release could trip).

## 6. Architecture

`crates/flags-core/src/usage.rs`, `ship_once`:

- After the count check, build a `HashSet<Uuid>` of the batch's `event_id`s. Walk
  `receipt.rejected`, removing each id from the set; an id that is not there (never in the batch,
  or already removed, i.e. repeated) counts as bad. If any is bad, log at `error` with counts only,
  `reschedule(db, &ids, "decode", cfg)` via `stored_reason(&Error::Decode(..))`, and return the
  `Decode` error, exactly as the miscount path does. The two checks share one small helper that
  builds the error, logs, and reschedules, so the paths cannot drift.
- The insert gains `AND id = ANY($3)` bound to `&ids`.

No public interface change; `ShipError` and the receipt types are unchanged.

## 7. Invariants impact

- **Metering:** strengthened; a usage event can no longer be recorded both as rejected and as
  delivered.
- **The database is small and shared:** unchanged query count per batch; the set is bounded by
  `MAX_USAGE_BATCH`.
- **Append-only migrations / rollout compatibility:** no migration.
- **No SQL outside flags-core / runtime `sqlx::query` only:** holds.
- Platform text stays out of logs above `debug` (#44): the new message carries counts only.

## 8. Migration

None.

## 9. Out-of-band

None.

## 10. Error handling and edge cases

- Empty `rejected`: passes trivially.
- A receipt that both miscounts and names bad ids: the count check fires first; same outcome.
- A bad receipt repeated on every retry: the rows back off (as for a miscount) and stay in the
  outbox; nothing is lost or double-counted. Same behaviour a persistent miscount already has.

## 11. Testing approach

e2e (`crates/flags-server/tests/e2e.rs`, its mock platform): the mock `Platform` gains
`receipt: Option<UsageReceipt>`; when set, `ship` answers it verbatim (checked before
`reject_with`). New test `a_receipt_naming_events_outside_the_batch_moves_nothing`:

1. Insert row A (due) and row B with `next_attempt_at = now() + interval '1 hour'` (not due).
   Receipt `{accepted: 0, duplicates: 0, rejected: [B]}`. `ship_once` errors with
   `failure_reason().reason == "decode"`; `usage_outbox` still has 2 rows; A's `last_error` is
   `decode`; `usage_outbox_rejected` is empty.
2. Make both rows due (batch A, B). Receipt `{accepted: 0, duplicates: 0, rejected: [A, A]}`
   (count 2 matches the batch). `ship_once` errors with `decode`; 2 rows remain;
   `usage_outbox_rejected` is empty.
3. Control: receipt `{accepted: 1, duplicates: 0, rejected: [A]}` with both due: `ship_once`
   returns 2; `usage_outbox_rejected` holds exactly one row, for A; `usage_outbox` is empty.

Gates: test, lint, format, extra, and the `crates/**` podman build.

## 12. Risks and open questions

- None material. A platform that legitimately repeats an id would now stall its batch with backoff;
  the platform contract says every event is in exactly one outcome, so it never should.
