# Receipt Rejected Ids Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** A usage receipt can only move rows of the batch it answers, each at most once: a receipt
whose rejected ids are not a duplicate-free subset of the batch is treated like a miscount, and the
insert into `usage_outbox_rejected` is limited to the batch's rows.

**Architecture:** In `ship_once`, after the count check, the rejected ids are checked against a set
of the batch's event ids; a bad receipt goes down the miscount path (error log with counts only,
reschedule with `decode`, return `Decode`) through one shared helper. The insert gains
`AND id = ANY($3)`.

**Tech Stack:** Rust (stable, MSRV 1.88), sqlx 0.8, Postgres 16.

**Ticket:** savvagent/otto-flags#63

**Spec:** `docs/specs/2026-10-14-receipt-rejected-ids-design.md`

## Global Constraints

- Worktree only (`/home/robhicks/dev/otto-flags/.claude/worktrees/fix-63-validate-rejected-ids`);
  `main` changes only through the PR.
- Gates (from `CLAUDE.md` > Gates), from the worktree root. Every DB-touching `cargo test` runs
  under the shared lock, because sibling worktrees share the Postgres cluster:
  - test: `flock /tmp/claude-1000/-home-robhicks-dev-otto-flags/1ac68bfd-14da-4327-8f16-2c64a249eba5/scratchpad/db-test.lock env DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'` prints nothing
  - when `crates/**`: `podman build -t otto-flags:check .`
- Evidence: pass counts per test binary; a `DATABASE_URL must be set` panic is a missing variable,
  not a pass. `DATABASE_URL` only ever points at the local Postgres on 15434.
- Invariants that apply: metering (an event is never both rejected and delivered); the database is
  small and shared (no new per-row queries); no migration; no SQL outside flags-core, runtime
  `sqlx::query` only; platform text stays out of logs above `debug` (#44).
- Out-of-band: none.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #63`. No attribution of any kind (no Co-Authored-By, no "Generated with", no credit
  markers).

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `crates/flags-core/src/usage.rs` | validate rejected ids; shared bad-receipt helper; batch-scoped insert |
| **Modify.** `crates/flags-server/tests/e2e.rs` | mock platform `receipt` override; test bad receipts |

## Task Order & Rationale

One task: a contained check in one function and the test that proves it.

---

### Task 1: Refuse receipts whose rejected ids are not this batch's, once each

**Model:** sonnet

**Files:**
- Modify: `crates/flags-core/src/usage.rs`
- Modify: `crates/flags-server/tests/e2e.rs`

- [ ] **Step 1: Write the failing test** in `crates/flags-server/tests/e2e.rs`:
  - Mock: add to `struct Platform` a field `/// When set, answered verbatim to every usage batch.
    receipt: Option<UsageReceipt>,`. In `ship`, first: `if let Some(r) = &p.receipt { return
    Json(r.clone()); }` (before the `reject_with` branch).
  - Test at the end of the file, doc comment "The platform's receipt can only move rows of the
    batch it answers, each once: one naming an event outside the batch, or repeating one, moves
    and deletes nothing (#63).", `#[sqlx::test(migrator = "flags_core::MIGRATOR")]`
    `a_receipt_naming_events_outside_the_batch_moves_nothing(pool: PgPool)`:
    - `start_platform`, `server(pool, &url)`. Insert row A (due) and row B with
      `next_attempt_at = now() + interval '1 hour'`, both via
      `INSERT INTO usage_outbox (org_id, user_id, tool, billable[, next_attempt_at]) VALUES (...) RETURNING event_id`.
    - Small `async fn` helpers taking `&otto_tenant::Db` (or `&PgPool`) for: count of `usage_outbox`, count of `usage_outbox_rejected`, and
      making every row due (`UPDATE usage_outbox SET next_attempt_at = now()`).
    - Case 1 (outside the batch): set `receipt = Some(UsageReceipt { accepted: 0, duplicates: 0,
      rejected: vec![RejectedEvent { event_id: b, reason: "x".into() }] })`. `ship_once(&s.db,
      &s.platform, &Default::default())` is `Err`, `failure_reason().reason == "decode"`;
      `usage_outbox` count 2; A's `last_error` is `Some("decode")`; B is untouched (`last_error` is `None`, `attempts` 0);
      `usage_outbox_rejected` count 0.
    - Case 2 (repeated): make all due; receipt `{accepted: 0, duplicates: 0, rejected: [A, A]}`.
      `ship_once` is `Err` with reason `decode`; `usage_outbox` count 2; `usage_outbox_rejected`
      count 0.
    - Case 3 (control): make all due; receipt `{accepted: 1, duplicates: 0, rejected: [A]}`.
      `ship_once` returns `Ok(2)`; `usage_outbox_rejected` has exactly one row, its `event_id`
      is A and its `reason` is `"x"`; `usage_outbox` count 0.
- [ ] **Step 2: Run, see it fail:**
  `flock /tmp/claude-1000/-home-robhicks-dev-otto-flags/1ac68bfd-14da-4327-8f16-2c64a249eba5/scratchpad/db-test.lock env DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-server --test e2e a_receipt_naming_events_outside_the_batch_moves_nothing`
  — case 1 fails (`ship_once` returns `Ok(1)` and copies B into `usage_outbox_rejected`).
- [ ] **Step 3: Implement** in `crates/flags-core/src/usage.rs` `ship_once`:
  - Factor the miscount branch into a helper, e.g.
    `async fn refuse_receipt(db: &Db, ids: &[i64], msg: String, cfg: &ShipperConfig) -> ShipError`
    which logs `tracing::error!("{msg}; keeping every row and retrying")`, builds
    `otto_resource::Error::Decode(msg)`, calls `reschedule(db, ids, &stored_reason(&err), cfg)`,
    and returns `err.into()`. The miscount branch uses it unchanged in behaviour.
  - After the count check: `let mut unanswered: HashSet<Uuid> = rows.iter().map(|r| r.event_id).collect();`
    `let stray = receipt.rejected.iter().filter(|r| !unanswered.remove(&r.event_id)).count();`
    If `stray > 0`, return `Err(refuse_receipt(db, &ids, format!("the platform rejected {} usage
    events, {stray} of them not in this batch or repeated", receipt.rejected.len()), cfg).await)`.
    Add a comment saying why: an id outside the batch would be recorded as rejected while still
    shipped later; a repeated id would insert two rejected rows (#63). No platform-supplied id in
    the message.
  - The insert: `... FROM usage_outbox WHERE event_id = $1 AND id = ANY($3)`, `.bind(&ids)` third;
    note in the comment that the insert can only reach this batch's rows.
  - `use std::collections::{HashMap, HashSet};`
  - The per-rejection `error!`/`debug!` logs stay inside the insert loop (not in the helper), so a
    refused receipt's ids are never logged.
- [ ] **Step 4: Run, see it pass:** the Step 2 command, then all gates (Global Constraints),
  recording pass counts per test binary. `usage_logs` and the existing e2e usage tests must pass
  unchanged.
- [ ] **Step 5: Commit:** `fix(core): refuse a usage receipt that rejects events outside its batch`,
  body saying why (a rejected id outside the batch was recorded as rejected yet still shipped; a
  repeated id duplicated the rejected row while the count check passed) and ending `Fixes #63`.
  No attribution.
