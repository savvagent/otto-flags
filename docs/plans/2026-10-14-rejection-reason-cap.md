# Rejection Reason Cap Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** The platform's per-event rejection reason is stored in `usage_outbox_rejected` capped at
500 characters with NUL replaced, so it is bounded and can never fail the shipping transaction.

**Architecture:** `ship_once` binds `crate::telemetry::stored(&r.reason, MAX_REJECTED_REASON)`;
`telemetry::stored` becomes `pub(crate)` as the crate's one rule for remote text bound into a
`text` column.

**Tech Stack:** Rust (stable, MSRV 1.88), sqlx 0.8, Postgres 16.

**Ticket:** savvagent/otto-flags#51

**Spec:** `docs/specs/2026-10-14-rejection-reason-cap-design.md`

## Global Constraints

- Worktree only (`/home/robhicks/dev/otto-flags/.claude/worktrees/fix-51-rejection-reason-cap`);
  `main` changes only through the PR.
- Gates (from `CLAUDE.md` > Gates), from the worktree root:
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'` prints nothing
  - when `crates/**`: `podman build -t otto-flags:check .`
- Evidence: pass counts per test binary; a `DATABASE_URL must be set` panic is a missing variable,
  not a pass. `DATABASE_URL` only ever points at the local Postgres on 15434. Only one DB test run
  at a time against that Postgres.
- Invariants that apply: the database is small and shared (this bounds a column); no migration (the
  column is unchanged); no SQL outside flags-core; platform text stays out of logs above `debug`
  (#44, unchanged).
- Out-of-band: none.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #51`. No attribution of any kind (no Co-Authored-By, no "Generated with", no credit
  markers).

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `crates/flags-core/src/usage.rs` | `MAX_REJECTED_REASON`; cap and clean the reason on insert |
| **Modify.** `crates/flags-core/src/telemetry.rs` | `stored` becomes `pub(crate)`, doc says so |
| **Modify.** `crates/flags-server/tests/e2e.rs` | mock platform `reject_with`; test the stored reason |

## Task Order & Rationale

One task: a one-line binding change, its helper's visibility, and the test that proves it.

---

### Task 1: Cap and clean the stored rejection reason

**Model:** sonnet

**Files:**
- Modify: `crates/flags-core/src/usage.rs`
- Modify: `crates/flags-core/src/telemetry.rs`
- Modify: `crates/flags-server/tests/e2e.rs`

- [ ] **Step 1: Write the failing test** in `crates/flags-server/tests/e2e.rs`:
  - Mock: add to `struct Platform` a field `/// When set, every shipped usage event is rejected
    with this reason. reject_with: Option<String>,`. In `ship`, when `reject_with` is `Some(reason)`,
    answer `UsageReceipt { accepted: 0, duplicates: 0, rejected: <one RejectedEvent { event_id,
    reason: reason.clone() } per event> }` (import `RejectedEvent` from `otto_resource` beside
    `UsageReceipt`); otherwise unchanged.
  - Test, after `only_setting_a_policy_is_billed` (end of file is fine), with a doc comment
    ("The platform's rejection reason is its text: stored capped, and NUL cannot fail the batch
    (#51).") and `#[sqlx::test(migrator = "flags_core::MIGRATOR")]`
    `a_rejected_usage_event_keeps_a_capped_reason`: `start_platform`, `server`; insert one row:
    `INSERT INTO usage_outbox (org_id, user_id, tool, billable) VALUES ($1, NULL, 'create_flag', true) RETURNING event_id`
    on `s.db.pool()` (or the pool the test holds; clone `pool` before passing it to `server` if
    needed); set `platform.lock().unwrap().reject_with = Some(format!("\0{}", "é".repeat(599)))`;
    `flags_core::usage::ship_once(&s.db, &s.platform, &Default::default()).await.unwrap()` is `1`;
    the stored `reason` (`SELECT reason FROM usage_outbox_rejected WHERE event_id = $1`) equals
    `format!("\u{FFFD}{}", "é".repeat(499))` and has 500 characters; `SELECT count(*) FROM
    usage_outbox` is 0.
- [ ] **Step 2: Run, see it fail:**
  `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-server --test e2e a_rejected_usage_event_keeps_a_capped_reason`
  — `ship_once` fails (the NUL fails the insert).
- [ ] **Step 3: Implement.**
  - `crates/flags-core/src/telemetry.rs`: `fn stored` → `pub(crate) fn stored`, with a doc comment:
    remote text (SDK telemetry, the platform's answers) as it is bound into a `text` column: at most
    `max` characters, and NUL, which Postgres `text` cannot hold, replaced with U+FFFD.
  - `crates/flags-core/src/usage.rs`: `/// The platform's rejection reason is its text, kept for
    operators; this many characters are enough to read it, and bound the column.
    const MAX_REJECTED_REASON: usize = 500;` near the other constants; in `ship_once`,
    `.bind(&r.reason)` → `.bind(crate::telemetry::stored(&r.reason, MAX_REJECTED_REASON))`, and
    extend the comment above the insert: stored capped and with NUL replaced, so it is bounded and
    cannot fail the batch.
- [ ] **Step 4: Run, see it pass:** the Step 2 command, then all gates (Global Constraints),
  recording pass counts per test binary. `usage_logs` (its reason is short) must still pass.
- [ ] **Step 5: Commit:** `fix(core): cap the platform's rejection reason in the usage outbox`,
  body saying why (the one unbounded remote-text column on a small shared database; a NUL would
  fail the batch and wedge the shipper) and ending `Fixes #51`. No attribution.
