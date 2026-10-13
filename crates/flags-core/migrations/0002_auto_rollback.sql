-- Auto-rollback (#11): per-flag-and-environment rollback policies and the
-- events they record, plus the SDK-key kind on error reports and the server
-- share of hourly evaluation counts, so the checker can ignore public-key
-- telemetry. flag_versions.change also gains the value 'auto_rollback'; the
-- baseline's comment listing the change values cannot be edited (sqlx
-- checksums applied migrations), so it is recorded here.
--
-- Rolling deploy: every change is additive. The previous release inserts into
-- flag_errors without reporter (NULL, treated as client) and upserts
-- flag_eval_hourly without server_count (stays 0, treated as client), and it
-- never touches the new tables.

-- Fail fast rather than queue: ALTER TABLE on flag_errors and flag_eval_hourly
-- takes ACCESS EXCLUSIVE, and a lock request waiting behind a long query
-- blocks every telemetry insert queued behind it. sqlx runs each migration in
-- its own transaction, so SET LOCAL ends with it; a timeout fails the boot and
-- the deploy rolls back, to be retried.
SET LOCAL lock_timeout = '3s';

-- Which kind of SDK key reported an error. Auto-rollback counts only server
-- reports unless a policy opts in: a client key is public, so anyone can
-- report anything with it. NULL for rows written before this column existed,
-- treated as client.
ALTER TABLE flag_errors ADD COLUMN reporter text;

-- How many of a bucket's evaluations a server key reported (count stays the
-- total). A column rather than a primary-key change, so the release still
-- running during a rolling deploy keeps working: its INSERT ... ON CONFLICT
-- leaves server_count at 0 (counted as client), and the new release adds
-- EXCLUDED.server_count in the same upsert.
ALTER TABLE flag_eval_hourly ADD COLUMN server_count bigint NOT NULL DEFAULT 0;

-- The checks are added NOT VALID: validating would scan every existing row
-- while ACCESS EXCLUSIVE is held, and those rows cannot fail them (reporter is
-- NULL, server_count 0). NOT VALID constraints still check every row written
-- from now on.
ALTER TABLE flag_errors ADD CONSTRAINT flag_errors_reporter_check
  CHECK (reporter IN ('client', 'server')) NOT VALID;
ALTER TABLE flag_eval_hourly ADD CONSTRAINT flag_eval_hourly_server_count_check
  CHECK (server_count >= 0 AND server_count <= count) NOT VALID;

CREATE TYPE rollback_mode AS ENUM ('enforce', 'observe');

-- One policy per flag and environment.
CREATE TABLE rollback_policies (
  id                     uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  org_id                 uuid          NOT NULL,
  app_id                 uuid          NOT NULL,
  flag_id                uuid          NOT NULL,
  environment            text          NOT NULL,
  mode                   rollback_mode NOT NULL DEFAULT 'enforce',
  -- Decision thresholds (see flags_core::rollback::decide).
  min_evaluations        bigint        NOT NULL DEFAULT 100  CHECK (min_evaluations >= 1),
  min_errors             bigint        NOT NULL DEFAULT 10   CHECK (min_errors >= 1),
  max_error_ratio        double precision NOT NULL DEFAULT 2.0 CHECK (max_error_ratio > 1.0),
  max_error_rate         double precision CHECK (max_error_rate > 0 AND max_error_rate <= 1),
  window_hours           integer       NOT NULL DEFAULT 1    CHECK (window_hours BETWEEN 1 AND 24),
  include_client_reports boolean       NOT NULL DEFAULT false,
  -- Set only when an enforcing policy turned the environment off (or found it
  -- already off); observe, not-entitled, and unknown-entitlement trips leave
  -- the policy armed. Cleared when the environment is turned back on. A
  -- tripped policy does not act again, so a rollback cannot flap.
  tripped_at             timestamptz,
  last_checked_at        timestamptz,
  -- Why this policy's last action failed: an error code (never database or
  -- reported text) and when. Set when the checker could not act on it (the
  -- policy stays armed and is retried); cleared by its next successful check.
  last_error_code        text,
  last_error_at          timestamptz,
  created_by             uuid,
  created_at             timestamptz   NOT NULL DEFAULT now(),
  updated_at             timestamptz   NOT NULL DEFAULT now(),
  UNIQUE (flag_id, environment),
  UNIQUE (org_id, id),
  FOREIGN KEY (org_id, app_id)  REFERENCES flag_apps (org_id, id)     ON DELETE CASCADE,
  FOREIGN KEY (org_id, flag_id) REFERENCES feature_flags (org_id, id) ON DELETE CASCADE
);

-- No separate org_id index: UNIQUE (org_id, id) leads with org_id and serves
-- org-wide lookups and the org purge.
-- The backstop's lookup: armed policies, least recently checked first.
CREATE INDEX rollback_policies_due_idx ON rollback_policies (last_checked_at NULLS FIRST) WHERE tripped_at IS NULL;

-- What policies did, or would have done. Kept 90 days (the evaluation
-- retention), swept unpinned like flag_errors.
CREATE TABLE rollback_events (
  id            bigserial   PRIMARY KEY,
  org_id        uuid        NOT NULL,
  -- NULL once the policy is removed: its history is kept (#28).
  policy_id     uuid,
  flag_id       uuid        NOT NULL,
  environment   text        NOT NULL,
  -- rolled_back | would_roll_back (observe mode) | not_entitled | entitlement_unknown
  outcome       text        NOT NULL
                CHECK (outcome IN ('rolled_back', 'would_roll_back', 'not_entitled', 'entitlement_unknown')),
  -- The numbers the decision was made on: counts, rates, thresholds.
  observed      jsonb       NOT NULL,
  -- The flag version the rollback created. NULL for every outcome but
  -- rolled_back, and for rolled_back when the environment was already off
  -- (nothing to turn off, so no version).
  flag_version  integer,
  created_at    timestamptz NOT NULL DEFAULT now(),
  -- Removing a policy keeps its events and clears only policy_id (a column
  -- list on SET NULL needs Postgres 15; production runs 16). org_id stays,
  -- so the row keeps its tenant; the org purge deletes events explicitly.
  FOREIGN KEY (org_id, policy_id) REFERENCES rollback_policies (org_id, id)
    ON DELETE SET NULL (policy_id),
  FOREIGN KEY (org_id, flag_id)   REFERENCES feature_flags (org_id, id)     ON DELETE CASCADE
);

CREATE INDEX rollback_events_flag_idx ON rollback_events (flag_id, created_at DESC);
-- The policy FK's SET NULL and the checker's once-per-hour dedup lookup both
-- filter by policy; leading with org_id keeps the org purge indexed too.
CREATE INDEX rollback_events_policy_idx ON rollback_events (org_id, policy_id, created_at DESC);
CREATE INDEX rollback_events_created_idx ON rollback_events (created_at);

-- Tenant isolation, as in the baseline: FORCE RLS and a policy named
-- <table>_tenant_isolation so Db::verify_tenant_isolation finds them.
ALTER TABLE rollback_policies ENABLE ROW LEVEL SECURITY;
ALTER TABLE rollback_policies FORCE ROW LEVEL SECURITY;
CREATE POLICY rollback_policies_tenant_isolation ON rollback_policies
  USING (org_id = current_org()) WITH CHECK (org_id = current_org());
-- The checker's backstop finds due policies across every org, unpinned, then
-- pins per org (begin_live) to act. Policies are permissive and OR together, so
-- this adds exactly one thing: an unpinned SELECT. A pinned transaction still
-- sees only its own org's policies, and nothing unpinned can write them.
--
-- Unpinned alone is not enough (#27): a request path that forgot to pin also
-- has current_org() NULL. Every unpinned policy added here and in 0003 also
-- needs the transaction-local marker app.maintenance = 'on', which only
-- flags-core's background maintenance helper (maintenance::begin) sets. The
-- baseline's DELETE-only retention policies cannot be edited, but a sweep's
-- WHERE reads columns and so needs one of these SELECT policies too.
CREATE POLICY rollback_policies_checker ON rollback_policies
  FOR SELECT USING (current_org() IS NULL
                    AND current_setting('app.maintenance', true) = 'on');

ALTER TABLE rollback_events ENABLE ROW LEVEL SECURITY;
ALTER TABLE rollback_events FORCE ROW LEVEL SECURITY;
CREATE POLICY rollback_events_tenant_isolation ON rollback_events
  USING (org_id = current_org()) WITH CHECK (org_id = current_org());
-- Marked like the read below, so an unmarked unpinned DELETE (with no WHERE,
-- which needs no SELECT policy) cannot empty the table either.
CREATE POLICY rollback_events_retention ON rollback_events
  FOR DELETE USING (current_org() IS NULL
                    AND current_setting('app.maintenance', true) = 'on');
-- Postgres also applies SELECT policies to a DELETE whose WHERE reads columns,
-- so a DELETE-only policy would let an otto_app sweep delete nothing: the
-- sweep's `WHERE created_at < ...` needs an unpinned read as well (#12),
-- behind the same marker (#27).
CREATE POLICY rollback_events_retention_read ON rollback_events
  FOR SELECT USING (current_org() IS NULL
                    AND current_setting('app.maintenance', true) = 'on');

-- Grants: the baseline's ALTER DEFAULT PRIVILEGES already covers tables and
-- sequences the migrating role creates later. Grant explicitly anyway, guarded
-- as the baseline guards it, so the migration does not depend on the
-- migrating role being the one that ran the baseline.
DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'otto_app') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON rollback_policies, rollback_events TO otto_app';
    EXECUTE 'GRANT USAGE, SELECT ON SEQUENCE rollback_events_id_seq TO otto_app';
  END IF;
END $$;
