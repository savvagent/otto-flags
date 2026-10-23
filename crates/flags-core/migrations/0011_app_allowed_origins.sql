-- Per-app origin allowlist for the public client key (#143).
--
-- `allowed_origins` is the browser origins (`https://app.example.com`, or
-- `https://*.example.com`) whose requests the client key may make. Empty, the
-- default, means unrestricted, which is what every app has today, so no
-- existing app changes behavior. The server (`srv_`) key is never checked.
--
-- Additive: the previous release never reads or writes the column, and its
-- `INSERT INTO flag_apps` and `UPDATE flag_apps` statements leave it at the
-- default. The entry shape is validated by `flags_core::apps::normalize_origins`
-- on every write path; the constraint below bounds what a path that skipped it
-- could store.
--
-- ADD COLUMN with a constant default is a catalog-only change, but the ALTER
-- queues behind any open transaction that touched flag_apps, and readers queue
-- behind it; the timeout turns that stall into a retriable failed boot, as in
-- 0002, 0005, 0007, 0008 and 0009.

SET LOCAL lock_timeout = '3s';

ALTER TABLE flag_apps
  ADD COLUMN allowed_origins text[] NOT NULL DEFAULT '{}'
  CONSTRAINT flag_apps_allowed_origins_check CHECK (cardinality(allowed_origins) <= 20);
