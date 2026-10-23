-- Enforce the app name rule in the database (#157). `validate_name`
-- (flags_core::eval) already restricts every write path to 1-64 characters of
-- letters, digits, '-', '_' and '.', and `sdk_guide` re-checks it at render
-- time because the name is substituted into text an agent reads as
-- instructions. This constraint makes a future write path that skips
-- `validate_name` (a rename tool, a backfill) fail here instead of storing a
-- name those consumers trust.
--
-- NOT VALID: existing rows are not scanned, so the migration cannot fail on
-- legacy data. Postgres still checks the constraint on every later INSERT and
-- on every UPDATE of a row, whichever column the UPDATE changes, so a legacy
-- row with a non-conforming name could not have its environments or keys
-- changed until renamed; none is expected, because `create_app` has always
-- validated. The previous release only writes names that pass
-- `validate_name`, so it is unaffected. A later migration runs VALIDATE
-- CONSTRAINT once production rows are confirmed to pass.
--
-- The ALTER holds ACCESS EXCLUSIVE on flag_apps only briefly (no scan), but it
-- queues behind any open transaction that touched the table, and readers queue
-- behind it; the timeout turns that stall into a retriable failed boot, as in
-- 0002, 0005, 0007 and 0008.
SET LOCAL lock_timeout = '3s';

ALTER TABLE flag_apps
  ADD CONSTRAINT flag_apps_name_check
  CHECK (name ~ '^[A-Za-z0-9._-]{1,64}$') NOT VALID;
