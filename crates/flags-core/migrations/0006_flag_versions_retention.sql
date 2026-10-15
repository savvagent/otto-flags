-- Flag history retention (#123): the background sweep removes versions that
-- are both older than 90 days and outside their flag's newest 100
-- (flags_core::flags::sweep_versions). It runs unpinned, across every org,
-- inside maintenance::begin; flag_versions had only its tenant policy, under
-- which an unpinned transaction sees and deletes nothing.
--
-- As 0003 did for the telemetry tables, each policy admits only an unpinned
-- transaction (current_org() IS NULL never holds inside a pinned one, so no
-- tenant sees anything new) that also carries the transaction-local marker
-- app.maintenance = 'on', which only flags-core's maintenance helper sets: a
-- request path that forgot to pin is unpinned too, and still sees nothing
-- (#27). The sweep's DELETE reads columns in its WHERE, so it needs the
-- SELECT policy as well as the DELETE one. Both require the marker
-- themselves, so no restrictive policy is needed (unlike 0003's, which
-- tightened 0001's markerless DELETE policies).
--
-- Policy-only and additive. The previous release never reads or deletes
-- flag_versions unpinned, so during rollout and after a rollback it behaves
-- as before; it just does not sweep.

-- CREATE POLICY takes a lock that conflicts with every flag write (each one
-- inserts a version): fail fast rather than queue behind a long transaction.
-- sqlx runs each migration in its own transaction, so SET LOCAL ends with it.
SET LOCAL lock_timeout = '3s';

CREATE POLICY flag_versions_retention_read ON flag_versions
  FOR SELECT USING (current_org() IS NULL
                    AND current_setting('app.maintenance', true) = 'on');
CREATE POLICY flag_versions_retention ON flag_versions
  FOR DELETE USING (current_org() IS NULL
                    AND current_setting('app.maintenance', true) = 'on');
