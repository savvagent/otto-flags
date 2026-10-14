# Member Removed After Org Deleted Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** A `member.removed` delivered after its org's `org.deleted` writes no audit row; it
records the dedupe marker and answers `Ignored` (then `Duplicate` on redelivery).

**Architecture:** `member_removed` in `flags_core::platform_events` checks the tombstone with the
existing `revoked_on(conn, org, None)` under the org's exclusive lifecycle lock, after recording
the delivery, and skips the audit write when the org is deleted.

**Tech Stack:** Rust (stable, MSRV 1.88), sqlx 0.8, Postgres 16.

**Ticket:** savvagent/otto-flags#41

**Spec:** `docs/specs/2026-10-14-member-removed-after-org-deleted-design.md`

## Global Constraints

- Worktree only
  (`/home/robhicks/dev/otto-flags/.claude/worktrees/fix-41-member-removed-after-org-deleted`);
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
- Invariants that apply: platform webhooks apply idempotently (marker in the same transaction);
  tenant isolation unchanged (the read is the existing `revoked_on` query, run as `otto_app` in the
  test); no SQL outside flags-core, runtime `sqlx::query` only; no migration.
- Out-of-band: none.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #41`. No attribution of any kind (no Co-Authored-By, no "Generated with", no credit
  markers).

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `crates/flags-core/src/platform_events.rs` | `member_removed` skips the audit write for a tombstoned org; module doc table |
| **Modify.** `crates/flags-core/tests/db.rs` | New late-event test; live-org test pins `Applied` + one audit row |

## Task Order & Rationale

One task: one function and the tests that prove it.

---

### Task 1: Skip the audit write for a deleted org

**Model:** sonnet

**Files:**
- Modify: `crates/flags-core/src/platform_events.rs`
- Modify: `crates/flags-core/tests/db.rs`

- [ ] **Step 1: Write the DB test** `member_removed_after_org_deleted_writes_no_audit_row` in
  `crates/flags-core/tests/db.rs`, directly after `a_removed_member_is_refused_but_others_are_not`,
  with `#[sqlx::test(migrator = "flags_core::MIGRATOR")]` and a doc comment citing #41:
  - `let db = db(pool.clone()).await;` `let org = OrgId::new();` create an app in `org` with
    `db.begin(org)` / `tx.create_app("web", &[], UserId::new())` / commit, and assert
    `audit_rows(&pool, org).await > 0`.
  - `let app_db = otto_app_db(&pool).await;` Build and apply an `org.deleted` event for `org`
    through `app_db` exactly as `org_deleted_purges_the_audit_trail_as_otto_app` does; assert it is
    `Outcome::Applied { .. }` and `audit_rows(&pool, org).await == 0`.
  - Build a `member.removed` event (`let user = UserId::new();`
    `json!({"org_id": org.as_uuid(), "user_id": user.as_uuid()})`,
    as in `a_removed_member_is_refused_but_others_are_not`), apply it through `app_db`; assert
    `Outcome::Ignored`; assert `audit_rows(&pool, org).await == 0`.
  - Assert the marker exists: `SELECT count(*) FROM platform_events WHERE event_id = $1` on the
    superuser test `pool` (not `app_db`, whose RLS would hide the row; bind `event.id`) equals 1.
  - Assert the removed user is still refused: `platform_events::revoked(&db, org, user)` is true.
  - Apply the same `member.removed` again; assert `Outcome::Duplicate` and
    `audit_rows(&pool, org).await == 0`.
  - `app_db.pool().close().await;` at the end (see `otto_app_db`'s doc).
- [ ] **Step 2: Extend** `a_removed_member_is_refused_but_others_are_not`: assert the `apply` result
  equals `Outcome::Applied { detail: json!({}) }`, and that
  `SELECT count(*) FROM audit_events WHERE org_id = $1 AND action = 'platform.member.removed'`
  (on the test pool; the test takes `pool`, so use `db(pool.clone())` and keep `pool`) equals 1.
- [ ] **Step 3: Run and see the new test fail** (the outcome is `Applied`, not `Ignored`):
  `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --test db member_removed_after_org_deleted`
  and see the extended test pass already:
  `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --test db a_removed_member`
- [ ] **Step 4: Implement** in `crates/flags-core/src/platform_events.rs`, in `member_removed`:
  - After `let first = first_delivery(...)`, add
    `let deleted = revoked_on(tx.conn(), org, None).await?;` with a comment: under the exclusive
    lifecycle lock, so a tombstone committed before this point is seen; one committed later is
    followed by a purge that removes whatever this writes; without the check a late event's audit
    row would outlive its org's purge (#41).
  - Write the audit entry only when `first && !deleted`.
  - After commit, log `tracing::info!(org = %org, event_id = %event.id, "member.removed for a deleted org; ignored")`
    when `deleted`, instead of (not in addition to) the existing line, which stays for the other cases.
  - Return `Outcome::Duplicate` when `!first`, `Outcome::Ignored` when `deleted`, else
    `Outcome::Applied { detail: json!({}) }`.
  - Module doc table, `member.removed` row: append "For an org already deleted, nothing is written
    but the dedupe marker and the short-lived removed-member tombstone (#41)."
- [ ] **Step 5: Run and see both tests pass** (same two commands as Step 3).
- [ ] **Step 6: Run every gate** (Global Constraints) and report pass counts per test binary.
- [ ] **Step 7: Commit** the two files with exactly this message (no trailers of any kind):

  ```
  fix(core): skip the audit write for a member removed from a deleted org

  A member.removed delivered after its org's org.deleted wrote a
  MEMBER_REMOVED audit row into the already-purged org, and nothing would
  remove it short of a redelivered org.deleted. member_removed now checks
  the tombstone under the org's lifecycle lock and, for a deleted org,
  records only the dedupe marker and answers ignored.

  Fixes #41
  ```

Quality review (orchestrator fix): the module doc names the `removed_members` row too,
`Outcome::Ignored`'s doc covers a deleted org, and the deleted-org log line carries `first`.

The spec's `**Status:**` line and this plan's boxes are updated in the PR's final commit by the
orchestrator.
