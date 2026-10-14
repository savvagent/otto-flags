# Usage Shipper Fencing Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** The usage shipper acts only on rows it still holds (fenced on the `attempts` value its
claim set), runs every outbox statement inside `maintenance::begin`, and a new migration makes
unpinned access to `usage_outbox` and `usage_outbox_rejected` require the maintenance marker.

**Architecture:** A restrictive `FOR ALL` policy per table (migration `0005`) ANDs the marker
requirement onto the baseline's permissive tenant policy. `ship_once` claims in a maintenance
transaction (returning `attempts`), ships, then finishes with one fenced
`DELETE … RETURNING` CTE that feeds the `INSERT INTO usage_outbox_rejected`; `reschedule` is fenced
the same way. `maintenance::begin` returns `sqlx::Result`.

**Tech Stack:** Rust (stable, MSRV 1.88), sqlx 0.8 (runtime queries), Postgres 16, axum (mock
platform in tests), tokio.

**Ticket:** savvagent/otto-flags#67 and savvagent/otto-flags#68 (one PR)

**Spec:** `docs/specs/2026-10-14-usage-shipper-fencing-design.md`

## Global Constraints

- Worktree only: `/home/robhicks/dev/otto-flags/.claude/worktrees/fix-67-usage-shipper-fencing`
  (branch `fix/67-usage-shipper-fencing`); `main` changes only through the PR.
- Gates, from the worktree root:
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'`
    must print nothing
  - when `crates/**` (final run only): `podman build -t otto-flags:check .`
- Evidence: report pass counts per test binary. A `DATABASE_URL must be set` panic is a missing
  variable, not a test failure or a pass. `DATABASE_URL` only ever points at the local Postgres on
  15434, never at a Fly database. No other worktree runs DB tests against 15434 during this work.
- Invariants that apply: append-only migrations (new `0005` only; never touch `0001`–`0004`);
  migration additive and compatible with the previous release (gap named in the spec); tenant
  isolation is RLS under `otto_app`; unpinned cross-org access only through
  `flags_core::maintenance::begin`; no SQL outside flags-core except test code, runtime
  `sqlx::query`/`query_as`/`query_scalar` only (no `query!` macros); metering (an event recorded as
  rejected at most once); small shared database (no connection held across the platform call, no
  per-row statements added); platform text stays out of logs above `debug` (#44).
- Out-of-band: migration `0005` applies at boot (verified post-deploy from the logs). Nothing
  human-only.
- Run `cargo fmt --all` before the format check: some code in this plan is pasted wider than
  rustfmt's line width.
- After Task 3 the orchestrator runs every gate once more on the final head, including
  `podman build -t otto-flags:check .`, and before merge adds the design-doc close-out commit (spec
  `**Status:**` -> `IMPLEMENTED (#<PR>)`, plan boxes ticked).
- Commit format: `<type>(<scope>): <imperative, lower-case>`; body explains why and ends with
  `Fixes #67` and/or `Fixes #68` as the task relates. No attribution of any kind (no
  Co-Authored-By, no "Generated with", no credit markers).

## File Structure

| File | Responsibility |
|---|---|
| **Create.** `crates/flags-core/migrations/0005_usage_outbox_maintenance.sql` | restrictive marker policies on both usage tables |
| **Modify.** `crates/flags-core/tests/db.rs` | prove the marker policy under `otto_app` |
| **Modify.** `crates/flags-core/src/maintenance.rs` | `begin` returns `sqlx::Result`; doc names the shipper |
| **Modify.** `crates/flags-core/src/usage.rs` | claim/finish/reschedule in maintenance transactions, fenced on `(id, attempts)` |
| **Modify.** `crates/flags-server/tests/e2e.rs` | mock usage gate; overlap test; `otto_app` shipper test |
| **Modify.** `docs/deploy/fly.md` | RLS/marker paragraph and rollback note cover `0005` |

## Task Order & Rationale

Task 1 lands the policy and its proof first: it makes the current shipper fail under a non-exempt
role, which Task 2's `otto_app` test then demonstrates and fixes. Task 2 is the shipper change with
both e2e tests. Task 3 updates the operations doc once the behaviour exists.

---

### Task 1: Require the maintenance marker for unpinned access to the usage outbox

**Files:**
- Create: `crates/flags-core/migrations/0005_usage_outbox_maintenance.sql`
- Test: `crates/flags-core/tests/db.rs`

**Interfaces:** produces the policies Task 2's `otto_app` test relies on.

**Out-of-band / invariants:** new migration only (append-only; additive; previous release's pinned
writes still pass). The contiguity test in `crates/flags-core/src/migrate.rs` must still pass.

**Model:** sonnet — SQL and a test fully specified here.

- [ ] **Step 1: Write the failing test** in `crates/flags-core/tests/db.rs`, directly after
  `tenant_isolation_is_enforced_on_every_tenant_table` (reuse the file's `as_app_unpinned`,
  `as_app_maintenance`, `sqlstate`, and `db` helpers):

```rust
/// The usage outbox is drained by the background shipper across every org, so
/// its baseline policy lets an unpinned transaction through. Unpinned alone is
/// not enough (#68, as #27 for the retention tables): a request path that
/// forgot to pin is unpinned too. Without the maintenance marker an unpinned
/// `otto_app` transaction sees, changes, and adds nothing; with it, it sees
/// every org's rows; pinned transactions still see only their own org.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn usage_outbox_tables_need_the_maintenance_marker_unpinned(pool: PgPool) {
    let (a, b) = (OrgId::new(), OrgId::new());
    for org in [a, b] {
        sqlx::query("INSERT INTO usage_outbox (org_id, tool, billable) VALUES ($1, 'create_flag', true)")
            .bind(org)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO usage_outbox_rejected (event_id, org_id, tool, billable, occurred_at, reason) \
             VALUES (gen_random_uuid(), $1, 'create_flag', true, now(), 'x')",
        )
        .bind(org)
        .execute(&pool)
        .await
        .unwrap();
    }

    for table in ["usage_outbox", "usage_outbox_rejected"] {
        // Unpinned, no marker: nothing visible, nothing changed.
        let mut tx = as_app_unpinned(&pool).await;
        let seen: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(seen, 0, "{table}: unpinned read without the marker");
        let updated = sqlx::query(&format!("UPDATE {table} SET tool = 'x'"))
            .execute(&mut *tx)
            .await
            .unwrap()
            .rows_affected();
        assert_eq!(updated, 0, "{table}: unpinned update without the marker");
        let deleted = sqlx::query(&format!("DELETE FROM {table}"))
            .execute(&mut *tx)
            .await
            .unwrap()
            .rows_affected();
        assert_eq!(deleted, 0, "{table}: unpinned delete without the marker");
        tx.rollback().await.unwrap();

        // Unpinned, no marker: an insert is refused.
        let mut tx = as_app_unpinned(&pool).await;
        let insert = if table == "usage_outbox" {
            "INSERT INTO usage_outbox (org_id, tool, billable) VALUES ($1, 'create_flag', true)"
        } else {
            "INSERT INTO usage_outbox_rejected (event_id, org_id, tool, billable, occurred_at, reason) \
             VALUES (gen_random_uuid(), $1, 'create_flag', true, now(), 'x')"
        };
        let err = sqlx::query(insert)
            .bind(a)
            .execute(&mut *tx)
            .await
            .unwrap_err();
        assert_eq!(sqlstate(&err), "42501", "{table}: unpinned insert without the marker");
        drop(tx);

        // Unpinned with the marker: every org's rows.
        let mut tx = as_app_maintenance(&pool).await;
        let seen: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(seen, 2, "{table}: maintenance read");
        tx.rollback().await.unwrap();

        // Pinned: only the org's own rows, and its own insert succeeds.
        let db = db(pool.clone()).await;
        let mut tx = db.begin(a).await.unwrap();
        let seen: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(tx.conn())
            .await
            .unwrap();
        assert_eq!(seen, 1, "{table}: pinned read");
        sqlx::query(insert).bind(a).execute(tx.conn()).await.unwrap();
        tx.rollback().await.unwrap();
    }
}
```

  Adapt only what the compiler requires (for example the `rollback` method name on `otto_tenant::Tx`
  or a missing import); keep every assertion.

- [ ] **Step 2: Run it and see it fail**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --test db usage_outbox_tables_need_the_maintenance_marker_unpinned`
Expected: FAIL on `usage_outbox: unpinned read without the marker` (left: 2, right: 0).

- [ ] **Step 3: Create the migration**
  `crates/flags-core/migrations/0005_usage_outbox_maintenance.sql`:

```sql
-- Unpinned access to the usage outbox needs the maintenance marker (#68).
--
-- 0001 gives usage_outbox and usage_outbox_rejected a permissive policy that
-- lets any unpinned transaction (current_org() IS NULL) read and write every
-- org's rows, for the background shipper that drains them. Unpinned is not
-- enough on its own (#27): a request path that forgot to pin is unpinned too.
-- As 0003 did for the retention tables, unpinned access now also needs the
-- transaction-local marker app.maintenance = 'on', which only flags-core's
-- maintenance::begin sets; the shipper runs inside it.
--
-- RESTRICTIVE and FOR ALL: it ANDs with the baseline's permissive policy for
-- every command, so a pinned transaction (current_org() set) is unaffected and
-- still reaches only its own org, while an unpinned one needs the marker to
-- read, change, delete, or insert.
--
-- Policy-only and additive. The previous release writes these tables pinned
-- (tool metering, the org.deleted purge), which still passes. Its shipper runs
-- unpinned without the marker: under an RLS-exempt pool role, as production's
-- superuser attach role is, RLS does not apply and it keeps shipping; under a
-- non-exempt pool role it would see no rows during the rollout overlap and
-- after a rollback, so usage waits in the outbox (nothing is lost) until a
-- release with the marker ships it.

-- CREATE POLICY takes a lock that conflicts with every metered tool call's
-- insert: fail fast rather than queue behind a long transaction. sqlx runs
-- each migration in its own transaction, so SET LOCAL ends with it.
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

- [ ] **Step 4: Run it and see it pass**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --test db`
Expected: every `db` test passes (including the new one and
`tenant_isolation_is_enforced_on_every_tenant_table`), none skipped. Then
`DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --lib`
(the migration contiguity test). Note: e2e shipper tests in `flags-server` still pass at this point
because the test pool is a superuser, which RLS does not apply to.

- [ ] **Step 5: Lint, format, extra**

Run: `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`,
`git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'` (prints
nothing).

- [ ] **Step 6: Commit**

```bash
git add crates/flags-core/migrations/0005_usage_outbox_maintenance.sql crates/flags-core/tests/db.rs
git commit -m "fix(core): require the maintenance marker for unpinned usage outbox access" \
  -m "The usage outbox tables still let any unpinned transaction read and write every org's rows, so a request path that forgot to pin an org could reach them. A restrictive policy now also requires the app.maintenance marker that only maintenance::begin sets, as 0003 did for the retention tables. Policy-only and additive; the previous release's pinned writes still pass." \
  -m "Fixes #68"
```

---

### Task 2: Fence the shipper's writes and run them in maintenance transactions

**Files:**
- Modify: `crates/flags-core/src/maintenance.rs`
- Modify: `crates/flags-core/src/usage.rs` (`Row`, `ship_once`, `refuse_receipt`, `reschedule`,
  module doc)
- Test: `crates/flags-server/tests/e2e.rs`

**Interfaces:** consumes Task 1's policies. `ship_once`, `run`, `ShipperConfig`, `ShipError`
signatures unchanged. `maintenance::begin` (crate-private) now returns
`sqlx::Result<Transaction<'static, Postgres>>`.

**Out-of-band / invariants:** unpinned access only via `maintenance::begin`; no connection held
across the platform call; no per-row statements added (the per-rejection insert loop becomes one
statement); platform text only at `debug`; `usage_logs.rs` expects exactly one ERROR line per
rejected event containing `refused a usage event`, the event id, and `reason="rejected"`, plus a
DEBUG line with the platform reason: keep those logs as they are.

**Model:** opus — concurrency and transaction logic.

- [ ] **Step 1: Mock platform usage gate** in `crates/flags-server/tests/e2e.rs`. Add to `struct
  Platform`:

```rust
    /// When set, each usage batch hands the test a sender and waits until the
    /// test fires it before answering, so the test can hold shippers mid-call.
    usage_gate: Option<tokio::sync::mpsc::UnboundedSender<tokio::sync::oneshot::Sender<()>>>,
```

  and make `ship` wait on it without holding the std mutex across the await:

```rust
async fn ship(State(p): State<Shared>, Json(b): Json<UsageBatch>) -> Json<UsageReceipt> {
    let gate = p.lock().unwrap().usage_gate.clone();
    if let Some(gate) = gate {
        let (release, released) = tokio::sync::oneshot::channel();
        gate.send(release).unwrap();
        released.await.unwrap();
    }
    let mut p = p.lock().unwrap();
    // … the existing body, unchanged …
}
```

- [ ] **Step 2: Write the failing tests** in `crates/flags-server/tests/e2e.rs`, after
  `a_receipt_naming_events_outside_the_batch_moves_nothing` (reuse `row_count`):

```rust
/// A shipper whose claim lapsed while it waited on the platform cannot act on
/// the row once another shipper has claimed it: the newer claim records it,
/// once (#67).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_lapsed_usage_claim_leaves_the_row_to_the_newer_one(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool, &url).await;
    let event_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO usage_outbox (org_id, user_id, tool, billable) \
         VALUES ($1, NULL, 'create_flag', true) RETURNING event_id",
    )
    .bind(Uuid::new_v4())
    .fetch_one(s.db.pool())
    .await
    .unwrap();
    let (gate, mut held) = tokio::sync::mpsc::unbounded_channel();
    {
        let mut p = platform.lock().unwrap();
        p.reject_with = Some("unknown org".into());
        p.usage_gate = Some(gate);
    }
    // A lease of 0: a claimed row is due again at once, as if the claim lapsed.
    let cfg = flags_core::usage::ShipperConfig {
        claim_lease: std::time::Duration::ZERO,
        ..Default::default()
    };
    let shipper = |cfg: flags_core::usage::ShipperConfig| {
        let (db, platform) = (s.db.clone(), s.platform.clone());
        tokio::spawn(async move { flags_core::usage::ship_once(&db, &platform, &cfg).await })
    };

    let first = shipper(cfg.clone());
    let release_first = held.recv().await.unwrap();
    let second = shipper(cfg.clone());
    let release_second = held.recv().await.unwrap();

    // The first shipper's claim has lapsed: its rejection is not recorded and
    // the row it no longer holds stays for the second.
    release_first.send(()).unwrap();
    assert_eq!(first.await.unwrap().unwrap(), 1);
    assert_eq!(row_count(s.db.pool(), "usage_outbox").await, 1);
    assert_eq!(row_count(s.db.pool(), "usage_outbox_rejected").await, 0);

    release_second.send(()).unwrap();
    assert_eq!(second.await.unwrap().unwrap(), 1);
    let rejected: Vec<Uuid> = sqlx::query_scalar("SELECT event_id FROM usage_outbox_rejected")
        .fetch_all(s.db.pool())
        .await
        .unwrap();
    assert_eq!(rejected, vec![event_id]);
    assert_eq!(row_count(s.db.pool(), "usage_outbox").await, 0);
}

/// The shipper reads and writes the outbox in maintenance transactions, so it
/// still works when its pool is subject to row-level security (#68). The test
/// pool is a superuser, which RLS ignores, so this one runs as otto_app.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn the_usage_shipper_sets_the_maintenance_marker(pool: PgPool) {
    let (url, platform) = start_platform().await;
    let s = server(pool.clone(), &url).await;
    let as_app = sqlx::postgres::PgPoolOptions::new()
        .max_connections(2)
        .after_connect(|conn, _| {
            Box::pin(async move {
                sqlx::query("SET ROLE otto_app").execute(conn).await?;
                Ok(())
            })
        })
        .connect_with((*pool.connect_options()).clone())
        .await
        .unwrap();
    let insert = || {
        sqlx::query_scalar::<_, Uuid>(
            "INSERT INTO usage_outbox (org_id, user_id, tool, billable) \
             VALUES ($1, NULL, 'create_flag', true) RETURNING event_id",
        )
        .bind(Uuid::new_v4())
        .fetch_one(&pool)
    };
    let _accepted = insert().await.unwrap();
    let refused = insert().await.unwrap();

    // The role is subject to RLS: unpinned and unmarked, it sees nothing.
    let unmarked: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_outbox")
        .fetch_one(&as_app)
        .await
        .unwrap();
    assert_eq!(unmarked, 0);

    platform.lock().unwrap().receipt = Some(UsageReceipt {
        accepted: 1,
        duplicates: 0,
        rejected: vec![RejectedEvent {
            event_id: refused,
            reason: "x".into(),
        }],
    });
    let db = otto_tenant::Db::from_pool(as_app);
    let n = flags_core::usage::ship_once(&db, &s.platform, &Default::default())
        .await
        .unwrap();
    assert_eq!(n, 2);
    assert_eq!(row_count(&pool, "usage_outbox").await, 0);
    let rejected: Vec<Uuid> = sqlx::query_scalar("SELECT event_id FROM usage_outbox_rejected")
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(rejected, vec![refused]);
    db.pool().close().await;
}
```

  Adapt only what the compiler requires (for example `ShipperConfig: Clone` already holds; if
  `Db` is not `Clone`, wrap it in `Arc`; import `otto_tenant::Db` if the file already names it as
  `Db`). Keep every assertion.

- [ ] **Step 3: Run them and see them fail**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-server --test e2e -- a_lapsed_usage_claim_leaves_the_row_to_the_newer_one the_usage_shipper_sets_the_maintenance_marker`
Expected: both FAIL — the first on `row_count(.., "usage_outbox") == 1` after the first shipper
(left: 0), the second on `n == 2` (left: 0: the shipper's unmarked claim sees no rows).

- [ ] **Step 4: `maintenance::begin` returns `sqlx::Result`** in
  `crates/flags-core/src/maintenance.rs`:

```rust
pub(crate) async fn begin(db: &Db) -> sqlx::Result<Transaction<'static, Postgres>> {
    let mut tx = db.pool().begin().await?;
    sqlx::query("SET LOCAL app.maintenance = 'on'")
        .execute(&mut *tx)
        .await?;
    Ok(tx)
}
```

  Remove the now-unused `use crate::error::Result;`. Add the usage shipper to the module doc's
  list of callers ("the retention sweeps, the rollback checker's backstop, and the usage shipper")
  and to the list of policies (`usage_outbox_maintenance`, `usage_outbox_rejected_maintenance`).
  Existing callers (`platform_events::sweep`, `telemetry` sweep, `rollback::due_policies`) keep
  `?` (`crate::Error: From<sqlx::Error>`).

- [ ] **Step 5: Fence and wrap the shipper** in `crates/flags-core/src/usage.rs`:

  - `Row` gains `attempts: i32`; the claim's `RETURNING` adds `attempts`.
  - The claim runs as `let mut tx = crate::maintenance::begin(db).await?;` …
    `.fetch_all(&mut *tx)` … `tx.commit().await?;` — committed before the platform call.
  - After sorting: `let ids: Vec<i64> = …; let attempts: Vec<i32> = rows.iter().map(|r|
    r.attempts).collect();`
  - `reschedule(db, &ids, &attempts, reason, cfg)` and `refuse_receipt(db, &ids, &attempts, msg,
    cfg)` everywhere they are called.
  - Replace the finish transaction (the per-rejection `INSERT` loop and the `DELETE`) with: keep
    the per-event `tracing::error!`/`tracing::debug!` pair for each `receipt.rejected` entry
    exactly as now (fixed `reason = "rejected"` above debug, platform reason only at debug), build
    `rejected_ids: Vec<Uuid>` and `reasons: Vec<String>` (`crate::telemetry::stored(&r.reason,
    MAX_REJECTED_REASON)`), then:

```rust
    let mut tx = crate::maintenance::begin(db).await?;
    // Fenced on the attempts value this claim set: a row whose claim lapsed and
    // was taken by another shipper has a newer value, so it is neither deleted
    // nor recorded here; the newer holder does both (#67). A row is recorded as
    // rejected only if this statement deleted it, and a row can be deleted
    // once, so a rejection is recorded at most once even when two shippers
    // finish together: the later DELETE waits on the row lock, then re-checks
    // the fence against the row as the newer claim left it.
    let (deleted, moved): (i64, i64) = sqlx::query_as(
        "WITH gone AS ( \
           DELETE FROM usage_outbox o \
            USING unnest($1::int8[], $2::int4[]) AS c(id, attempts) \
            WHERE o.id = c.id AND o.attempts = c.attempts \
           RETURNING o.event_id, o.org_id, o.user_id, o.tool, o.billable, o.occurred_at), \
         moved AS ( \
           INSERT INTO usage_outbox_rejected \
             (event_id, org_id, user_id, tool, billable, occurred_at, reason) \
           SELECT g.event_id, g.org_id, g.user_id, g.tool, g.billable, g.occurred_at, r.reason \
             FROM gone g JOIN unnest($3::uuid[], $4::text[]) AS r(event_id, reason) \
               ON r.event_id = g.event_id \
           RETURNING 1) \
         SELECT (SELECT count(*) FROM gone), (SELECT count(*) FROM moved)",
    )
    .bind(&ids)
    .bind(&attempts)
    .bind(&rejected_ids)
    .bind(&reasons)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    if (deleted as usize) < ids.len() {
        tracing::warn!(claimed = ids.len(), lost = ids.len() - deleted as usize,
            "usage rows this shipper claimed are no longer held by its claim (reclaimed after it lapsed, or purged); left as they are");
    }
    if (moved as usize) < rejected_ids.len() {
        tracing::warn!(rejected = rejected_ids.len(), recorded = moved,
            "rejected usage events are no longer held by this claim; left for their current holder to record");
    }
    Ok(ids.len())
```

  - `reschedule` becomes:

```rust
async fn reschedule(db: &Db, ids: &[i64], attempts: &[i32], reason: &str, cfg: &ShipperConfig) {
    let r = async {
        let mut tx = crate::maintenance::begin(db).await?;
        // Fenced like the finish step: a lapsed claim must not overwrite the
        // schedule or last_error of a newer one (#67).
        sqlx::query(
            "UPDATE usage_outbox o SET last_error = $3, \
                    next_attempt_at = now() + make_interval(secs => \
                        LEAST($4::float8 * power(2, LEAST(o.attempts - 1, 20)), $5::float8)) \
               FROM unnest($1::int8[], $2::int4[]) AS c(id, attempts) \
              WHERE o.id = c.id AND o.attempts = c.attempts",
        )
        .bind(ids)
        .bind(attempts)
        .bind(reason)
        .bind(cfg.base_backoff.as_secs_f64())
        .bind(cfg.max_backoff.as_secs_f64())
        .execute(&mut *tx)
        .await?;
        tx.commit().await
    }
    .await;
    if let Err(e) = r {
        // … the existing warn!/debug! logging, unchanged …
    }
}
```

  - Update the `ship_once` doc ("Never loses a row …") to add that every statement runs in a
    maintenance transaction and the writes after the claim are fenced on `(id, attempts)`; update
    the module doc's "Shipping is a background task" bullet the same way in one sentence.

- [ ] **Step 6: Run the tests and see them pass**

Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-server --test e2e -- a_lapsed_usage_claim_leaves_the_row_to_the_newer_one the_usage_shipper_sets_the_maintenance_marker`
Expected: both PASS. Then the full gate:
`DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace` — every
binary passes, including `usage_logs`, `a_rejected_usage_event_keeps_a_capped_reason`, and
`a_receipt_naming_events_outside_the_batch_moves_nothing`, none skipped.

- [ ] **Step 7: Lint, format, extra**

Run: `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`,
`git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'` (prints
nothing).

- [ ] **Step 8: Commit**

```bash
git add crates/flags-core/src/maintenance.rs crates/flags-core/src/usage.rs crates/flags-server/tests/e2e.rs
git commit -m "fix(core): fence usage shipper writes and run them as maintenance" \
  -m "A shipper whose claim lapsed while it waited on the platform could delete and record rows another shipper had since claimed, and two shippers finishing together could both record one rejection. The finish step and reschedule now match the attempts value the claim set, and the finish deletes first and records only what it deleted, in one statement. Every outbox statement runs in maintenance::begin, which the new marker policy requires for unpinned access." \
  -m "Fixes #67" -m "Fixes #68"
```

---

### Task 3: Document the marker on the usage outbox

**Files:**
- Modify: `docs/deploy/fly.md`

**Interfaces:** none.

**Out-of-band / invariants:** none.

**Model:** haiku — a docs update to match the change.

- [ ] **Step 1: Edit** the bullet in `docs/deploy/fly.md` that begins "Migration `0003` (#12)
  gives the retention sweeps an unpinned read": after its sentence about `0002` and `0003`
  requiring the marker, add: "Migration `0005` (#68) does the same for `usage_outbox` and
  `usage_outbox_rejected`, for every command, and the usage shipper runs in a maintenance
  transaction. Under the superuser attach role RLS does not apply, so this changes nothing in
  production today; under a non-exempt pool role, a release without this change ships no usage
  while it runs against that schema (rollout overlap, rollback), and the rows wait in the outbox
  until a newer release ships them."
- [ ] **Step 2: Check** with `git diff docs/deploy/fly.md` that only that bullet changed.
- [ ] **Step 3: Commit**

```bash
git add docs/deploy/fly.md
git commit -m "docs(deploy): note the maintenance marker on the usage outbox" \
  -m "Operators reading the RLS notes need to know that 0005 extends the marker requirement to the usage outbox, and what a release older than it does under a non-exempt pool role." \
  -m "Fixes #68"
```
