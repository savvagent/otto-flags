-- Meter production evaluations (#140): the hourly evaluation counter.
--
-- usage_meter_hourly counts SDK evaluations per (org, app, hour) in
-- served/reported columns. flags_core::metering flushes in-memory counts into
-- it and closes each finished hour into a metered usage_outbox event, billed as
-- the larger of the two (telemetry also reports server-answered evaluations).
-- Rows live about an hour past their hour, then are deleted.
--
-- Additive: the previous release never touches the table. Rows written before
-- a rollback wait here until a release that closes them runs again. It is
-- tenant-isolated like the other unpinned tables: RLS enabled and forced, with
-- unpinned access needing the maintenance marker (0003, 0005).

SET LOCAL lock_timeout = '3s';

CREATE TABLE usage_meter_hourly (
  org_id   uuid        NOT NULL,
  app_id   uuid        NOT NULL,
  hour     timestamptz NOT NULL,
  served   bigint      NOT NULL DEFAULT 0 CHECK (served >= 0),
  reported bigint      NOT NULL DEFAULT 0 CHECK (reported >= 0),
  PRIMARY KEY (org_id, app_id, hour),
  FOREIGN KEY (org_id, app_id) REFERENCES flag_apps (org_id, id) ON DELETE CASCADE
) WITH (fillfactor = 70);
-- Each row is updated by every flush of its hour; free space on the page keeps
-- those updates HOT (no index changes), since no indexed column ever changes.
CREATE INDEX usage_meter_hourly_hour_idx ON usage_meter_hourly (hour);

ALTER TABLE usage_meter_hourly ENABLE ROW LEVEL SECURITY;
ALTER TABLE usage_meter_hourly FORCE ROW LEVEL SECURITY;
CREATE POLICY usage_meter_hourly_tenant_isolation ON usage_meter_hourly
  USING (org_id = current_org() OR current_org() IS NULL)
  WITH CHECK (org_id = current_org() OR current_org() IS NULL);
CREATE POLICY usage_meter_hourly_maintenance ON usage_meter_hourly
  AS RESTRICTIVE FOR ALL
  USING (current_org() IS NOT NULL OR current_setting('app.maintenance', true) = 'on')
  WITH CHECK (current_org() IS NOT NULL OR current_setting('app.maintenance', true) = 'on');
-- No GRANT: 0001's ALTER DEFAULT PRIVILEGES already gives otto_app SELECT, INSERT, UPDATE and
-- DELETE on new tables. A bare GRANT would fail on a database without the role (0002 adds a table
-- the same way).
