-- Unpinned access to the usage outbox needs the maintenance marker (#68).
--
-- 0001 gives usage_outbox and usage_outbox_rejected a permissive policy that
-- lets any unpinned transaction (current_org() IS NULL) read and write every
-- org's rows, for the background shipper that drains them. Unpinned is not
-- enough on its own (#27): a request path that forgot to pin is unpinned too.
-- As 0003 did for the retention tables, unpinned access now also needs the
-- transaction-local marker app.maintenance = 'on', which only flags-core's
-- maintenance::begin sets; the shipper runs inside it.
--
-- RESTRICTIVE and FOR ALL: it ANDs with the baseline's permissive policy for
-- every command, so a pinned transaction (current_org() set) is unaffected and
-- still reaches only its own org, while an unpinned one needs the marker to
-- read, change, delete, or insert.
--
-- Policy-only and additive. The previous release writes these tables pinned
-- (tool metering, the org.deleted purge), which still passes. Its shipper runs
-- unpinned without the marker: under an RLS-exempt pool role, as production's
-- superuser attach role is, RLS does not apply and it keeps shipping; under a
-- non-exempt pool role it would see no rows during the rollout overlap and
-- after a rollback, so usage waits in the outbox (nothing is lost) until a
-- release with the marker ships it.

-- CREATE POLICY takes a lock that conflicts with every metered tool call's
-- insert: fail fast rather than queue behind a long transaction. sqlx runs
-- each migration in its own transaction, so SET LOCAL ends with it.
SET LOCAL lock_timeout = '3s';

CREATE POLICY usage_outbox_maintenance ON usage_outbox
  AS RESTRICTIVE FOR ALL
  USING (current_org() IS NOT NULL OR current_setting('app.maintenance', true) = 'on')
  WITH CHECK (current_org() IS NOT NULL OR current_setting('app.maintenance', true) = 'on');
CREATE POLICY usage_outbox_rejected_maintenance ON usage_outbox_rejected
  AS RESTRICTIVE FOR ALL
  USING (current_org() IS NOT NULL OR current_setting('app.maintenance', true) = 'on')
  WITH CHECK (current_org() IS NOT NULL OR current_setting('app.maintenance', true) = 'on');
