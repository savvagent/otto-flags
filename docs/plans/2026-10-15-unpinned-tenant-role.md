# Unpinned tenant role Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** every unpinned query in flags-core runs through `flags_core::unpinned::begin`, which
drops into the tenant role (or refuses an RLS-exempt role it cannot leave) without the
maintenance marker; a test in `tests/db.rs` refuses any new raw `.pool()` / `begin_unpinned` site.

**Architecture:** `maintenance::begin`'s body moves to `unpinned::begin_with_marker(db, bool)`;
`maintenance::begin` and the new `unpinned::begin` call it. `keys::resolve_hash`,
`platform_events::revoked`, and the two tombstone writes switch to `unpinned::begin`. A textual
check over every workspace crate's non-test `src` pins the allowed sites.

**Tech Stack:** Rust (stable, MSRV 1.88), sqlx 0.8, Postgres 16, otto-tenant at rev `fa1ae08`.

**Ticket:** savvagent/otto-flags#80

**Spec:** `docs/specs/2026-10-15-unpinned-tenant-role-design.md`

## Global Constraints

- Worktree only:
  `/home/robhicks/dev/otto-flags/.claude/worktrees/refactor-80-unpinned-tenant-role`; `main`
  changes only through the PR.
- Gates, from the worktree root, with `CARGO_BUILD_JOBS=3` and `TMPDIR=$PWD/target/tmp`:
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15445/otto_flags cargo test --workspace`
    (this run's dedicated Postgres; the section's 15434 is shared by parallel agents)
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'`
    prints nothing
  - `podman build -t otto-flags:check .` (crates changed; once before the PR)
- Evidence: pass counts per test binary; a skipped or ignored test is not a pass.
- Invariants: tenant isolation is RLS under `otto_app`; unpinned cross-org access only through
  `maintenance::begin`; no SQL outside flags-core, runtime queries only; small shared database;
  no migration.
- No attribution of any kind in commits, comments, or docs.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why, ends `Fixes #80`.

### Task 1: shared helper, callers, check

Model: opus

**Files:**
- Create: `crates/flags-core/src/unpinned.rs`
- Modify: `crates/flags-core/src/lib.rs` (`mod unpinned;`), `crates/flags-core/src/maintenance.rs`,
  `crates/flags-core/src/keys.rs`, `crates/flags-core/src/platform_events.rs`,
  `crates/flags-core/tests/db.rs`, `CLAUDE.md`, `docs/deploy/fly.md`

- [ ] **Step 1: failing tests.** In `unpinned.rs` add `#[sqlx::test(migrator = "crate::MIGRATOR")]`
  tests: `begin_runs_as_the_tenant_role_without_the_marker`,
  `begin_sees_no_tenant_rows` (insert a `flag_apps` row on the superuser pool, count inside the
  transaction is 0), `begin_reads_and_writes_the_pre_org_tables` (insert into `deleted_orgs`,
  `removed_members`, select from `app_keys`), `begin_refuses_an_exempt_role_that_cannot_assume_the_tenant_role`.
  In `tests/db.rs` add `no_raw_pool_queries_outside_the_allowed_sites` with the allowlist
  `flags-core/src/unpinned.rs: 1`, `flags-core/src/migrate.rs: 1`,
  `flags-server/src/health.rs: 1`, `flags-server/src/main.rs: 1`.
- [ ] **Step 2: run, see them fail** (`cargo test -p flags-core` with `DATABASE_URL`): the unit
  tests do not compile; the check reports `keys.rs`, `platform_events.rs`, `maintenance.rs`.
- [ ] **Step 3: implement.** Move the probe into `unpinned::begin_with_marker`; make
  `maintenance::begin` a call to it; switch the four call sites; keep `revoked_on`'s SQL.
- [ ] **Step 4: run, see them pass**, plus the full gates.
- [ ] **Step 5: docs.** `CLAUDE.md` invariant bullet: one clause naming `unpinned::begin` and the
  check. `docs/deploy/fly.md` RLS note: replace "A query run directly on the pool ... still runs as
  the superuser attach role" with the new behavior.
- [ ] **Step 6: commit** `refactor(core): run unpinned queries as the tenant role`, body explains
  why, ends `Fixes #80`.
