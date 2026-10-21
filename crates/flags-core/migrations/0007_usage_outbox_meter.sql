-- Meter production evaluations (#140): outbox meter columns.
--
-- usage_outbox and usage_outbox_rejected gain meter (text) and quantity
-- (bigint), so an event can carry a metered amount instead of one billable tool
-- call. A CHECK on each keeps the pair together: both NULL for an unmetered
-- row, or both set with quantity > 0.
--
-- Additive and rollback-compatible. The previous release writes outbox rows
-- with meter and quantity NULL, which the CHECKs allow. Its shipper does not
-- read the new columns; docs/deploy/fly.md (Metering, Rollback) says what
-- that costs during an overlap.
--
-- Only these two ALTERs run here, so the ACCESS EXCLUSIVE locks they take are
-- held briefly and on nothing else: 0008 creates the hourly table, whose
-- foreign key locks flag_apps. In one migration, a tool call holding
-- flag_apps and then writing usage_outbox could deadlock with it.
--
-- The CHECKs are NOT VALID: every existing row has both columns NULL (they
-- were just added), so there is nothing to validate, and skipping the scan
-- keeps metered tool calls from queueing behind it. New and updated rows are
-- checked as usual.

-- Fail fast rather than queue behind a long transaction. sqlx runs each
-- migration in its own transaction, so SET LOCAL ends with it.
SET LOCAL lock_timeout = '3s';

-- usage_outbox_rejected first, the order the shipper locks them (as in 0005).
ALTER TABLE usage_outbox_rejected
  ADD COLUMN meter text,
  ADD COLUMN quantity bigint,
  ADD CONSTRAINT usage_outbox_rejected_meter_quantity
    CHECK ((meter IS NULL) = (quantity IS NULL) AND (quantity IS NULL OR quantity > 0)) NOT VALID;
ALTER TABLE usage_outbox
  ADD COLUMN meter text,
  ADD COLUMN quantity bigint,
  ADD CONSTRAINT usage_outbox_meter_quantity
    CHECK ((meter IS NULL) = (quantity IS NULL) AND (quantity IS NULL OR quantity > 0)) NOT VALID;
