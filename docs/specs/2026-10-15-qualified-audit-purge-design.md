# Call the audit purge function by schema

**Status:** DRAFT
**Issue:** #53

## 1. Brief

Found by the security review of #52 (#40). `org_deleted` (`crates/flags-core/src/platform_events.rs`)
calls the audit purge unqualified, `SELECT purge_deleted_org_audit($1)`, so Postgres resolves it
through the connection's `search_path` (by default `"$user", public`). The startup check added in
#52 (`flags_core::audit_purge`) inspects `public.purge_deleted_org_audit(uuid)` explicitly. If a
schema named after the tenant role (for example `otto_app`) ever held a function of the same name,
the purge would call that one while the check vouched for the one in `public`. Exploiting it needs
the right to create a schema, which already defeats the guards, so this is hardening, not a live
hole.

Acceptance criteria (#53):

- The purge call names the function by schema (`public.purge_deleted_org_audit($1)`), ideally
  built from `flags_core::audit_purge::FUNCTION`'s name so the call and the check cannot drift.
- Existing purge tests pass unchanged; no migration.

## 2. Premise corrections

- `FUNCTION` is a signature (`public.purge_deleted_org_audit(uuid)`), not a name, and Rust cannot
  `concat!` a `const` or slice one at compile time on stable. "Built from `FUNCTION`'s name"
  therefore means one source for the qualified name from which both `FUNCTION` and the call are
  built, rather than deriving the call from `FUNCTION` itself (section 6).

## 3. Assumptions

- **A private `macro_rules!` holding the qualified name** feeds `concat!` for both strings, so they
  are `&'static str` constants that cannot drift. Rejected: `format!` at each call (a runtime
  allocation, and the string still has to be spelled somewhere), and the `const_format` crate (a
  new dependency for two strings).
- **The operator-facing messages in `audit_purge.rs`** (`PurgeReport::problems`, `summary`), which
  say `purge_deleted_org_audit(uuid)` unqualified, are left as they are. They are text for a
  human, not resolution, and changing them changes test-visible strings for no safety gain.
- **The tests in `crates/flags-core/tests/db.rs`** that call the function unqualified are left
  as they are (the AC says "pass unchanged"). They run as the test pool role or as `otto_app` in
  a database where no decoy schema exists.
- All Run Profile values come from the CLAUDE.md section; none are assumed.

## 4. Goal and success criteria

`org.deleted` always runs the function the startup check vouched for, whatever schemas exist on
the connection's `search_path`.

- `platform_events::org_deleted` executes `SELECT public.purge_deleted_org_audit($1)`.
- The call string and `audit_purge::FUNCTION` both come from a single definition of the
  qualified name.
- A new DB test fails on `origin/main` and passes after the change: with a decoy
  `otto_app.purge_deleted_org_audit(uuid)` visible to `otto_app`, an `org.deleted` delivery still
  purges the audit trail through `public`.
- Every existing test passes unchanged. No migration.

## 5. Scope

In: the call in `platform_events.rs`, the constants in `audit_purge.rs`, and one DB test.

Out: the operator-facing message text; the unqualified calls in existing tests; other unqualified
SQL (the tables are already qualified where it matters, and the function body qualifies its own
references, #21); pinning `search_path` per connection or per function.

## 6. Architecture

`crates/flags-core/src/audit_purge.rs`:

```rust
macro_rules! purge_function {
    () => {
        "public.purge_deleted_org_audit"
    };
}

pub const FUNCTION: &str = concat!(purge_function!(), "(uuid)");

/// The statement `org.deleted` purges through: [`FUNCTION`], named by schema
/// so `search_path` cannot resolve it elsewhere (#53).
pub(crate) const PURGE: &str = concat!("SELECT ", purge_function!(), "($1)");
```

`FUNCTION`'s value is unchanged (`public.purge_deleted_org_audit(uuid)`), so `check`/`inspect`
behave exactly as before. `PURGE` is `pub(crate)`: only `platform_events` uses it, so the public
interface gains nothing.

`crates/flags-core/src/platform_events.rs` (`org_deleted`, the purge call at about line 191):
`sqlx::query_scalar::<_, i64>(audit_purge::PURGE)`, which replaces the literal. No other line
changes, so #80's edits in this file do not conflict.

## 7. Invariants impact

- No SQL outside `flags-core`, runtime queries only: holds (the constant is in `flags-core` and
  passed to runtime `query_scalar`).
- Webhooks apply idempotently: unchanged.
- Tenant isolation, RLS under `otto_app`: unchanged. The call runs in the same pinned transaction
  as the same role.
- Append-only migrations: no migration.

## 8. Migration

None.

## 9. Out-of-band

None. No config, secret, scope, or platform registration.

## 10. Error handling and edge cases

- The function is missing from `public`: the startup check already refuses to serve. At call time
  the qualified call fails just as the unqualified one did (`42883`).
- A deployment where `public` is not where 0004 created the function: impossible, because 0004
  creates it in `public` explicitly, and `FUNCTION` already assumed that.

## 11. Testing approach

New `#[sqlx::test(migrator = "flags_core::MIGRATOR")]` in `crates/flags-core/tests/db.rs`,
`org_deleted_purges_through_public_whatever_the_search_path`:

1. Create apps in two orgs so both have audit rows.
2. As the pool role: `CREATE SCHEMA otto_app`, then
   `CREATE FUNCTION otto_app.purge_deleted_org_audit(uuid) RETURNS bigint LANGUAGE sql AS 'SELECT -1::bigint'`
   and `GRANT USAGE ON SCHEMA otto_app TO otto_app`. Functions are executable by `PUBLIC` by
   default. The test asserts that `otto_app` resolves the unqualified name to the decoy, so the
   test cannot pass vacuously.
3. Apply `org.deleted` through `otto_app_db`. Assert `detail["audit_events"]` equals the org's
   row count, its audit rows are gone, and the other org's rows remain.

Gates: test with `DATABASE_URL` against the dedicated port, clippy, fmt, the migrations extra
check, and `podman build` (`crates/**`).

## 12. Risks and open questions

- #80 also edits `platform_events.rs`. This change touches one line there plus an import.
