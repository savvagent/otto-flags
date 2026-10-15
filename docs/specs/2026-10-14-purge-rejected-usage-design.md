# Purge and sweep rejected usage events

**Status:** DRAFT
**Issue:** #74

## 1. Brief

From #74 (found by the security and architecture reviews of #72):

> The `org.deleted` purge (`crates/flags-core/src/platform_events.rs`, the per-table delete list)
> clears `usage_outbox` but not `usage_outbox_rejected`, and nothing sweeps
> `usage_outbox_rejected` by age. A deleted org's rejected usage events, including `user_id`s, are
> kept indefinitely, and the table is unbounded, against the "no unbounded tables" invariant.
>
> **Acceptance criteria**
> - [ ] The `org.deleted` purge also deletes the org's `usage_outbox_rejected` rows (pinned, like
>   the others), and its `deleted` detail reports the count.
> - [ ] `usage_outbox_rejected` gets a retention bound (a sweep inside `maintenance::begin`, with a
>   named retention period), covered by a test.

## 2. Premise corrections

None. The brief matches the code: `org_deleted` (`platform_events.rs:174`) deletes from a fixed
list that ends with `usage_outbox`, and no sweep names `usage_outbox_rejected`.

A note on what already holds: no migration is needed. `usage_outbox_rejected` already has the
baseline policy `usage_outbox_rejected_tenant_isolation` (`org_id = current_org() OR current_org()
IS NULL`, FOR ALL) and `0005`'s restrictive `usage_outbox_rejected_maintenance` (pinned, or the
maintenance marker). A pinned purge DELETE and a marked unpinned sweep DELETE (as `otto_app`, since
#73) both pass, and `otto_app` has DELETE through the baseline grant.

## 3. Assumptions

- **Retention period: 90 days** (`usage::REJECTED_RETENTION_DAYS`). Rejected events are kept for
  operators to diagnose a billing disagreement with the platform (#44); 90 days covers a
  quarter of monthly billing periods and matches the evaluation-rollup and rollback-event
  retention (`telemetry::EVAL_RETENTION_DAYS`). A separate constant, because the reasons differ.
- **The sweep lives in `usage`** (`usage::sweep_rejected`), beside the shipper that writes the
  table, and is called from `flags-server`'s hourly `sweep_loop`, next to the platform-event and
  telemetry sweeps. `telemetry::sweep` stays about telemetry.
- **No new index.** The sweep's `WHERE rejected_at < …` cannot use
  `usage_outbox_rejected_org_idx (org_id, rejected_at DESC)` and scans the table hourly. Rows
  appear only when the platform refuses an event outright, so the table is small, and the sweep
  now bounds it. An index would be a migration for a scan of a handful of rows.
- **Purge order:** `usage_outbox_rejected` goes after `usage_outbox` in the delete list. Neither
  references the other.
- Run Profile: every value comes from `CLAUDE.md` > Development Workflow; nothing assumed.

## 4. Goal & success criteria

A deleted org leaves no rejected usage behind, and `usage_outbox_rejected` is bounded by age.

- `org.deleted` deletes the org's `usage_outbox_rejected` rows and reports
  `detail["usage_outbox_rejected"]` with the count; another org's rows stay.
- `usage::sweep_rejected` deletes rows older than `REJECTED_RETENTION_DAYS` across every org,
  keeps newer ones, and returns the count, on the default (superuser) test pool, where
  maintenance transactions run as `otto_app` (#73).
- `flags-server`'s hourly sweep loop calls it and logs failures with the same text-free reason
  fields as the other sweeps.

## 5. Scope

In: `crates/flags-core/src/platform_events.rs` (purge list), `crates/flags-core/src/usage.rs`
(constant, sweep, module doc), `crates/flags-server/src/main.rs` (`sweep_loop`), tests in
`crates/flags-core/tests/db.rs`.

Out: an index or any migration; retention of `usage_outbox` itself (rows leave as they ship);
`audit_events` retention.

## 6. Architecture

`platform_events.rs`, `org_deleted`: add `"usage_outbox_rejected"` after `"usage_outbox"` in the
delete list. The loop already deletes `WHERE org_id = $1` on the pinned transaction and records
each table's `rows_affected` in `deleted`, which becomes `Outcome::Applied { detail }`.

`usage.rs`:

```rust
/// How long `usage_outbox_rejected` keeps a refused event for operators.
pub const REJECTED_RETENTION_DAYS: i32 = 90;

/// Delete rejected usage events older than [`REJECTED_RETENTION_DAYS`], across
/// every org, in a maintenance transaction. Returns how many went.
pub async fn sweep_rejected(db: &Db) -> crate::error::Result<u64> {
    let mut tx = crate::maintenance::begin(db).await?;
    let n = sqlx::query(
        "DELETE FROM usage_outbox_rejected \
         WHERE rejected_at < now() - make_interval(days => $1)",
    )
    .bind(REJECTED_RETENTION_DAYS)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(n)
}
```

`crates/flags-server/src/main.rs`, `sweep_loop`: a third `match` like the telemetry one, logging
`swept expired rejected usage events` on success and `rejected-usage sweep failed` with
`code`, `reason`, `status`, `sqlstate`, `transport` at warn (error text at debug only), as the
other two do. The loop's doc comment names it.

Public interface: `usage::REJECTED_RETENTION_DAYS` and `usage::sweep_rejected` are new public
items of `flags-core` (additive, crate-internal to this workspace). The `org.deleted` webhook's
`detail` gains a key (additive; the platform does not read it).

## 7. Invariants impact

- **No unbounded tables:** restored for `usage_outbox_rejected`.
- **Tenant isolation, unpinned access only through `maintenance::begin`:** the sweep uses it; the
  purge is pinned, as before.
- **Platform webhooks apply idempotently:** unchanged; a redelivery is still `Duplicate` and
  deletes nothing more.
- **No SQL outside flags-core, runtime queries only:** holds.
- **Small shared database:** one hourly DELETE over a small table; no connection held.
- **Migrations:** none.
- **Rollout/rollback:** the previous release neither purges nor sweeps the table; running
  against the same schema it simply leaves rows, as today.

## 8. Migration

None.

## 9. Out-of-band

None: no secret, config key, scope, or registration. The first sweep after deploy deletes
whatever rejected rows production holds older than 90 days; that is the intended retention.

## 10. Error handling & edge cases

- A sweep failure is logged and retried next hour (the loop continues), as for the other sweeps.
- `org.deleted` failing on this table fails the whole purge transaction, which rolls back and is
  retried by the platform, as for any other table in the list.
- Rows with `rejected_at` exactly at the cutoff stay (strict `<`), as the other sweeps.

## 11. Testing approach

Gates: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`,
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`, the
migrations extra gate, `podman build -t otto-flags:check .`.

- `crates/flags-core/tests/db.rs`, `org_deleted_purges_everything_and_revokes_keys`: insert one
  `usage_outbox_rejected` row for each org before the purge; assert
  `detail["usage_outbox_rejected"] == 1`, the org's rows are gone, and the other org's row stays.
- `crates/flags-core/tests/db.rs`, new `rejected_usage_is_swept_after_its_retention`: on the
  default pool, two orgs, one row each at `REJECTED_RETENTION_DAYS + 1` days old and one fresh;
  `sweep_rejected` returns 2 and each org keeps its fresh row.

## 12. Risks & open questions

- 90 days is a judgment call; changing it later is a one-line constant change.
