-- Unpinned retention sweeps must work for a role subject to RLS (otto_app).
--
-- The baseline's retention policies on these tables are FOR DELETE only.
-- Postgres also applies a table's SELECT policies to a DELETE whose WHERE
-- reads columns, and every sweep has such a WHERE (received_at, hour), so
-- under otto_app the sweeps matched no rows and deleted nothing. Production
-- hid this because its pool connects as an RLS-exempt role (#12).
--
-- Each policy adds only an unpinned read: current_org() IS NULL never holds
-- inside a pinned transaction, so no tenant sees anything new. Unpinned is
-- not enough on its own (#27): a request path that forgot to pin is unpinned
-- too, so each read also needs the transaction-local marker
-- app.maintenance = 'on', which only flags-core's background maintenance
-- helper (maintenance::begin) sets; the sweeps run inside it. Additive and
-- policy-only; no table, column, or data changes. audit_events is excluded on
-- purpose: it has no retention sweep and otto_app has DELETE revoked on it.
-- #21 covers purging an org's audit rows on org.deleted, not retention.

-- CREATE POLICY takes a lock that conflicts with every telemetry write on
-- these tables: fail fast rather than queue behind a long query. sqlx runs
-- each migration in its own transaction, so SET LOCAL ends with it.
SET LOCAL lock_timeout = '3s';

CREATE POLICY flag_errors_retention_read ON flag_errors
  FOR SELECT USING (current_org() IS NULL
                    AND current_setting('app.maintenance', true) = 'on');
CREATE POLICY flag_eval_hourly_retention_read ON flag_eval_hourly
  FOR SELECT USING (current_org() IS NULL
                    AND current_setting('app.maintenance', true) = 'on');
CREATE POLICY platform_events_retention_read ON platform_events
  FOR SELECT USING (current_org() IS NULL
                    AND current_setting('app.maintenance', true) = 'on');

-- 0001's DELETE-only retention policies (flag_errors_retention and siblings)
-- grant an unpinned DELETE with no marker, and a DELETE with no WHERE reads no
-- columns, so it needs no SELECT policy: an unmarked unpinned
-- `DELETE FROM flag_errors` would still empty the table across orgs (#27).
-- A RESTRICTIVE policy ANDs with those permissive ones, so an unpinned DELETE
-- now also needs the maintenance marker, WHERE clause or not. Pinned deletes
-- (the org.deleted purge's per-org loop, any per-org write) are unaffected:
-- current_org() is set. audit_events is left alone: otto_app has no DELETE
-- on it, and its unpinned purge delete is #21's.
CREATE POLICY flag_errors_maintenance_delete ON flag_errors
  AS RESTRICTIVE FOR DELETE
  USING (current_org() IS NOT NULL OR current_setting('app.maintenance', true) = 'on');
CREATE POLICY flag_eval_hourly_maintenance_delete ON flag_eval_hourly
  AS RESTRICTIVE FOR DELETE
  USING (current_org() IS NOT NULL OR current_setting('app.maintenance', true) = 'on');
CREATE POLICY platform_events_maintenance_delete ON platform_events
  AS RESTRICTIVE FOR DELETE
  USING (current_org() IS NOT NULL OR current_setting('app.maintenance', true) = 'on');
