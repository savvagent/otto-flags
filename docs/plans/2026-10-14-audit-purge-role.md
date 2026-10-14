# Audit-trail purge Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** `org.deleted` purges the org's audit trail through `purge_deleted_org_audit`, a
`SECURITY DEFINER` function owned by the migrating role, inside the pinned purge transaction, so it
works when the pool runs as `otto_app`, without `otto_app` gaining DELETE on `audit_events`.

**Architecture:** New migration `0004_audit_purge.sql`: a PL/pgSQL `SECURITY DEFINER` function
that refuses an owner not exempt from RLS (`55000`) and any org but the pinned one (`42501`), and
the EXECUTE grant to `otto_app`. `org_deleted` calls it on `tx.conn()` instead of the unpinned
`DELETE`; three DB tests. (Task 1 as first planned also had marker-gated policies; Task 2 removed
them.)

**Tech Stack:** Rust (stable, MSRV 1.88), sqlx 0.8.6, Postgres 16.

**Ticket:** savvagent/otto-flags#21

**Spec:** `docs/specs/2026-10-14-audit-purge-role-design.md`

## Global Constraints

- Worktree only (`/home/robhicks/dev/otto-flags/.claude/worktrees/fix-21-audit-purge-role`);
  `main` changes only through the PR.
- Gates (from `CLAUDE.md` > Gates), from the worktree root. Another worktree shared the local
  Postgres, so DB test runs were serialized (see `CLAUDE.md` > Worktree):
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
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
  `SET search_path = pg_catalog, public, pg_temp` (pg_temp last), `public.`-qualified names in
  the body, the owner-exemption check (`55000`), the pinned-org check (`42501`), the
  `deleted_orgs` guard, `REVOKE EXECUTE … FROM PUBLIC`, and `GRANT EXECUTE … TO otto_app` inside
  the `otto_app`-exists guard. (Task 1's original list also named the marker, the policies, and
  `lock_timeout`; Task 2 removed those.)
- Out-of-band: the migration applies at boot (agent-owned). Nothing human-only.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #21`. No attribution of any kind (no Co-Authored-By, no "Generated with", no credit
  markers).

### Task 1: Purge the audit trail through an owned, guarded function

> Shipped, then reworked by Task 2: the marker, the `TO CURRENT_USER` policies, `lock_timeout`, and
> the marker test below were removed in review. Spec §3 and §8 describe what shipped.

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
  `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --test db -- org_deleted_purges_the_audit_trail_as_otto_app audit_purge_only_reaches_deleted_orgs`
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
  `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --test db`
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

### Task 2: Review round (PR #39)

**Model:** opus (done by the orchestrator, with the findings in hand)

PR review found that a non-superuser cannot create a function with a `SET` clause on the custom
`app.audit_purge` parameter, so the marker-gated `TO CURRENT_USER` policies broke the very
configuration they were for, and no test could reach them. They were removed. In their place:

- `0004` is now PL/pgSQL. It refuses an owner that is not exempt from RLS and refuses any org but
  the pinned one (`42501`). It no longer needs `lock_timeout`, and its header supersedes 0001's
  note.
- The tests assert exact purge counts, the cross-org refusal, the refusal for an owner subject to
  RLS (a new test), and the audit trail in the superuser-pool purge test.

- [x] Rework `0004` and the spec to the design in spec §3 and §8.
- [x] Tests as in spec §11; run the `db` binary, then every gate.
- [x] Commit with a why-body ending `Refs #21`; no trailers.

Review round 2 (still Task 2): the owner refusal got its own SQLSTATE (`55000`); the owner test
runs in one rolled-back transaction so a failing run leaves no role; `docs/deploy/fly.md` records
the owner precondition (#40 tracks a startup check); the migration header and spec §12 state that
both guards can be set by the caller. #41 is a pre-existing gap the review found.

