# Audit-trail purge that works without an RLS-exempt pool role

**Status:** DRAFT
**Issue:** #21

## 1. Brief

From #21 (found while folding #12 into #11):

> The `org.deleted` purge deletes the org's audit trail unpinned, on the pool's own role
> (`crates/flags-core/src/platform_events.rs:181`):
>
> ```rust
> sqlx::query("DELETE FROM audit_events WHERE org_id = $1").execute(db.pool())
> ```
>
> `0001_baseline.sql` revokes `UPDATE, DELETE` on `audit_events` from `otto_app` and gives it only a
> DELETE-only `current_org() IS NULL` policy. So this delete works only while the pool connects as
> a role exempt from RLS (production's attach role is). Under any other pool role it either is
> refused (`otto_app`: permission denied, which fails the whole `org.deleted` delivery) or deletes
> nothing (a non-exempt role with DELETE: the `WHERE org_id = $1` needs a SELECT policy, the same
> shape as #12).
>
> **Impact today:** none. It is the same dependency on the pool role staying exempt that #12
> removes for the retention sweeps, but #12's fix (unpinned SELECT companion policies) does not
> apply here, because `otto_app` cannot delete from `audit_events` at all.
>
> **Acceptance criteria**
> - [ ] Decide which role the audit purge is meant to run as when the pool is not exempt, and make
>   the purge work under it without granting `otto_app` a general DELETE on the append-only trail
>   (for example a `SECURITY DEFINER` purge function owned by the migrating role).
> - [ ] A `crates/flags-core/tests/db.rs` test applies `org.deleted` on a `Db` whose pool runs as
>   `otto_app` and asserts the org's `audit_events` rows are gone.

## 2. Premise corrections

- The brief holds against the code: `org_deleted` (`crates/flags-core/src/platform_events.rs`)
  runs the delete on `db.pool()`, outside the purge transaction, so under an `otto_app` pool the
  `org.deleted` delivery fails with `permission denied` after the tombstone is written (keys and
  tokens are already refused; the data stays until a retry, which fails the same way).
- Production's tenant transactions already run as `otto_app` (boot log: `tenant isolation
  enforced as role "otto_app"`), and the purge transaction is one of them. Moving the audit delete
  into it means production will call the function as `otto_app`, so the `EXECUTE` grant is
  load-bearing in production, not only in the test's `otto_app` pool.
- Two further facts the fix relies on:
  - `audit_events` is `FORCE ROW LEVEL SECURITY`, so even its owner is subject to its policies
    unless the owner is a superuser or has `BYPASSRLS`. A `SECURITY DEFINER` function alone is
    therefore not enough when the migrating role is an ordinary role: it would run as the owner and
    see no rows. It needs a policy that lets the owner see and delete exactly the rows it may purge.
  - The delete runs outside the purge transaction today, so the audit trail and the rest of the
    org's data are not removed atomically. Calling the function inside the pinned purge transaction
    fixes that too, at no cost.

## 3. Decisions and assumptions

- **Which role purges:** the migrating role (the role that owns the schema and ran the
  migrations), through a function it owns. Request code reaches it only by calling that function,
  never by holding DELETE on the trail.
- **Migration `0004_audit_purge.sql`** (new, append-only), in one transaction:
  - `CREATE FUNCTION purge_deleted_org_audit(target uuid) RETURNS bigint LANGUAGE sql
    SECURITY DEFINER SET search_path = pg_catalog, public, pg_temp SET app.audit_purge = 'on'`
    whose body deletes `public.audit_events WHERE org_id = target AND EXISTS (SELECT 1 FROM
    public.deleted_orgs WHERE org_id = target)` and returns the row count. The guard means the
    function can only ever remove the trail of an org the platform has already deleted
    (tombstoned), whoever calls it. `pg_temp` is listed last and the tables are
    schema-qualified: without that, Postgres searches `pg_temp` first, and a caller (`otto_app`
    has `TEMP` on the database) could create a temporary `deleted_orgs` naming a live org and
    purge its trail as the owner.
  - `REVOKE EXECUTE ON FUNCTION … FROM PUBLIC`; `GRANT EXECUTE … TO otto_app` inside the same
    `IF EXISTS (… otto_app)` guard the baseline uses for its grants.
  - Two permissive policies scoped `TO CURRENT_USER` (the migrating role) on `audit_events`:
    `audit_events_purge_read FOR SELECT` and `audit_events_purge_delete FOR DELETE`, both
    `USING (current_setting('app.audit_purge', true) = 'on' AND org_id IN (SELECT org_id FROM
    public.deleted_orgs))`. They are no-ops for a superuser or `BYPASSRLS` owner (production's
    attach role) and are what make the function work for an ordinary owner (both are needed: the
    `WHERE` and `RETURNING` read rows). The `app.audit_purge` marker, set only by the function's
    own `SET` clause, matters in the deployment shape otto-tenant also accepts where `otto_app`
    cannot be created and tenant transactions run as the connecting (migrating) role: without it,
    every request transaction would match these policies and any org could read or delete
    tombstoned orgs' audit rows until the purge commits. With it, only code inside the function
    (or code that deliberately sets the marker, as with `app.maintenance`) matches. They grant
    nothing to `otto_app`. Inside the pinned purge transaction the existing
    `audit_events_tenant_isolation` policy already covers the reads (the pinned org is the
    target), so `audit_events_purge_read` matters only for an unpinned call.
  - `SET LOCAL lock_timeout = '3s'` first, as 0002 and 0003 do: `CREATE POLICY` takes an
    `ACCESS EXCLUSIVE` lock on `audit_events`, which every audited MCP write inserts into.
- **Calling it:** `org_deleted` replaces the unpinned `DELETE` with
  `SELECT purge_deleted_org_audit($1)` on `tx.conn()`, inside the pinned purge transaction and
  after `exclude_requests`, so the audit trail goes in the same transaction as everything else.
  The returned count goes into the `deleted` detail under `audit_events`, as now.
- **Existing policy kept:** `audit_events_retention` (`FOR DELETE USING (current_org() IS NULL)`)
  stays; dropping it would be a destructive change the previous release might rely on, and it
  grants nothing to `otto_app`, which has no DELETE privilege.
- **`search_path` pinned** on the function, as `SECURITY DEFINER` functions must be, so a caller
  cannot shadow `audit_events` or `deleted_orgs` with objects in another schema.
- **Not tested:** an ordinary (non-superuser, non-`BYPASSRLS`) migrating role. The test cluster's
  migrating role is a superuser, as production's is; the `TO CURRENT_USER` policies are reasoned,
  not exercised. Recorded under Risks.
- Every Run Profile value was read from the Development Workflow section; none assumed.

## 4. Goal and success criteria

`org.deleted` removes the org's whole audit trail whether the pool is exempt from RLS or runs as
`otto_app`, without `otto_app` gaining DELETE on `audit_events`.

- A new DB test applies `org.deleted` through a `Db` whose every connection is `otto_app`
  (`otto_app_db`) and asserts: the outcome is `Applied` with `audit_events` > 0 in its detail; the
  org's `audit_events` count is 0; another org's audit rows are untouched.
- Under the same `otto_app` pool, `purge_deleted_org_audit` on an org that is *not* in
  `deleted_orgs` deletes nothing and returns 0, and a plain `DELETE FROM audit_events` is still
  refused (`permission denied`).
- `org_deleted_purges_everything_and_revokes_keys` and every other existing test pass unchanged.
- `tenant_isolation_is_enforced_on_every_tenant_table` passes (no new tenant table).

## 5. Scope

**In:** `crates/flags-core/migrations/0004_audit_purge.sql` (new),
`crates/flags-core/src/platform_events.rs` (`org_deleted`), `crates/flags-core/tests/db.rs`.

**Out:** the other purge statements (already pinned and `otto_app`-safe); retention sweeps (#12,
done); changing which role production's pool connects as (production stays on its attach role;
this only removes the dependency).

## 6. Architecture

- The migration adds one function and two policies; no table, column, or index.
- `org_deleted` keeps its order: tombstone on the pool, open the pinned transaction, take the
  lifecycle lock, then (moved inside the transaction) purge the audit trail, record the delivery,
  delete the tenant tables, commit. Because the tombstone is committed first, the function's
  `deleted_orgs` guard is satisfied.
- No public Rust interface changes; the `Outcome::Applied` detail keeps its `audit_events` key.

## 7. Invariants impact

- **Append-only migrations:** a new file, `0004`; nothing existing changes.
- **Compatible with the previous release during rollout and after a rollback:** additive only (a
  function, two policies scoped to the migrating role, a grant). The previous release still runs
  its unpinned `DELETE` on the exempt pool role, which the new policies do not restrict
  (permissive policies only add access). `MIGRATOR` ignores `0004` on a rolled-back release.
- **Tenant isolation is RLS under `otto_app`, proven:** `otto_app` gains EXECUTE on one function
  that can only delete tombstoned orgs' audit rows; it gains no table privilege or policy. No new
  tenant table, so the isolation proof's table set is unchanged.
- **Request paths open transactions with `begin_live`:** `org_deleted` is the webhook path, which
  already uses `db.begin(org)` for the purge (unchanged; a deleted org cannot `begin_live`).
- **No SQL outside flags-core; runtime `sqlx::query` only:** holds.
- **Platform webhooks apply idempotently:** a redelivery finds no rows (returns 0) and is a
  `Duplicate`, as now.

## 8. Migration

`crates/flags-core/migrations/0004_audit_purge.sql`:

```sql
-- #21: the org.deleted purge removes a deleted org's audit trail through a
-- function owned by the migrating role, so it works when the pool runs as
-- otto_app (which has no DELETE on audit_events) and when the migrating role
-- is not exempt from RLS.

SET LOCAL lock_timeout = '3s';

CREATE FUNCTION purge_deleted_org_audit(target uuid) RETURNS bigint
LANGUAGE sql SECURITY DEFINER
-- pg_temp last: otherwise it is searched first, and a caller's temporary
-- deleted_orgs could stand in for the real one.
SET search_path = pg_catalog, public, pg_temp
SET app.audit_purge = 'on'
AS $$
  WITH gone AS (
    DELETE FROM public.audit_events
     WHERE org_id = target
       AND EXISTS (SELECT 1 FROM public.deleted_orgs d WHERE d.org_id = target)
    RETURNING 1
  )
  SELECT count(*) FROM gone
$$;

REVOKE EXECUTE ON FUNCTION purge_deleted_org_audit(uuid) FROM PUBLIC;

-- audit_events is FORCE ROW LEVEL SECURITY, so an owner that is not a
-- superuser or BYPASSRLS needs these to read (WHERE, RETURNING) and delete
-- what it may purge. No-ops for an exempt owner; nothing here reaches
-- otto_app. The marker, set only by the function above, keeps them from
-- matching ordinary transactions when tenant transactions run as this role.
CREATE POLICY audit_events_purge_read ON audit_events
  FOR SELECT TO CURRENT_USER
  USING (current_setting('app.audit_purge', true) = 'on'
         AND org_id IN (SELECT org_id FROM public.deleted_orgs));
CREATE POLICY audit_events_purge_delete ON audit_events
  FOR DELETE TO CURRENT_USER
  USING (current_setting('app.audit_purge', true) = 'on'
         AND org_id IN (SELECT org_id FROM public.deleted_orgs));

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'otto_app') THEN
    EXECUTE 'GRANT EXECUTE ON FUNCTION purge_deleted_org_audit(uuid) TO otto_app';
  END IF;
END $$;
```

Compatible with the running release (see §7). Applied at boot under the advisory lock; the boot
log shows `applying migrations`.

## 9. Out-of-band

None beyond the migration, which the agent's merge applies at boot (agent-owned, per the
section). No new secret, config key, or scope; no role change on `otto-db` (the function and
policies are created by the migrating role in its own database, as every migration is). No
recurring cost.

## 10. Error handling and edge cases

- Redelivery of `org.deleted`: the function deletes nothing and returns 0; the delivery is a
  `Duplicate` (unchanged).
- An `org.deleted` for an org with no audit rows: returns 0.
- A failure inside the purge transaction now also rolls back the audit delete (before, the audit
  rows could be gone while the rest of the purge rolled back); a retry redoes all of it.
- Calling the function for a live org (a bug, or a crafted call, including one with a temporary
  `deleted_orgs` in the caller's session): the guard reads `public.deleted_orgs` and makes it a
  no-op.

## 11. Testing approach

In `crates/flags-core/tests/db.rs` with `#[sqlx::test(migrator = "flags_core::MIGRATOR")]`:

- `org_deleted_purges_the_audit_trail_as_otto_app`: two orgs with audit rows (from `create_app`,
  which audits), `org.deleted` for one applied through `otto_app_db(&pool)`; assert `Applied`,
  `detail["audit_events"] > 0`, zero rows left for that org, the other org's rows intact (counted
  on the superuser pool). Close the `otto_app` pool before returning, as `otto_app_db`'s doc says.
- `audit_purge_only_reaches_deleted_orgs`: as `otto_app` on a live org, `SELECT
  purge_deleted_org_audit($1)` returns 0 and deletes nothing; after `CREATE TEMP TABLE
  deleted_orgs (org_id uuid)` holding that live org, the call still returns 0 and the rows remain;
  `DELETE FROM audit_events WHERE org_id = $1` fails with SQLSTATE `42501`. Run inside
  `db.begin(live_org)` on the `otto_app` `Db`, as production calls it (one connection: the temp
  table is per session). After the call, `current_setting('app.audit_purge', true)` in the same
  transaction is not `'on'`, pinning that the marker exists only during the function.
- Existing `org_deleted_purges_everything_and_revokes_keys` and the isolation proof run unchanged.
- Gates: test (with `DATABASE_URL`), clippy, fmt, the migrations guard (must print nothing: `0004`
  is an addition), and `podman build`.

## 12. Risks and open questions

- The `TO CURRENT_USER` policies are not exercised by a test (the test cluster's migrating role is
  a superuser). If production's migrating role ever stopped being exempt, the purge depends on
  them; their logic is two lines and reviewed.
- In the no-`otto_app` deployment shape, code that sets `app.audit_purge` itself could read
  tombstoned orgs' audit rows; nothing does, and the same is true of `app.maintenance` today.
- `CURRENT_USER` is resolved when the migration runs; if migrations later run as a different role,
  that role would need the same policies (a new migration).
