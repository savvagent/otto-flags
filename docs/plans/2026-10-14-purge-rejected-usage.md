# Purge and sweep rejected usage Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [x]`) syntax for tracking.

**Goal:** `org.deleted` also purges the org's `usage_outbox_rejected` rows, and an hourly sweep
bounds that table at `usage::REJECTED_RETENTION_DAYS` (90).

**Architecture:** One more table in `org_deleted`'s pinned delete list; a new
`usage::sweep_rejected` in a maintenance transaction, called from `flags-server`'s `sweep_loop`.
No migration: the baseline and `0005` policies already allow both deletes.

**Tech Stack:** Rust (stable, MSRV 1.88), sqlx 0.8, Postgres 16.

**Ticket:** savvagent/otto-flags#74

**Spec:** `docs/specs/2026-10-14-purge-rejected-usage-design.md`

## Global Constraints

- Worktree only: `/home/robhicks/dev/otto-flags/.claude/worktrees/fix-74-purge-rejected-usage`;
  `main` changes only through the PR.
- Gates, from the worktree root (set `TMPDIR=$PWD/target/tmp` after `mkdir -p target/tmp`: the
  host's `/tmp` tmpfs is nearly full and doc-test linking crashes there):
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check` (run `cargo fmt --all` first)
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'`
    prints nothing
  - `podman build -t otto-flags:check .` (crates changed; once, before the PR)
- Evidence: report pass counts per test binary. `cargo test` without `DATABASE_URL` panics
  ("DATABASE_URL must be set"): a missing variable, not a test failure. `DATABASE_URL` points only
  at the local Postgres on 15434. Skipped or ignored tests are not a pass. Do not run DB tests
  concurrently with another worktree's.
- Invariants that apply: no unbounded tables; unpinned cross-org access only through
  `maintenance::begin`; the purge stays pinned; platform webhooks apply idempotently; no SQL
  outside flags-core, runtime queries only; small shared database; no migration.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #74`. No attribution of any kind (no Co-Authored-By, no "Generated with", no credit
  markers).

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `crates/flags-core/src/platform_events.rs` | `org_deleted` delete list gains `usage_outbox_rejected` |
| **Modify.** `crates/flags-core/src/usage.rs` | `REJECTED_RETENTION_DAYS`, `sweep_rejected`, module doc line |
| **Modify.** `crates/flags-server/src/main.rs` | `sweep_loop` calls `sweep_rejected`; doc comment |
| **Modify.** `crates/flags-core/tests/db.rs` | Purge test extended; new sweep test |

## After review (PR #81)

The purge comment states why `usage_outbox_rejected` must stay after `usage_outbox` (read committed,
a later statement); the sweep test adds a row a day inside the window, which fails if the cutoff or
its unit changes.

## Task Order & Rationale

Task 1 (purge) and Task 2 (sweep) are independent; the purge is the PII half of the ticket, so it
goes first.

---

### Task 1: `org.deleted` purges rejected usage

**Files:**
- Modify: `crates/flags-core/src/platform_events.rs` (`org_deleted`, the `for table in [...]` list)
- Test: `crates/flags-core/tests/db.rs` (`org_deleted_purges_everything_and_revokes_keys`)

**Out-of-band / invariants:** none out-of-band. The delete stays on the pinned `tx` (guard 1: the
`WHERE org_id = $1`; guard 2: the pinned policy). Idempotent redelivery unchanged.

**Model:** sonnet.

- [x] **Step 1: Extend the test.** In `org_deleted_purges_everything_and_revokes_keys`, before the
  webhook `body` is built (after the rollback-event `tx.commit()`), insert one rejected row per
  org on the superuser test pool:

```rust
    // Each org has a rejected usage event; the purge takes only the org's own.
    for o in [org, other] {
        sqlx::query(
            "INSERT INTO usage_outbox_rejected (event_id, org_id, user_id, tool, billable, occurred_at, reason) \
             VALUES (gen_random_uuid(), $1, gen_random_uuid(), 'create_flag', true, now(), 'x')",
        )
        .bind(o)
        .execute(db.pool())
        .await
        .unwrap();
    }
```

  After `assert_eq!(detail["rollback_events"], 1, "{detail}");` add
  `assert_eq!(detail["usage_outbox_rejected"], 1, "{detail}");`. Change the emptiness loop
  `for table in ["rollback_policies", "rollback_events"]` to
  `for table in ["rollback_policies", "rollback_events", "usage_outbox_rejected"]`. After that loop
  add:

```rust
    let others: i64 =
        sqlx::query_scalar("SELECT count(*) FROM usage_outbox_rejected WHERE org_id = $1")
            .bind(other)
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(others, 1, "another org's rejected usage stays");
```

- [x] **Step 2: Run it; it fails.**
  `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --test db org_deleted_purges_everything_and_revokes_keys`
  Expected: FAIL (`detail["usage_outbox_rejected"]` is null).

- [x] **Step 3: Implement.** In `crates/flags-core/src/platform_events.rs`, `org_deleted`, add
  `"usage_outbox_rejected",` after `"usage_outbox",` in the delete list, and extend the comment
  above the list:

```rust
    // Children before parents. Every statement names the org (guard 1) in
    // addition to the pinned policy (guard 2); app_keys has no policy and
    // relies on the predicate alone. usage_outbox_rejected holds user ids
    // (#74); the shipper cannot re-create a purged org's rows, since it only
    // records a rejection for an outbox row its own fenced DELETE removed.
```

- [x] **Step 4: Run it; it passes.** Same command: 1 passed, not skipped. Then the full gates.

- [x] **Step 5: Commit.**

```bash
git add crates/flags-core/src/platform_events.rs crates/flags-core/tests/db.rs
git commit -m "fix(core): purge a deleted org's rejected usage events

The org.deleted purge cleared usage_outbox but left the org's rejected
usage events, user ids included, forever.

Fixes #74"
```

---

### Task 2: Rejected usage is swept after 90 days

**Files:**
- Modify: `crates/flags-core/src/usage.rs` (new constant and function after `ship_once`'s helpers;
  module doc)
- Modify: `crates/flags-server/src/main.rs` (`sweep_loop`, around lines 152–190)
- Test: `crates/flags-core/tests/db.rs` (new test after `sweeps_reach_every_org_on_the_superuser_pool`)

**Interfaces:** produces `flags_core::usage::REJECTED_RETENTION_DAYS: i32` and
`flags_core::usage::sweep_rejected(&Db) -> flags_core::error::Result<u64>` (new public items,
additive).

**Out-of-band / invariants:** none out-of-band. Unpinned cross-org access through
`crate::maintenance::begin` only; error text only at debug in the server log.

**Model:** sonnet.

- [x] **Step 1: Write the failing test** in `crates/flags-core/tests/db.rs` (add
  `usage` to the `flags_core::{...}` imports if it is not already imported; check the file's
  `use` lines):

```rust
/// Rejected usage events are kept for operators for
/// `usage::REJECTED_RETENTION_DAYS`, then swept across every org (#74), as
/// `otto_app` in a maintenance transaction (#73) even on this superuser pool.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn rejected_usage_is_swept_after_its_retention(pool: PgPool) {
    let db = db(pool.clone()).await;
    let orgs = [OrgId::new(), OrgId::new()];
    for org in orgs {
        for days in [usage::REJECTED_RETENTION_DAYS + 1, 1] {
            sqlx::query(
                "INSERT INTO usage_outbox_rejected \
                 (event_id, org_id, user_id, tool, billable, occurred_at, reason, rejected_at) \
                 VALUES (gen_random_uuid(), $1, gen_random_uuid(), 'create_flag', true, now(), 'x', \
                         now() - make_interval(days => $2))",
            )
            .bind(org)
            .bind(days)
            .execute(&pool)
            .await
            .unwrap();
        }
    }

    assert_eq!(usage::sweep_rejected(&db).await.unwrap(), 2);
    for org in orgs {
        let (total, expired): (i64, i64) = sqlx::query_as(
            "SELECT count(*), count(*) FILTER (WHERE rejected_at < now() - make_interval(days => $2)) \
             FROM usage_outbox_rejected WHERE org_id = $1",
        )
        .bind(org)
        .bind(usage::REJECTED_RETENTION_DAYS)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!((total, expired), (1, 0), "each org keeps its fresh row");
    }
}
```

- [x] **Step 2: Run it; it fails to compile** (`sweep_rejected` and `REJECTED_RETENTION_DAYS` do
  not exist):
  `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --test db rejected_usage_is_swept_after_its_retention`

- [x] **Step 3: Implement in `crates/flags-core/src/usage.rs`.** Near the top-level constants
  (after `WARN_AT`):

```rust
/// How long `usage_outbox_rejected` keeps an event the platform refused, for
/// operators reconciling usage with it (#44); [`sweep_rejected`] removes older
/// ones (#74).
pub const REJECTED_RETENTION_DAYS: i32 = 90;
```

  After `ship_once` and its helpers (before `stored_reason`'s doc comment is fine; keep it a
  top-level `pub async fn`):

```rust
/// Delete rejected usage events older than [`REJECTED_RETENTION_DAYS`],
/// across every org, in a maintenance transaction ([`crate::maintenance`]),
/// which the table's unpinned policies require. Returns how many went.
pub async fn sweep_rejected(db: &Db) -> Result<u64> {
    let mut tx = crate::maintenance::begin(db).await?;
    let swept = sqlx::query(
        "DELETE FROM usage_outbox_rejected \
         WHERE rejected_at < now() - make_interval(days => $1)",
    )
    .bind(REJECTED_RETENTION_DAYS)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(swept)
}
```

  If `Result` in `usage.rs` is not `crate::error::Result`, write `crate::error::Result<u64>`.
  In the module doc, after the "Shipping is a background task" bullet, add:

```rust
//! - **Refused events are kept, then swept.** An event the platform rejects
//!   outright moves to `usage_outbox_rejected` with its reason, is kept
//!   [`REJECTED_RETENTION_DAYS`], and goes with its org on `org.deleted`.
```

- [x] **Step 4: Run the test; it passes.** Same command: 1 passed, not skipped.

- [x] **Step 5: Call it from the server.** In `crates/flags-server/src/main.rs`, `sweep_loop`, after
  the `flags_core::telemetry::sweep` `match`, add (same shape as that one):

```rust
        match flags_core::usage::sweep_rejected(&db).await {
            Ok(0) => {}
            Ok(n) => tracing::info!(rows = n, "swept expired rejected usage events"),
            Err(e) => {
                let f = e.failure_reason();
                tracing::warn!(
                    code = e.code(),
                    reason = f.reason,
                    status = f.status,
                    sqlstate = f.sqlstate(),
                    transport = f.transport,
                    "rejected-usage sweep failed"
                );
                tracing::debug!(code = e.code(), reason = f.reason, status = f.status, sqlstate = f.sqlstate(), transport = f.transport, error = %e, "rejected-usage sweep failed");
            }
        }
```

  and change the doc comment above `sweep_loop` from "Hourly retention: platform-event markers,
  removed-member tombstones, error reports, and old evaluation buckets." to "Hourly retention:
  platform-event markers, removed-member tombstones, error reports, old evaluation buckets, and
  rejected usage events."

- [x] **Step 6: Gates.** `cargo fmt --all`, then test, lint, format, extra, and
  `podman build -t otto-flags:check .` as in Global Constraints.

- [x] **Step 7: Commit.**

```bash
git add crates/flags-core/src/usage.rs crates/flags-server/src/main.rs crates/flags-core/tests/db.rs
git commit -m "fix(core): sweep rejected usage events after 90 days

Nothing bounded usage_outbox_rejected, against the no-unbounded-tables
invariant. Sweep it hourly with the other retention sweeps, keeping
refused events 90 days for operators.

Fixes #74"
```
