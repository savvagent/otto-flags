# Qualified audit purge call Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** `org.deleted` calls `public.purge_deleted_org_audit($1)` by schema. The call string and
`audit_purge::FUNCTION` are built from one definition of the qualified name.

**Architecture:** A private `macro_rules!` in `audit_purge.rs` yields the qualified name. `concat!`
builds both `FUNCTION` (unchanged value) and a new `pub(crate) const PURGE`, which
`platform_events::org_deleted` passes to `query_scalar`.

**Tech Stack:** Rust (stable, edition 2021), sqlx (runtime queries), Postgres 16.

**Ticket:** #53

**Spec:** `docs/specs/2026-10-15-qualified-audit-purge-design.md`

## Global Constraints

- Work in the worktree `.claude/worktrees/fix-53-qualified-audit-purge` only. Trunk changes only via PR.
- Export `CARGO_BUILD_JOBS=3` and `TMPDIR=$PWD/target/tmp` (create it first) for every cargo command.
- Gates:
  - `DATABASE_URL=postgres://flags:flags@localhost:15441/otto_flags cargo test --workspace`
    (dedicated Postgres; never 15434 or a Fly database)
  - `cargo clippy --workspace --all-targets -- -D warnings`
  - `cargo fmt --all -- --check`
  - `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'` prints nothing
  - `podman build -t otto-flags:check .`
- Evidence: pass counts per test binary. A "DATABASE_URL must be set" panic is a missing variable, not a test result.
- Invariants: no SQL outside `flags-core`; runtime `query`/`query_scalar` only, no `query!` macros;
  no migration; tenant isolation and webhook idempotency unchanged. Keep `platform_events.rs`
  edits minimal: #80 also edits that file.
- No attribution of any kind in commits, comments, or docs.

---

### Task 1: Qualify the purge call, test first

**Files:**
- Modify: `crates/flags-core/tests/db.rs` (new test after `org_deleted_purges_the_audit_trail_as_otto_app`)
- Modify: `crates/flags-core/src/audit_purge.rs` (the `FUNCTION` constant)
- Modify: `crates/flags-core/src/platform_events.rs` (the purge call in `org_deleted`, and imports)

**Interfaces:** `audit_purge::FUNCTION` keeps its value. New `pub(crate) const audit_purge::PURGE: &str`.

**Out-of-band / invariants:** none out-of-band. Runtime query, SQL stays in `flags-core`, no migration.

**Model:** sonnet

- [x] **Step 1: Write the failing test** in `crates/flags-core/tests/db.rs`:
  `org_deleted_purges_through_public_whatever_the_search_path`, using
  `#[sqlx::test(migrator = "flags_core::MIGRATOR")]`. Copy the setup of
  `org_deleted_purges_the_audit_trail_as_otto_app`: two orgs with an app each, audit rows counted
  with `audit_rows`. Then, on `pool` (the owner role), run
  `CREATE SCHEMA otto_app`,
  `CREATE FUNCTION otto_app.purge_deleted_org_audit(uuid) RETURNS bigint LANGUAGE sql AS 'SELECT -1::bigint'`,
  and `GRANT USAGE ON SCHEMA otto_app TO otto_app`. Build `app_db = otto_app_db(&pool)`. Assert
  the decoy is live: `SELECT purge_deleted_org_audit($1)` on `app_db.pool()` returns `-1`. Apply
  the `org.deleted` event through `app_db`. Assert `detail["audit_events"] == rows`, that
  `audit_rows(org)` is 0, and that the other org's rows are unchanged. Close `app_db.pool()`.
  Give the test a doc comment saying what it guards (#53).
- [x] **Step 2: Run it and see it fail.**
  `DATABASE_URL=… cargo test -p flags-core --test db org_deleted_purges_through_public`. It fails
  because `audit_events` is `-1` and the rows remain.
- [x] **Step 3: Implement** in `audit_purge.rs`: a private `macro_rules! purge_function` returning
  `"public.purge_deleted_org_audit"`, defined above its uses.
  `pub const FUNCTION: &str = concat!(purge_function!(), "(uuid)");` keeps the existing doc.
  Add `pub(crate) const PURGE: &str = concat!("SELECT ", purge_function!(), "($1)");` with a doc
  comment saying it names the function by schema so `search_path` cannot resolve it elsewhere
  (#53). In `platform_events.rs`, replace the literal `"SELECT purge_deleted_org_audit($1)"` with
  `crate::audit_purge::PURGE`. Change nothing else.
- [x] **Step 4: Run the test and see it pass.** Then run the full gates (Global Constraints).
- [x] **Step 5: Commit**

```bash
git add crates/flags-core/src/audit_purge.rs crates/flags-core/src/platform_events.rs crates/flags-core/tests/db.rs
git commit -m "fix(core): call the audit purge function by schema" \
  -m "org.deleted called purge_deleted_org_audit unqualified, so search_path chose the function: a same-named function in a schema named after the tenant role would have run instead of the one the startup check vouches for in public. The call and audit_purge::FUNCTION now come from one qualified name." \
  -m "Fixes #53"
```
