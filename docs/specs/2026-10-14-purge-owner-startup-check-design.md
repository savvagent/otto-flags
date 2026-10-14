# Refuse to serve when the audit purge cannot run

**Status:** IMPLEMENTED (#52)
**Issue:** #40

## 1. Brief

From #40 (follow-up to #21, PR #39):

> Migration `0004` makes the migrating role the owner of `purge_deleted_org_audit`, the
> `SECURITY DEFINER` function the `org.deleted` purge uses to remove a deleted org's append-only
> audit trail. `audit_events` is `FORCE ROW LEVEL SECURITY`, so the function only works when its
> owner is a superuser or has `BYPASSRLS`. Otherwise it refuses with SQLSTATE `55000`.
>
> Production's attach role is a superuser, so nothing is affected today. But
> `Db::verify_tenant_isolation` also accepts a setup where `otto_app` cannot be created and
> tenant transactions run as a migrating role that is not exempt from RLS. In that setup:
>
> - every `org.deleted` delivery now fails;
> - the org's other data stays in place, because the purge is one transaction;
> - the tombstone still refuses all access.
>
> Before #39 that setup deleted the other tables and silently kept the audit rows.
> `docs/deploy/fly.md` documents the precondition for whoever changes the attach role (it plans
> to move to a dedicated instance).
>
> **Acceptance criteria**
> - [ ] A non-exempt owner is reported at startup, for example as a check next to the
>   tenant-isolation proof that names `purge_deleted_org_audit`'s owner and its exemption, rather
>   than on the first `org.deleted`.
> - [ ] Decide whether a non-exempt owner should be supported. If it should, use a transaction-local
>   marker set with `set_config` inside the function plus policies scoped to the owner, and add a
>   test that migrates and purges as a `NOSUPERUSER NOBYPASSRLS` owner. A function-level `SET`
>   clause will not work: a non-superuser cannot create one on a custom parameter, which is why
>   #39 dropped that design. If it should not, the startup check refuses to serve instead.

## 2. Premise corrections

None. The code matches the brief: `0004_audit_purge.sql` raises `55000` when
`current_user` (the owner, inside the `SECURITY DEFINER` body) is neither `rolsuper` nor
`rolbypassrls`; `org_deleted` (`crates/flags-core/src/platform_events.rs`) calls it inside the
pinned purge transaction; `crates/flags-server/src/main.rs` runs `verify_tenant_isolation` after
migrating and before binding, and nothing checks the function.

## 3. Decision: a non-exempt owner is not supported

The startup check refuses to serve. Reasons:

- **Nobody runs that shape.** Production's migrating role is the superuser attach role. The one
  planned change (a dedicated instance) is a deployment Rob controls, where giving the migrating
  role `BYPASSRLS`, or handing the function to a role that has it, is one statement
  (`docs/deploy/fly.md` already says so).
- **Supporting it costs a migration on the audit trail's security boundary.** The marker design
  needs new `audit_events` policies scoped to the owner. In the shape that needs it, the owner is
  also the role tenant transactions run as (no `otto_app` to drop into), so any tenant SQL could
  set the marker; the guard would rest on the function body alone. That is a new, subtle policy
  on the one append-only table, plus a new invariant to test, for a deployment that does not
  exist. A migration also has to stay correct under the previous release (rollback rule);
  a code-only check has no such constraint.
- **Fail loud at boot beats fail on the first `org.deleted`.** Refusing at startup makes a
  misconfigured deploy crash-loop, so Fly rolls it back and the operator sees the reason in the
  boot log, instead of a purge that fails days later on a customer's deletion.
- **Refusing rather than warning** matches the AC ("If it should not, the startup check refuses
  to serve instead") and the tenant-isolation proof next to it: a deployment that cannot honour
  `org.deleted` should not take customer data.

**What this narrows.** `otto_tenant`'s isolation proof accepts a second shape: `otto_app` cannot
be assumed and the connecting role is neither a superuser nor `BYPASSRLS` (its docs call it the
managed-Postgres case). There, migrations run as the connecting role, which then owns the function
and is also the role tenant transactions run as. After this change that shape does not boot unless
the function belongs to a *separate* exempt role: either reassign it
(`ALTER FUNCTION purge_deleted_org_audit(uuid) OWNER TO <exempt role>`), or run migrations as an
exempt role and serve with `FLAGS_RUN_MIGRATIONS=false`. Giving the connecting role `BYPASSRLS`
is not a fix there: the isolation proof would then refuse it. On a managed Postgres that offers no superuser or `BYPASSRLS` role at all (and where a
non-superuser can only hand a function to a role it belongs to), that shape is unsupported
outright. Before #39 that shape served but
kept every deleted org's audit trail; since #39 it serves but fails every `org.deleted`. Refusing
to start is the honest version of both.

If a real deployment ever needs a non-exempt owner, #40's marker design is the route; that would
be a new issue.

## 4. Assumptions

- **The check also covers the other ways the purge call fails for a fixed reason:** the function
  is missing (the schema predates `0004`, possible with `FLAGS_RUN_MIGRATIONS=false`), or the role
  tenant transactions run as lacks `EXECUTE` on it (`0004` grants it to `otto_app` only if that
  role existed when the migration ran). Each would also fail every `org.deleted`; reading them is
  two more columns of the same catalog query. This is the same AC ("reported at startup ... rather
  than on the first `org.deleted`") applied to its neighbours, not new behaviour.
- **The role checked for `EXECUTE` is the one a tenant transaction runs as**, found by opening a
  transaction exactly as `org_deleted` does (`Db::begin`, which drops into `otto_app` when it can)
  and rolling it back. It is not a request path, so `begin_live` does not apply; the org is a fresh
  random id and nothing is written.
- **The owner's exemption is read from the owner role's own attributes** (`rolsuper`,
  `rolbypassrls`), the same test the function body applies. `BYPASSRLS` is not inherited through
  membership, so this is exact. It is deliberately not the isolation proof's membership-aware
  ownership test: what matters is what the function body will see as `current_user`.
- **The judgement is a pure `problems()` method** on a facts struct, like
  `otto_tenant::IsolationReport`, so each failing state is unit-testable without a database, and
  `main.rs` turns problems into the refusal.
- `CLAUDE.md` is not edited by this change: its verify step's boot-log list should gain the new
  `audit purge ready` line, which is proposed to Rob in the run's report rather than made here.
- Every Run Profile value was read from the Development Workflow section; none assumed.

## 5. Goal and success criteria

A deployment where the `org.deleted` purge could not remove an org's audit trail refuses to start,
saying why, before binding a port.

- With the function owned by a superuser or a `BYPASSRLS` role, executable by the tenant role,
  the server boots and logs one info line naming the owner, its exemption, and the tenant role.
- With an owner that is neither, the check reports a problem naming the owner and the fix.
- With the function missing, or not executable by the tenant role, the check reports a problem
  naming the role and the fix.
- `flags-server` exits with the problems joined into its error before binding, after the
  tenant-isolation proof.
- Nothing else changes: no migration, no config key, the purge itself untouched.

## 6. Scope

**In:** a new `crates/flags-core/src/audit_purge.rs` (facts, judgement, catalog read); the call
in `crates/flags-server/src/main.rs`; tests in that module and `crates/flags-core/tests/db.rs`;
the precondition paragraph in `docs/deploy/fly.md`.

**Out:** supporting a non-exempt owner (section 3); changing `0004` or the purge; `otto-tenant`'s
isolation proof; any production role change.

## 7. Architecture

`flags_core::audit_purge`:

```rust
pub const FUNCTION: &str = "public.purge_deleted_org_audit(uuid)";

pub struct PurgeReport {
    pub owner: Option<String>,      // None: the function does not exist
    pub owner_is_superuser: bool,
    pub owner_bypasses_rls: bool,
    pub tenant_role: String,        // current_user in a tenant-shaped transaction
    pub executable: bool,           // tenant_role holds EXECUTE
}
impl PurgeReport {
    pub fn problems(&self) -> Vec<String>;  // empty = the purge can run
    pub fn summary(&self) -> String;        // the info line
}
/// Reads the facts as the connection's current role. Public so a test can
/// put the catalog into a failing state inside a transaction it rolls back.
/// Owner columns decode as `Option<_>`: they are NULL when the function is missing.
pub async fn inspect(conn: &mut PgConnection) -> Result<PurgeReport>;
/// `db.begin(OrgId::new())`, `inspect`, rollback.
pub async fn check(db: &Db) -> Result<PurgeReport>;
```

One query, the function's signature bound as `$1` from `FUNCTION` so the two cannot drift:

```sql
SELECT r.rolname, r.rolsuper, r.rolbypassrls, current_user::text,
       COALESCE(has_function_privilege(p.oid, 'EXECUTE'), false)
  FROM (SELECT to_regprocedure($1) AS oid) p
  LEFT JOIN pg_catalog.pg_proc f ON f.oid = p.oid
  LEFT JOIN pg_catalog.pg_roles r ON r.oid = f.proowner
```

`problems()`, in operator order: missing function ("run migrations"); owner neither superuser nor
`BYPASSRLS` (names the owner, says every `org.deleted` would fail with `55000`, and gives the
fixes: `ALTER FUNCTION purge_deleted_org_audit(uuid) OWNER TO <exempt role>` or migrating as an
exempt role with `FLAGS_RUN_MIGRATIONS=false`; and, only when the owner is not `tenant_role`,
`ALTER ROLE <owner> BYPASSRLS`, which the isolation proof would refuse when the owner is the
tenant role); tenant role lacks
`EXECUTE` (`GRANT EXECUTE ON FUNCTION purge_deleted_org_audit(uuid) TO <role>`). Role names are
printed with `{:?}`; they come from our own catalog, not a caller.

`main.rs`, right after the isolation proof:

```rust
let purge = flags_core::audit_purge::check(&db).await
    .context("refusing to serve: could not read the audit purge function from the catalog")?;
let problems = purge.problems();
if !problems.is_empty() {
    anyhow::bail!("refusing to serve: an org.deleted could not purge the org's audit trail: {}",
                  problems.join(" | "));
}
tracing::info!("{}", purge.summary());
```

Summary, e.g.: `audit purge ready: purge_deleted_org_audit(uuid) is owned by "flags" (superuser) and
executable by "otto_app"`.

No public interface changes beyond the new crate module (additive). No wire, tool, route, config,
scope, or schema change.

## 8. Invariants impact

- **Append-only migrations / rollback compatibility:** no migration. The previous release does not
  run the check and is unaffected; a rollback to it is unchanged.
- **Tenant isolation:** the check runs after the isolation proof, in a tenant-shaped transaction
  that reads only catalogs and is rolled back.
- **No SQL outside `flags-core`, runtime queries only:** the query lives in `flags-core` and uses
  `sqlx::query_as`.
- **Request paths use `begin_live`:** this is startup, not a request path; `Db::begin` matches the
  purge's own transaction, which is what the check must reproduce.
- **Database error text never reaches a tool response:** the check's errors go only to the boot
  log via `anyhow`, like the isolation proof.
- **Small shared database:** one query at boot, one connection, released at once.

## 9. Migration

None.

## 10. Out-of-band

None. Production's owner is the superuser attach role and `otto_app` existed before `0004` ran
(the boot log already shows `tenant isolation enforced as role "otto_app"`), so the check is
expected to pass. If it does not, the new machine fails to boot, Fly rolls back, and the fix is a
human-only change on `otto-db`, handed to Rob with the exact statement from the boot log (the
`ALTER FUNCTION … OWNER TO` or `GRANT EXECUTE` form).

## 11. Error handling and edge cases

- Catalog read fails (connection): the server refuses to start with a context line, as for the
  isolation proof; Fly restarts it.
- Function missing: one problem; the owner, exemption, and `EXECUTE` problems are not also
  reported (nothing to judge; `executable` is false then).
- Owner exempt but tenant role lacks `EXECUTE`: one problem.
- Both owner non-exempt and no `EXECUTE`: both, owner first.
- A superuser tenant role: `has_function_privilege` is true; the isolation proof has already
  refused that shape.

## 12. Testing approach

- Unit tests in `audit_purge.rs`: `problems()` empty for a superuser owner and for a `BYPASSRLS`
  owner; one problem naming the owner for neither, offering `ALTER ROLE … BYPASSRLS` only when the
  owner differs from the tenant role (a case where they are equal must not offer it); the missing-function problem alone; the
  `EXECUTE` problem naming the role; `summary()` names owner, exemption, and role.
- DB tests in `crates/flags-core/tests/db.rs` (`#[sqlx::test(migrator = "flags_core::MIGRATOR")]`):
  - `check` on a freshly migrated database: no problems, tenant role `otto_app`, owner a superuser.
  - In one rolled-back transaction each (`CREATE ROLE`, `REVOKE`, `DROP` are transactional):
    owner handed to a `NOSUPERUSER NOBYPASSRLS` role, then `SET LOCAL ROLE otto_app` and
    `inspect` reports the owner problem naming that role; owner handed to a `NOSUPERUSER BYPASSRLS`
    role passes (the ACL survives the owner change, so this exercises only the exemption branch);
    `EXECUTE` revoked from `otto_app` reports the `EXECUTE` problem; the function
    dropped reports the missing problem.
- `main.rs` is wiring only; the server's existing e2e tests still pass.
- Gates: test, lint, format, extra (migrations), and the `podman build` gate (`crates/**`).

## 13. Risks and open questions

- If production's `otto_app` somehow lacks `EXECUTE`, the deploy fails and rolls back. That
  would be a real, previously silent fault (every `org.deleted` failing), surfaced as intended;
  the remedy is human-only.
- The check proves the purge's preconditions, not that a purge succeeds; the existing DB tests
  cover the purge itself.
