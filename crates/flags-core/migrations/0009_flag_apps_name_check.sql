-- Enforce the app name rule in the database (#157). `validate_name`
-- (flags_core::eval) already restricts every write path to 1-64 characters of
-- letters, digits, '-', '_' and '.', and `sdk_guide` re-checks it at render
-- time because the name is substituted into text an agent reads as
-- instructions. This constraint makes a future write path that skips
-- `validate_name` (a rename tool, a backfill) fail here instead of storing a
-- name those consumers trust.
--
-- NOT VALID: checked on every insert and update from now on, but existing rows
-- are not scanned, so the migration cannot fail on legacy data and takes no
-- long lock. The previous release only writes names that pass `validate_name`,
-- so it is unaffected. A later migration runs VALIDATE CONSTRAINT once
-- production rows are confirmed to pass.
ALTER TABLE flag_apps
  ADD CONSTRAINT flag_apps_name_check
  CHECK (name ~ '^[A-Za-z0-9._-]{1,64}$') NOT VALID;
