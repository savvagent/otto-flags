-- Unpinned retention sweeps must work for a role subject to RLS (otto_app).
--
-- The baseline's retention policies on these tables are FOR DELETE only.
-- Postgres also applies a table's SELECT policies to a DELETE whose WHERE
-- reads columns, and every sweep has such a WHERE (received_at, hour), so
-- under otto_app the sweeps matched no rows and deleted nothing. Production
-- hid this because its pool connects as an RLS-exempt role (#12).
--
-- Each policy adds only an unpinned read: current_org() IS NULL never holds
-- inside a pinned transaction, so no tenant sees anything new. Additive and
-- policy-only; no table, column, or data changes. audit_events is excluded on
-- purpose (otto_app has DELETE revoked on it; tracked as #21).
CREATE POLICY flag_errors_retention_read ON flag_errors
  FOR SELECT USING (current_org() IS NULL);
CREATE POLICY flag_eval_hourly_retention_read ON flag_eval_hourly
  FOR SELECT USING (current_org() IS NULL);
CREATE POLICY platform_events_retention_read ON platform_events
  FOR SELECT USING (current_org() IS NULL);
