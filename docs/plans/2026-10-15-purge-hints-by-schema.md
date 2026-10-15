# Purge hints by schema Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** The audit purge startup check names `public.purge_deleted_org_audit(uuid)` in every
statement it suggests and in its summary, built from the same source as `FUNCTION`, and its
missing-function message covers a function 0004 created outside `public`.

**Architecture:** Split the private `purge_function!()` macro into `purge_name!()` and the
qualified name built from it. `problems()` and `summary()` interpolate `FUNCTION`; the
missing-function message adds a catalog query and a `SET SCHEMA public` statement built from
`purge_name!()`.

**Tech Stack:** Rust (stable, edition 2021), sqlx (runtime queries), Postgres 16.

**Ticket:** #98

**Spec:** `docs/specs/2026-10-15-purge-hints-by-schema-design.md`

## Global Constraints

- Work in the worktree `.claude/worktrees/fix-98-qualified-purge-hints` only. Trunk changes only via PR.
- Export `CARGO_BUILD_JOBS=3` and `TMPDIR=$PWD/target/tmp` (create it first) for every cargo command.
- Gates:
  - `DATABASE_URL=postgres://flags:flags@localhost:15447/otto_flags cargo test --workspace`
    (dedicated Postgres; never 15434 or a Fly database)
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo fmt --all -- --check`
  - `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'` prints nothing
  - `podman build -t otto-flags:check .`
- Evidence: pass counts per test binary. A "DATABASE_URL must be set" panic is a missing variable, not a test result.
- Invariants: no SQL outside `flags-core`; runtime queries only, no `query!` macros; no
  migration; `FUNCTION` and `PURGE` keep their values; `PurgeReport`'s fields do not change.
- Existing test assertions are not edited (none pins the changed text).
- Never run a suggested statement against production; they are advice for Rob.
- No attribution of any kind in commits, comments, or docs.

---

### Task 1: Name the function by schema in the hints, test first

**Files:**
- Modify: `crates/flags-core/src/audit_purge.rs` (macros, `problems`, `summary`, unit tests)
- Modify: `crates/flags-core/tests/db.rs` (new test after `audit_purge_check_reports_a_missing_function`)
- Modify: `docs/deploy/fly.md` (Infrastructure, migrating-role bullet: boot line and `ALTER FUNCTION` example)

**Interfaces:** none changed. `FUNCTION` and `PURGE` keep their values.

**Out-of-band / invariants:** none out-of-band. No migration; no new server SQL.

**Model:** sonnet

- [x] **Step 1: Write the failing tests.**
  Unit tests in `audit_purge.rs`:
  `suggested_statements_name_the_function_by_schema` (owner problem contains
  `format!("ALTER FUNCTION {FUNCTION} OWNER TO")`, grant problem contains
  `format!("GRANT EXECUTE ON FUNCTION {FUNCTION} TO")`, and `FUNCTION.starts_with("public.")`);
  `a_missing_function_explains_an_applied_migration` (the problem contains `FUNCTION`,
  `recorded as applied`, `proname = 'purge_deleted_org_audit'`,
  `ALTER FUNCTION <schema>.purge_deleted_org_audit(uuid) SET SCHEMA public`, and `0004_audit_purge.sql`);
  extend `the_summary_names_owner_exemption_and_role` with a needle for `FUNCTION`.
  DB test `audit_purge_check_reports_a_function_outside_public`, `#[sqlx::test(migrator =
  "flags_core::MIGRATOR")]`: in `pool.begin()`, run `CREATE SCHEMA stray`,
  `ALTER FUNCTION purge_deleted_org_audit(uuid) SET SCHEMA stray`, `SET LOCAL ROLE otto_app`;
  `inspect` gives `owner == None` and one problem containing
  `ALTER FUNCTION <schema>.purge_deleted_org_audit(uuid) SET SCHEMA public`; then `RESET ROLE`,
  `ALTER FUNCTION stray.purge_deleted_org_audit(uuid) SET SCHEMA public`, `SET LOCAL ROLE otto_app`;
  `inspect` gives no problems; roll back. Doc comment: it proves the advice (#98).
- [x] **Step 2: Run them and see the new ones fail.**
  `DATABASE_URL=postgres://flags:flags@localhost:15447/otto_flags cargo test -p flags-core audit_purge`
  (the DB test fails on the missing advice, not on setup; the unit tests fail on the unqualified text).
- [x] **Step 3: Implement** in `audit_purge.rs`: `purge_name!()` and `purge_function!()` as in the
  spec, defined above their uses; `{FUNCTION}` in the owner, grant, and summary text; the
  missing-function message per spec section 4. Update the two quoted lines in `docs/deploy/fly.md`.
- [x] **Step 4: Run the gates and see them pass**, all of Global Constraints.
- [x] **Step 5: Commit** `fix(core): name the purge function by schema in the startup hints`, body
  explaining why (a pasted statement could act on a same-named function elsewhere on the
  `search_path`; 0004 will not re-run once recorded), ending `Fixes #98`. No attribution.
