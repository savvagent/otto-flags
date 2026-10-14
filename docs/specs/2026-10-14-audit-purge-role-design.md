# Audit-trail purge that works without an RLS-exempt pool role

**Status:** IMPLEMENTED (#39)
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
  ran the delete on `db.pool()`, outside the purge transaction. So under an `otto_app` pool the
  `org.deleted` delivery fails with `permission denied` after the tombstone is written. Keys and
  tokens are already refused at that point; the data stays until a retry, which fails the same way.
- Production's tenant transactions already run as `otto_app` (boot log: `tenant isolation
  enforced as role "otto_app"`), and the purge transaction is one of them. Moving the audit delete
  into it means production calls the function as `otto_app`, so the `EXECUTE` grant is
  load-bearing in production, not only in the test's `otto_app` pool.
- Running the delete outside the purge transaction also meant the audit trail and the rest of the
  org's data were not removed atomically. Calling the function inside the pinned purge
  transaction fixes that at no cost.
- `audit_events` is `FORCE ROW LEVEL SECURITY`, so even its owner is subject to its policies
  unless the owner is a superuser or has `BYPASSRLS`. A `SECURITY DEFINER` function owned by a
  non-exempt role would see no rows and purge nothing in silence.

## 3. Decisions and assumptions

- **Which role purges:** the migrating role (the role that owns the schema and ran the
  migrations), through a function it owns. Request code reaches the delete only by calling that
  function, never by holding DELETE on the trail.
- **The owner must be exempt from RLS,** as production's attach role (a superuser) and the test
  cluster's role are. The function checks this on every call and raises if its owner is neither a
  superuser nor `BYPASSRLS`. The delivery then fails and is retried visibly, instead of reporting
  success with the trail still there.
  - *Rejected in review:* an earlier draft also supported a non-exempt owner, through two
    `TO CURRENT_USER` policies gated on an `app.audit_purge` marker that the function set with a
    function-level `SET` clause. PR review (the test analyzer and the database reviewer, each
    reproducing it on PG 16) found that a non-superuser cannot create a function with a `SET`
    clause on an unregistered custom parameter. So the migration would fail to boot for exactly the
    role those policies were for, and no test here could exercise them. A runtime `set_config`
    marker would work, but it would add two permissive policies on the append-only trail for a
    configuration no deployment uses, which the test suite (which migrates as a superuser)
    could exercise only through hand-made policies. Failing loudly is the smaller, safer
    contract. Supporting a non-exempt owner, or reporting one at startup, is #40.
- **Migration `0004_audit_purge.sql`** (new, append-only), in one transaction:
  - `CREATE FUNCTION purge_deleted_org_audit(purge_org uuid) RETURNS bigint LANGUAGE plpgsql
    SECURITY DEFINER SET search_path = pg_catalog, public, pg_temp`. Its body:
    1. raises (SQLSTATE `55000`) unless `current_user` (the owner, inside a definer function) is
       a superuser or has `BYPASSRLS`;
    2. raises with SQLSTATE `42501` unless `purge_org` is the org the transaction is pinned to
       (`public.current_org()`);
    3. deletes `public.audit_events WHERE org_id = purge_org AND EXISTS (SELECT 1 FROM
       public.deleted_orgs WHERE org_id = purge_org)` and returns the row count.

    So the function can only remove the trail of the pinned org, and only once the platform has
    tombstoned it.
  - **Name resolution.** `pg_temp` is listed last and every table is schema-qualified. Without
    that, Postgres searches `pg_temp` first, and a caller (`otto_app` has `TEMP` on the database)
    could create a temporary `deleted_orgs` naming a live org. PL/pgSQL raises on a name that is
    both a column and a variable, so a column added later that matches the parameter's name cannot
    silently change what the body compares.
  - `REVOKE EXECUTE ON FUNCTION … FROM PUBLIC`; `GRANT EXECUTE … TO otto_app` inside the same
    `IF EXISTS (… otto_app)` guard the baseline uses for its grants.
  - No policy, table, or lock-taking statement. Creating a PL/pgSQL function does not lock the
    tables its body names, so no `lock_timeout` is needed.
- **Calling it:** `org_deleted` replaces the unpinned `DELETE` with
  `SELECT purge_deleted_org_audit($1)` on `tx.conn()`. It runs inside the pinned purge transaction,
  after `exclude_requests`, so the audit trail goes in the same transaction as everything else.
  The returned count goes into the `deleted` detail under `audit_events`, as before.
- **Existing policy kept:** `audit_events_retention` (`FOR DELETE USING (current_org() IS NULL)`)
  stays. The previous release's unpinned delete still uses it after a rollback, and it grants
  nothing to `otto_app`, which has no DELETE privilege. 0001's comment that the trail is deleted
  only unpinned is superseded; 0004's header says so.
- Every Run Profile value was read from the Development Workflow section; none assumed.

## 4. Goal and success criteria

`org.deleted` removes the org's whole audit trail when the purge runs as `otto_app`, without
`otto_app` gaining DELETE on `audit_events`.

- A DB test applies `org.deleted` through a `Db` whose every connection is `otto_app`
  (`otto_app_db`) and asserts:
  - the outcome is `Applied`, with `audit_events` in its detail equal to the org's audit row count
    before the purge;
  - the org's `audit_events` count is 0;
  - another org's audit rows are untouched, including after a redelivery, which is `Duplicate`.
- The existing superuser-pool purge test also asserts the org's audit rows are gone.
- As `otto_app`, in a transaction pinned to a live org, these hold:
  - the function returns 0 for that org;
  - a temporary `deleted_orgs` naming it does not change that;
  - it is refused (`42501`) for another, tombstoned org, whose rows survive;
  - a plain `DELETE FROM audit_events` is refused (`42501`).
- With the function owned by a role that is neither a superuser nor `BYPASSRLS`, a call raises.
- Every other existing test passes unchanged, including
  `tenant_isolation_is_enforced_on_every_tenant_table` (no new tenant table).

## 5. Scope

**In:** `crates/flags-core/migrations/0004_audit_purge.sql` (new),
`crates/flags-core/src/platform_events.rs` (`org_deleted`), `crates/flags-core/tests/db.rs`.

**Out:**
- The other purge statements, which are already pinned and `otto_app`-safe.
- Retention sweeps (#12, done).
- Supporting a migrating role that is not exempt from RLS (§3).
- Changing which role production's pool connects as.

## 6. Architecture

- The migration adds one function and its grants; no table, column, index, or policy.
- `org_deleted` keeps its order, with one step moved inside the transaction:
  1. tombstone on the pool;
  2. open the pinned transaction and take the lifecycle lock;
  3. purge the audit trail (moved inside the transaction);
  4. record the delivery;
  5. delete the tenant tables;
  6. commit.

  Because the tombstone is committed first, the function's `deleted_orgs` guard is satisfied, and
  the transaction is pinned to the org being purged, so the pinned-org check is too.
- No public Rust interface changes. The `Outcome::Applied` detail keeps its `audit_events` key.

## 7. Invariants impact

- **Append-only migrations:** a new file, `0004`; nothing existing changes.
- **Compatible with the previous release during rollout and after a rollback:** additive only (a
  function and a grant). The previous release still runs its unpinned `DELETE` on the exempt pool
  role, which nothing here restricts. `MIGRATOR` ignores `0004` on a rolled-back release.
- **Tenant isolation is RLS under `otto_app`, proven:** `otto_app` gains EXECUTE on one function
  that can delete only its pinned, tombstoned org's audit rows. It gains no table privilege or
  policy, and there is no new tenant table, so the isolation proof's table set is unchanged.
- **Request paths open transactions with `begin_live`:** `org_deleted` is the webhook path, which
  already uses `db.begin(org)` for the purge (unchanged; a deleted org cannot `begin_live`).
- **No SQL outside flags-core; runtime `sqlx::query` only:** holds.
- **Platform webhooks apply idempotently:** a redelivery finds no rows (returns 0) and is a
  `Duplicate`, as before.

## 8. Migration

`crates/flags-core/migrations/0004_audit_purge.sql`:

```sql
-- #21: the org.deleted purge removes a deleted org's audit trail through a
-- function owned by the migrating role, so it works when the purge runs as
-- otto_app, which has no DELETE on audit_events. This supersedes 0001's note
-- that the trail is deleted only unpinned; audit_events_retention stays for
-- the previous release, which still deletes that way.
--
-- The owner must be exempt from row-level security (production's attach role
-- is a superuser): audit_events is FORCE ROW LEVEL SECURITY, so any other owner
-- would see no rows and purge nothing. The function refuses to run then, so
-- the delivery fails and is retried visibly instead.
--
-- The guards are the tombstone in deleted_orgs and the pinned org, and both
-- can be set by the caller: otto_app can write deleted_orgs (the purge writes
-- it first) and app.org_id. So SQL running as otto_app could tombstone any org
-- and erase its trail. That needs arbitrary SQL, which inside a tenant
-- transaction can already RESET ROLE to the pool's own role; the guards stop
-- bugs, not injection, and a forged tombstone also locks the org out for good,
-- which is not silent.

CREATE FUNCTION purge_deleted_org_audit(purge_org uuid) RETURNS bigint
LANGUAGE plpgsql SECURITY DEFINER
-- pg_temp last: otherwise it is searched first, and a caller's temporary
-- tables could stand in for these. The body also names them by schema.
SET search_path = pg_catalog, public, pg_temp
AS $$
DECLARE
  purged bigint;
BEGIN
  IF NOT EXISTS (SELECT 1 FROM pg_catalog.pg_roles
                  WHERE rolname = current_user AND (rolsuper OR rolbypassrls)) THEN
    RAISE EXCEPTION 'purge_deleted_org_audit needs an owner exempt from row-level security'
      USING ERRCODE = 'object_not_in_prerequisite_state';
  END IF;
  IF purge_org IS DISTINCT FROM public.current_org() THEN
    RAISE EXCEPTION 'purge_deleted_org_audit only purges the org the transaction is pinned to'
      USING ERRCODE = 'insufficient_privilege';
  END IF;
  DELETE FROM public.audit_events
   WHERE org_id = purge_org
     AND EXISTS (SELECT 1 FROM public.deleted_orgs d WHERE d.org_id = purge_org);
  GET DIAGNOSTICS purged = ROW_COUNT;
  RETURN purged;
END
$$;

REVOKE EXECUTE ON FUNCTION purge_deleted_org_audit(uuid) FROM PUBLIC;

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

None beyond the migration, which the merge applies at boot (agent-owned, per the section). No new
secret, config key, or scope. No role change on `otto-db`: the function is created by the
migrating role in its own database, as every migration is. No recurring cost.

## 10. Error handling and edge cases

- **Redelivery of `org.deleted`:** the function deletes nothing and returns 0; the delivery is a
  `Duplicate`.
- **An org with no audit rows:** returns 0.
- **A failure inside the purge transaction** now also rolls back the audit delete; before, the
  audit rows could be gone while the rest of the purge rolled back. A retry redoes all of it.
- **Calling the function for a live org** (a bug, or a crafted call, including one with a temporary
  `deleted_orgs` in the caller's session): the guard reads `public.deleted_orgs` and returns 0.
- **Calling it for an org other than the pinned one:** refused with `42501`.
- **An owner that is not exempt from RLS:** the call raises `55000`, so the delivery fails, is
  logged, and the platform retries it.

## 11. Testing approach

In `crates/flags-core/tests/db.rs` with `#[sqlx::test(migrator = "flags_core::MIGRATOR")]`:

- `org_deleted_purges_the_audit_trail_as_otto_app`: two orgs with audit rows (`create_app`
  audits). `org.deleted` for one is applied through `otto_app_db(&pool)`. Assert:
  - the outcome is `Applied`, with `detail["audit_events"]` equal to the count before;
  - zero rows are left for that org;
  - the other org's rows are intact after the first delivery and after the `Duplicate`
    redelivery.

  Counts are taken on the superuser pool, and the `otto_app` pool is closed before returning.
- `audit_purge_only_reaches_deleted_orgs`: a live org and a tombstoned one, both with audit rows.
  As `otto_app`, in a transaction pinned to the live org:
  - the function returns 0;
  - after a temporary `deleted_orgs` naming the live org is created, it still returns 0;
  - inside a savepoint, a call for the tombstoned org is refused with `42501`;
  - a plain `DELETE FROM audit_events` is refused with `42501`.

  Both orgs' rows are unchanged afterwards. The temporary-table check guards the schema
  qualification; the `search_path` order is a second layer the test does not separate.
- `audit_purge_refuses_an_owner_subject_to_rls`: in one transaction on the superuser pool that
  is rolled back, the function is handed to a throwaway `NOSUPERUSER NOBYPASSRLS` role, and a
  call raises SQLSTATE `55000`. `CREATE ROLE` is transactional, so the rollback, or a panic,
  undoes both the role and the owner change.
- By hand, in a scratch database since dropped: 0001–0004 apply under a `NOSUPERUSER
  NOBYPASSRLS` migrating role (the review-round boot failure), and the purge then raises.
- `org_deleted_purges_everything_and_revokes_keys` additionally asserts the org's audit rows are
  gone on the superuser pool.
- Gates: test (with `DATABASE_URL`), clippy, fmt, the migrations guard (must print nothing: `0004`
  is an addition), and `podman build`.

## 12. Risks and open questions

- **Forged tombstone (accepted).** `otto_app` has INSERT on `deleted_orgs`, which the purge's own
  tombstone write needs when the pool runs as `otto_app`, and `current_org()` reads `app.org_id`,
  which any caller can set. So SQL running as `otto_app` could tombstone any org, pin to it, and
  call the purge, erasing an append-only trail the role cannot otherwise delete. This was raised
  by an automated commit review. The pinned-org check added in PR review limits what a bug can
  reach, not what injected SQL can. It is accepted because:
  - nothing in the database can tell a platform-verified tombstone from a forged one, and the AC
    requires the purge to work for an `otto_app` pool;
  - it needs arbitrary SQL, and arbitrary SQL inside a tenant transaction can already
    `RESET ROLE` to the pool's session role (production's superuser attach role), so `otto_app`
    is a guard against code bugs, not injection;
  - no plausible bug writes a tombstone for a live org and then calls the purge, and a forged
    tombstone permanently locks the org out (keys and tokens refused), which is not silent.

  The migration's header comment records the same reasoning.
- **Superuser-owned definer function.** In production the function's owner is the superuser
  attach role. The body is small, fully schema-qualified, takes one typed `uuid`, and has no
  dynamic SQL. Any later change to it (a new migration) must keep it that way. Moving ownership to
  a dedicated `BYPASSRLS` role is possible later; that is a human-only role change on `otto-db`.
- **Migrating role.** The function and its owner check are bound to the role that ran `0004`. If
  migrations ever run as a role that is not exempt from RLS, the purge raises until the owner
  changes. That is loud by design, and `docs/deploy/fly.md` records it for whoever changes the
  attach role. In that configuration a failing `org.deleted` also leaves the org's other data
  in place (the purge is one transaction), while the tombstone still refuses all access.
- `audit_purge_refuses_an_owner_subject_to_rls` creates its throwaway role inside a transaction
  it rolls back, so a failing run leaves no role behind.
