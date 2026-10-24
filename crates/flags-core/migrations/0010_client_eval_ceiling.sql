-- Bound what a public client key can add to an org's bill (#143).
--
-- usage_meter_hourly.served_client is the part of `served` that was answered
-- to a public client key (`sdk_`). flags_core::metering bills at most a
-- per-app, per-hour ceiling of it when it closes the hour. usage_meter_daily
-- keeps what that closing did, per (org, app, UTC day) and for 90 days, so
-- `usage` can show a client-served spike the org did not cause and how much
-- of it the ceiling left unbilled.
--
-- Additive. The previous release inserts and updates usage_meter_hourly
-- without the new column (it takes its default 0, and the check holds
-- because that release only ever raises `served`) and never touches
-- usage_meter_daily. Rows it writes while it runs are closed, with a
-- client share of 0, by the release that knows the column.

SET LOCAL lock_timeout = '3s';

ALTER TABLE usage_meter_hourly
  ADD COLUMN served_client bigint NOT NULL DEFAULT 0
  CHECK (served_client >= 0 AND served_client <= served);

CREATE TABLE usage_meter_daily (
  org_id        uuid   NOT NULL,
  app_id        uuid   NOT NULL,
  day           date   NOT NULL,
  served_client bigint NOT NULL DEFAULT 0 CHECK (served_client >= 0),
  served_server bigint NOT NULL DEFAULT 0 CHECK (served_server >= 0),
  clipped       bigint NOT NULL DEFAULT 0 CHECK (clipped >= 0),
  PRIMARY KEY (org_id, app_id, day),
  FOREIGN KEY (org_id, app_id) REFERENCES flag_apps (org_id, id) ON DELETE CASCADE
);
CREATE INDEX usage_meter_daily_day_idx ON usage_meter_daily (day);

ALTER TABLE usage_meter_daily ENABLE ROW LEVEL SECURITY;
ALTER TABLE usage_meter_daily FORCE ROW LEVEL SECURITY;
CREATE POLICY usage_meter_daily_tenant_isolation ON usage_meter_daily
  USING (org_id = current_org() OR current_org() IS NULL)
  WITH CHECK (org_id = current_org() OR current_org() IS NULL);
CREATE POLICY usage_meter_daily_maintenance ON usage_meter_daily
  AS RESTRICTIVE FOR ALL
  USING (current_org() IS NOT NULL OR current_setting('app.maintenance', true) = 'on')
  WITH CHECK (current_org() IS NOT NULL OR current_setting('app.maintenance', true) = 'on');
-- No GRANT: 0001's ALTER DEFAULT PRIVILEGES gives otto_app its rights on new tables (as in 0008).
