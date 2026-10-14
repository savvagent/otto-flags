-- #21: the org.deleted purge removes a deleted org's audit trail through a
-- function owned by the migrating role, so it works when the pool runs as
-- otto_app (which has no DELETE on audit_events) and when the migrating role
-- is not exempt from RLS.
--
-- The guard is the tombstone in deleted_orgs, which otto_app can write (the
-- purge writes it before calling this). So SQL running as otto_app could
-- tombstone a live org and then erase its trail. That needs arbitrary SQL, which
-- inside a tenant transaction can already RESET ROLE to the pool's own role;
-- the guard stops bugs, not injection, and a forged tombstone also locks the
-- org out for good, which is not silent.

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
