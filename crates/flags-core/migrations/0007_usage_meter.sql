-- Meter production evaluations (#140): outbox meter columns and the hourly counter.
--
-- usage_outbox and usage_outbox_rejected gain meter (text) and quantity
-- (bigint) so an event can carry a metered amount instead of one billable tool
-- call. A CHECK on each keeps the pair together (both NULL for an unmetered
-- row, or both set with quantity > 0). usage_meter_hourly counts SDK
-- evaluations per (org, app, hour) in served/reported columns; a later closer
-- turns each finished hour into a metered outbox event.
--
-- Additive and rollback-compatible. The previous release writes outbox rows
-- with meter and quantity NULL, which the CHECKs allow, and never touches
-- usage_meter_hourly, so it runs unchanged during the rollout overlap and
-- after a rollback; the new columns and table simply sit unused. The new
-- table is tenant-isolated like the other retention tables: RLS enabled and
-- forced, with unpinned access needing the maintenance marker (0003, 0005).

-- ACCESS EXCLUSIVE locks below: fail fast rather than queue behind a long
-- transaction. sqlx runs each migration in its own transaction, so SET LOCAL
-- ends with it.
SET LOCAL lock_timeout = '3s';

ALTER TABLE usage_outbox_rejected
  ADD COLUMN meter text,
  ADD COLUMN quantity bigint,
  ADD CONSTRAINT usage_outbox_rejected_meter_quantity
    CHECK ((meter IS NULL) = (quantity IS NULL) AND (quantity IS NULL OR quantity > 0));
ALTER TABLE usage_outbox
  ADD COLUMN meter text,
  ADD COLUMN quantity bigint,
  ADD CONSTRAINT usage_outbox_meter_quantity
    CHECK ((meter IS NULL) = (quantity IS NULL) AND (quantity IS NULL OR quantity > 0));

CREATE TABLE usage_meter_hourly (
  org_id   uuid        NOT NULL,
  app_id   uuid        NOT NULL,
  hour     timestamptz NOT NULL,
  -- Successful /api/evaluate answers, and accepted telemetry evaluation events. Billed as the
  -- larger of the two, never their sum (telemetry also reports server-answered evaluations).
  served   bigint      NOT NULL DEFAULT 0 CHECK (served >= 0),
  reported bigint      NOT NULL DEFAULT 0 CHECK (reported >= 0),
  PRIMARY KEY (org_id, app_id, hour),
  FOREIGN KEY (org_id, app_id) REFERENCES flag_apps (org_id, id) ON DELETE CASCADE
);
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
