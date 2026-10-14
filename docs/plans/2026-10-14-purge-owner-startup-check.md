# Purge Owner Startup Check Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** `flags-server` refuses to start, naming the cause, when `purge_deleted_org_audit` is
missing, owned by a role subject to row-level security, or not executable by the role tenant
transactions run as.

**Architecture:** A new `flags_core::audit_purge` module reads the function's owner, the owner's
exemption, and the tenant role's `EXECUTE` privilege from the catalog in one query inside a
rolled-back tenant-shaped transaction (`PurgeReport`), and judges them in a pure `problems()`.
`main.rs` calls it right after the tenant-isolation proof and bails on any problem.

**Tech Stack:** Rust (stable, MSRV 1.88), sqlx 0.8, Postgres 16, anyhow, tracing.

**Ticket:** savvagent/otto-flags#40

**Spec:** `docs/specs/2026-10-14-purge-owner-startup-check-design.md`

## Global Constraints

- Worktree only
  (`/home/robhicks/dev/otto-flags/.claude/worktrees/fix-40-purge-owner-startup-check`);
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
- Invariants that apply: no migration (append-only, rollback-compatible by having none); no SQL
  outside flags-core, runtime `sqlx::query_as` only (never `query!`); tenant isolation unchanged
  (the check reads catalogs only, in a transaction it rolls back); database error text stays in
  the boot log, never a tool response.
- Out-of-band: none.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #40`. No attribution of any kind (no Co-Authored-By, no "Generated with", no credit
  markers).

## File Structure

| File | Responsibility |
|---|---|
| **Create.** `crates/flags-core/src/audit_purge.rs` | `PurgeReport`, `problems()`, `summary()`, `inspect()`, `check()`, unit tests |
| **Modify.** `crates/flags-core/src/lib.rs` | `pub mod audit_purge;` |
| **Modify.** `crates/flags-core/tests/db.rs` | DB tests for `check` and `inspect` |
| **Modify.** `crates/flags-server/src/main.rs` | call the check after the isolation proof; refuse to serve on problems |
| **Modify.** `docs/deploy/fly.md` | the precondition paragraph names the boot check |

## Task Order & Rationale

Task 1 builds and proves the check (unit + DB). Task 2 wires it into startup and updates the docs;
it depends on Task 1's API.

---

### Task 1: `flags_core::audit_purge`

**Files:**
- Create: `crates/flags-core/src/audit_purge.rs`
- Modify: `crates/flags-core/src/lib.rs` (add `pub mod audit_purge;` in alphabetical order, after `pub mod audit;`)
- Test: unit tests in `audit_purge.rs`; DB tests appended to `crates/flags-core/tests/db.rs`

**Interfaces:** produces `flags_core::audit_purge::{PurgeReport, inspect, check}` for Task 2.

**Out-of-band / invariants:** no migration; runtime `sqlx::query_as` only; reads catalogs only.

**Model:** sonnet — specified code and tests.

The module (module doc first, explaining #40: why the function's owner must be exempt, that a
non-exempt owner is deliberately unsupported (spec section 3), and that the server refuses to
serve instead):

```rust
use otto_tenant::ids::OrgId;
use otto_tenant::Db;
use sqlx::PgConnection;

use crate::error::Result;

/// The function `org.deleted` purges a deleted org's audit trail through
/// (migration 0004, #21).
pub const FUNCTION: &str = "public.purge_deleted_org_audit(uuid)";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PurgeReport {
    /// The function's owner; `None` when the function does not exist.
    pub owner: Option<String>,
    pub owner_is_superuser: bool,
    pub owner_bypasses_rls: bool,
    /// `current_user` in a transaction shaped like the purge's own.
    pub tenant_role: String,
    /// Whether `tenant_role` may execute the function.
    pub executable: bool,
}

impl PurgeReport {
    pub fn problems(&self) -> Vec<String> { ... }
    pub fn summary(&self) -> String { ... }
}

pub async fn inspect(conn: &mut PgConnection) -> Result<PurgeReport> { ... }

pub async fn check(db: &Db) -> Result<PurgeReport> {
    let mut tx = db.begin(OrgId::new()).await?;
    let report = inspect(tx.conn()).await;
    tx.rollback().await?;
    report
}
```

Query for `inspect`, binding `FUNCTION` as `$1` (decode the owner columns as `Option<String>`,
`Option<bool>`, `Option<bool>` because they are NULL when the function is missing; treat `None`
booleans as `false`):

```sql
SELECT r.rolname::text, r.rolsuper, r.rolbypassrls, current_user::text,
       COALESCE(has_function_privilege(p.oid, 'EXECUTE'), false)
  FROM (SELECT to_regprocedure($1) AS oid) p
  LEFT JOIN pg_catalog.pg_proc f ON f.oid = p.oid
  LEFT JOIN pg_catalog.pg_roles r ON r.oid = f.proowner
```

`problems()`, in this order:
1. `owner == None`: one problem only (return immediately): `purge_deleted_org_audit(uuid) does not
   exist, so every org.deleted would fail: run migrations (0004) against this database before serving (with FLAGS_RUN_MIGRATIONS=false,
   migrate separately first)`.
2. owner neither superuser nor BYPASSRLS: `purge_deleted_org_audit(uuid) is owned by {owner:?},
   which is neither a superuser nor BYPASSRLS, so it cannot see the audit trail it purges and
   every org.deleted would fail (SQLSTATE 55000). Hand the function to a role that is exempt
   (ALTER FUNCTION purge_deleted_org_audit(uuid) OWNER TO <exempt role>), or run migrations as
   one and serve with FLAGS_RUN_MIGRATIONS=false` + (only when `owner != tenant_role`)
   `, or give {owner:?} BYPASSRLS (ALTER ROLE {owner} BYPASSRLS)` + `; see docs/deploy/fly.md`.
   When owner equals the tenant role, BYPASSRLS on it would fail the tenant-isolation proof, so it
   must not be offered.
3. `!executable`: `{tenant_role:?}, the role tenant transactions run as, may not execute
   purge_deleted_org_audit(uuid), so every org.deleted would fail: GRANT EXECUTE ON FUNCTION
   purge_deleted_org_audit(uuid) TO {tenant_role}`.

`summary()`: `audit purge ready: purge_deleted_org_audit(uuid) is owned by {owner:?}
({superuser|BYPASSRLS}) and executable by {tenant_role:?}` (for a missing owner, say `missing`).

- [x] **Step 1: Write the failing unit tests** in `audit_purge.rs` `#[cfg(test)] mod tests` with
  a `ready()` fixture (owner `"postgres"`, superuser, tenant role `"otto_app"`, executable):
  - `ready` has no problems; a BYPASSRLS-only owner has no problems
  - a non-exempt owner (`"migrator"`, tenant role `"otto_app"`) gives exactly one problem containing `"migrator"`, `55000`, `OWNER TO`, and `ALTER ROLE`
  - a non-exempt owner equal to the tenant role (both `"app"`) gives one problem containing `OWNER TO` and NOT containing `ALTER ROLE`
  - a missing function gives exactly one problem containing `does not exist`, even with `executable = false`
  - `executable = false` gives exactly one problem containing `"otto_app"` and `GRANT EXECUTE`
  - non-exempt owner and not executable gives two problems, owner first
  - `summary()` of `ready` contains `"postgres"`, `superuser`, and `"otto_app"`
- [x] **Step 2: Write the failing DB tests** appended to `crates/flags-core/tests/db.rs`
  (`#[sqlx::test(migrator = "flags_core::MIGRATOR")]`, using `flags_core::audit_purge`):
  - `audit_purge_check_passes_on_a_migrated_database`: `check(&Db::from_pool(pool))` has no
    problems, `tenant_role == "otto_app"`, owner is `Some`, `owner_is_superuser`, `executable`.
  - `audit_purge_check_reports_an_owner_subject_to_rls`: in one `pool.begin()` transaction, create
    `purge_owner_<uuid simple>` `NOLOGIN NOSUPERUSER NOBYPASSRLS`, `ALTER FUNCTION … OWNER TO` it,
    `SET LOCAL ROLE otto_app`, `inspect(&mut *tx)`; assert `owner` is the role, not exempt, and
    one problem containing the role name;
    rollback (doc comment: everything is transactional, so a failed run leaves no role behind).
  - `audit_purge_check_accepts_a_bypassrls_owner`: same with `NOSUPERUSER BYPASSRLS`; no problems
    (otto_app's grant survives the owner change, so this tests only the exemption branch).
  - `audit_purge_check_reports_a_tenant_role_without_execute`: in a transaction,
    `REVOKE EXECUTE ON FUNCTION purge_deleted_org_audit(uuid) FROM otto_app`,
    `SET LOCAL ROLE otto_app`, `inspect`; `executable == false`, one problem containing `GRANT EXECUTE`.
  - `audit_purge_check_reports_a_missing_function`: in a transaction, `DROP FUNCTION
    purge_deleted_org_audit(uuid)`, `inspect`; `owner == None`, one problem containing `does not exist`.
- [x] **Step 3: Run to see them fail** (compile errors count):
  `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core audit_purge`
- [x] **Step 4: Implement** the module per above; register it in `lib.rs`.
- [x] **Step 5: Run to see them pass:** the same command, then `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all -- --check`.
- [x] **Step 6: Commit:** `feat(core): read whether the audit purge can run from the catalog`, body
  explaining why (a purge that cannot run is otherwise found on the first org.deleted), ending `Refs #40`.

### Task 2: refuse to serve, and document it

**Files:**
- Modify: `crates/flags-server/src/main.rs` (after the `verify_tenant_isolation` block, before `platform_client`)
- Modify: `docs/deploy/fly.md` (the "migrating role must stay exempt" bullet)

**Interfaces:** consumes `flags_core::audit_purge::check` and `PurgeReport::{problems, summary}`.

**Out-of-band / invariants:** none; `main.rs` holds no SQL.

**Model:** haiku — wiring and docs fully specified.

(After the PR opens, the orchestrator adds the final record-as-shipped commit: the spec's
`**Status:**` becomes `IMPLEMENTED (#<PR>)` and this plan's boxes are ticked.)

- [x] **Step 1: Wire it** in `main.rs`:

```rust
    // The org.deleted purge removes a deleted org's audit trail through a
    // function whose owner must be exempt from row-level security (#40).
    // Like isolation, only the running database can say whether it is.
    let purge = flags_core::audit_purge::check(&db)
        .await
        .context("refusing to serve: could not read the audit purge function from the catalog")?;
    let problems = purge.problems();
    if !problems.is_empty() {
        anyhow::bail!(
            "refusing to serve: an org.deleted could not purge the org's audit trail: {}",
            problems.join(" | ")
        );
    }
    tracing::info!("{}", purge.summary());
```

- [x] **Step 2: Docs.** In `docs/deploy/fly.md`, the "migrating role must stay exempt" bullet: add
  that `flags-server` checks this at boot and refuses to serve otherwise (`refusing to serve: an
  org.deleted could not purge …`), that a healthy boot logs `audit purge ready: …`, and that where
  `otto_app` cannot be assumed and the connecting role owns the function, `BYPASSRLS` on that role
  is not a fix (the isolation proof refuses it): reassign the function or migrate separately.
  Quote the summary line exactly as the code logs it. Do not edit `CLAUDE.md`.
- [x] **Step 3: Gates:** full test, lint, format, extra, and `podman build -t otto-flags:check .`.
- [x] **Step 4: Commit:** `fix(server): refuse to serve when the audit purge cannot run`, body
  explaining why, ending `Fixes #40`.
