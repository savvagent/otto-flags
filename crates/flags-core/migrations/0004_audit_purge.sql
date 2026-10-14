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
-- The guard is the tombstone in deleted_orgs, which otto_app can write (the
-- purge writes it first), and the pinned org. So SQL running as otto_app could
-- tombstone its own pinned org and erase that trail. That needs arbitrary SQL,
-- which inside a tenant transaction can already RESET ROLE to the pool's own
-- role; the guard stops bugs, not injection, and a forged tombstone also locks
-- the org out for good, which is not silent.

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
    RAISE EXCEPTION 'purge_deleted_org_audit needs an owner exempt from row-level security';
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
