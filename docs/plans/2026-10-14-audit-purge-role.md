# Audit-trail purge Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** `org.deleted` purges the org's audit trail through `purge_deleted_org_audit`, a
`SECURITY DEFINER` function owned by the migrating role, inside the pinned purge transaction, so it
works when the pool runs as `otto_app`, without `otto_app` gaining DELETE on `audit_events`.

**Architecture:** New migration `0004_audit_purge.sql` (function, two marker-gated policies scoped
to the migrating role, EXECUTE grant to `otto_app`); `org_deleted` calls the function on
`tx.conn()` instead of the unpinned `DELETE`; two DB tests.

**Tech Stack:** Rust (stable, MSRV 1.88), sqlx 0.8.6, Postgres 16.

**Ticket:** savvagent/otto-flags#21

**Spec:** `docs/specs/2026-10-14-audit-purge-role-design.md`

## Global Constraints

- Worktree only (`/home/robhicks/dev/otto-flags/.claude/worktrees/fix-21-audit-purge-role`);
  `main` changes only through the PR.
- Gates (from `CLAUDE.md` > Gates), from the worktree root. Another worktree shares the local
  Postgres, so every DB test run takes the run's lock:
  - test: `flock /tmp/claude-1000/-home-robhicks-dev-otto-flags/b406261e-e2cc-4fde-a8a9-63de98aae867/scratchpad/dbtest.lock env DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'` prints nothing (`0004` is an addition, status `A`, which this filter does not print)
  - when `crates/**`: `podman build -t otto-flags:check .`
- Evidence: pass counts per test binary; a `DATABASE_URL must be set` panic is a missing variable,
  not a pass. `DATABASE_URL` only ever points at the local Postgres on 15434.
- Invariants that apply: append-only migrations (a new `0004`; never touch `0001`–`0003`); the
  migration is additive and safe for the previous release during rollout and after rollback;
  tenant isolation (`otto_app` gains EXECUTE on one guarded function, no table privilege or policy;
  `tenant_isolation_is_enforced_on_every_tenant_table` must stay green); no SQL outside flags-core,
  runtime `sqlx::query` only; webhook application stays idempotent.
- Security details that must survive exactly as written in the spec's §8: `SECURITY DEFINER`,
  `SET search_path = pg_catalog, public, pg_temp` (pg_temp last), `public.`-qualified table names in
  the body and in the policy subqueries, `SET app.audit_purge = 'on'` on the function, both policies
  `TO CURRENT_USER` and gated on the marker and on `public.deleted_orgs`, `REVOKE EXECUTE … FROM
  PUBLIC`, `GRANT EXECUTE … TO otto_app` inside the `otto_app`-exists guard, and
  `SET LOCAL lock_timeout = '3s'` first.
- Out-of-band: the migration applies at boot (agent-owned). Nothing human-only.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #21`. No attribution of any kind (no Co-Authored-By, no "Generated with", no credit
  markers).

### Task 1: Purge the audit trail through an owned, guarded function

**Model:** sonnet

**Files:**
- Create: `crates/flags-core/migrations/0004_audit_purge.sql`
- Modify: `crates/flags-core/src/platform_events.rs` (`org_deleted` only)
- Modify: `crates/flags-core/tests/db.rs` (two new tests, after
  `org_deleted_purges_everything_and_revokes_keys`; use the existing `otto_app_db` helper)

- [x] **Step 1: Write the failing tests** in `crates/flags-core/tests/db.rs`, each with
  `#[sqlx::test(migrator = "flags_core::MIGRATOR")]`. In both, build the superuser `Db` with
  `db(pool.clone()).await` before `otto_app_db(&pool)` (as `due_policies_sees_every_org_as_otto_app`
  does), and check SQLSTATEs with the file's existing `sqlstate(&err)` helper:
  - `org_deleted_purges_the_audit_trail_as_otto_app`: on the superuser `db`, create two orgs with an
    app each (`create_app` writes audit rows; follow `org_deleted_purges_everything_and_revokes_keys`
    for setup and for building the `org.deleted` event). Assert each org has > 0 `audit_events` rows.
    Build `let app_db = otto_app_db(&pool).await;` and apply the event with
    `platform_events::apply(&app_db, &event)`. Assert `Outcome::Applied { detail }` with
    `detail["audit_events"]` > 0; on the superuser pool, the deleted org has 0 `audit_events` rows
    and the other org's count is unchanged; a second `apply` is `Outcome::Duplicate`. Close
    `app_db.pool()` before returning.
  - `audit_purge_only_reaches_deleted_orgs`: one live org with audit rows (as above); count them on
    the superuser pool first. With
    `app_db = otto_app_db(&pool)` and `let mut tx = app_db.begin(org).await` (pinned, as production
    calls it; one connection for the whole test body):
    1. `SELECT purge_deleted_org_audit($1)` returns 0 (`i64`).
    2. `CREATE TEMP TABLE deleted_orgs (org_id uuid)`, `INSERT` the live org into it, call the
       function again: returns 0.
    3. `query_scalar::<_, Option<String>>("SELECT current_setting('app.audit_purge', true)")` is
       not `Some("on")` (after the function's `SET` is undone it is usually `''`).
    4. `DELETE FROM audit_events WHERE org_id = $1` fails with SQLSTATE `42501` (last statement
       before the rollback, so no savepoint is needed).
    Roll back the transaction; on the superuser pool the org's audit row count is unchanged. Close
    `app_db.pool()` before returning.
- [x] **Step 2: Run them, see them fail** (the first with `permission denied` from the purge, the
  second because the function does not exist):
  `flock /tmp/claude-1000/-home-robhicks-dev-otto-flags/b406261e-e2cc-4fde-a8a9-63de98aae867/scratchpad/dbtest.lock env DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --test db -- org_deleted_purges_the_audit_trail_as_otto_app audit_purge_only_reaches_deleted_orgs`
- [x] **Step 3: Implement.**
  - Create `crates/flags-core/migrations/0004_audit_purge.sql` with exactly the SQL in the spec's
    §8 (copy it, including its comments).
  - In `org_deleted` (`crates/flags-core/src/platform_events.rs`): remove the unpinned
    `DELETE FROM audit_events … .execute(db.pool())` and its comment; after
    `exclude_requests(&mut tx, org).await?;` run
    `sqlx::query_scalar::<_, i64>("SELECT purge_deleted_org_audit($1)").bind(org).fetch_one(tx.conn()).await?`
    and keep inserting that count as `deleted["audit_events"]`. Replace the old comment with one
    saying why: the trail is append-only to `otto_app`, so it goes through the migrating role's
    guarded function (#21), inside the purge transaction so it is removed atomically with the rest.
- [x] **Step 4: Run the new tests, then the whole `db` binary** (the purge, isolation, and sweep
  tests must stay green):
  `flock /tmp/claude-1000/-home-robhicks-dev-otto-flags/b406261e-e2cc-4fde-a8a9-63de98aae867/scratchpad/dbtest.lock env DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --test db`
- [x] **Step 5: Run every gate** (Global Constraints) and report pass counts per test binary.
- [x] **Step 6: Commit** the three files with exactly this message (no trailers of any kind):

  ```
  fix(core): purge a deleted org's audit trail through a guarded function

  otto_app has no DELETE on the append-only audit trail, so the org.deleted
  purge deleted it unpinned on the pool's role, which only works while the
  pool is exempt from RLS. Migration 0004 adds purge_deleted_org_audit, a
  SECURITY DEFINER function owned by the migrating role that only removes
  a tombstoned org's rows, with marker-gated policies for an owner that is
  not exempt; the purge calls it inside its transaction, as otto_app.

  Fixes #21
  ```

The spec's `**Status:**` line and this plan's boxes are updated in the PR's final commit by the
orchestrator.
