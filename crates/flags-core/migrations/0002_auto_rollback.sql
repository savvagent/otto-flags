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

-- Which kind of SDK key reported an error. Auto-rollback counts only server
-- reports unless a policy opts in: a client key is public, so anyone can
-- report anything with it. NULL for rows written before this column existed,
-- treated as client.
ALTER TABLE flag_errors ADD COLUMN reporter text
  CHECK (reporter IN ('client', 'server'));

-- How many of a bucket's evaluations a server key reported (count stays the
-- total). A column rather than a primary-key change, so the release still
-- running during a rolling deploy keeps working: its INSERT ... ON CONFLICT
-- leaves server_count at 0 (counted as client), and the new release adds
-- EXCLUDED.server_count in the same upsert.
ALTER TABLE flag_eval_hourly ADD COLUMN server_count bigint NOT NULL DEFAULT 0
  CHECK (server_count >= 0);

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
  -- Set when the policy trips; cleared when the environment is turned back on.
  -- A tripped policy does not act again, so a rollback cannot flap.
  tripped_at             timestamptz,
  last_checked_at        timestamptz,
  created_by             uuid,
  created_at             timestamptz   NOT NULL DEFAULT now(),
  updated_at             timestamptz   NOT NULL DEFAULT now(),
  UNIQUE (flag_id, environment),
  UNIQUE (org_id, id),
  FOREIGN KEY (org_id, app_id)  REFERENCES flag_apps (org_id, id)     ON DELETE CASCADE,
  FOREIGN KEY (org_id, flag_id) REFERENCES feature_flags (org_id, id) ON DELETE CASCADE
);

CREATE INDEX rollback_policies_org_idx ON rollback_policies (org_id);
-- The backstop's lookup: armed policies, least recently checked first.
CREATE INDEX rollback_policies_due_idx ON rollback_policies (last_checked_at NULLS FIRST) WHERE tripped_at IS NULL;

-- What policies did, or would have done. Kept 90 days (the evaluation
-- retention), swept unpinned like flag_errors.
CREATE TABLE rollback_events (
  id            bigserial   PRIMARY KEY,
  org_id        uuid        NOT NULL,
  policy_id     uuid        NOT NULL,
  flag_id       uuid        NOT NULL,
  environment   text        NOT NULL,
  -- rolled_back | would_roll_back (observe mode) | not_entitled | entitlement_unknown
  outcome       text        NOT NULL
                CHECK (outcome IN ('rolled_back', 'would_roll_back', 'not_entitled', 'entitlement_unknown')),
  -- The numbers the decision was made on: counts, rates, thresholds.
  observed      jsonb       NOT NULL,
  -- The flag version the rollback created (rolled_back only).
  flag_version  integer,
  created_at    timestamptz NOT NULL DEFAULT now(),
  FOREIGN KEY (org_id, policy_id) REFERENCES rollback_policies (org_id, id) ON DELETE CASCADE,
  FOREIGN KEY (org_id, flag_id)   REFERENCES feature_flags (org_id, id)     ON DELETE CASCADE
);

CREATE INDEX rollback_events_flag_idx ON rollback_events (flag_id, created_at DESC);
-- The policy FK cascade and the checker's once-per-hour dedup lookup both filter
-- by policy; leading with org_id keeps the org purge indexed too.
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
-- sees only its own org's policies, and nothing unpinned can write them. Same
-- shape as the baseline's unpinned retention policies (flag_errors_retention and its siblings in 0001_baseline.sql).
CREATE POLICY rollback_policies_checker ON rollback_policies
  FOR SELECT USING (current_org() IS NULL);

ALTER TABLE rollback_events ENABLE ROW LEVEL SECURITY;
ALTER TABLE rollback_events FORCE ROW LEVEL SECURITY;
CREATE POLICY rollback_events_tenant_isolation ON rollback_events
  USING (org_id = current_org()) WITH CHECK (org_id = current_org());
CREATE POLICY rollback_events_retention ON rollback_events
  FOR DELETE USING (current_org() IS NULL);
-- Postgres also applies SELECT policies to a DELETE whose WHERE reads columns,
-- so a DELETE-only policy would let an otto_app sweep delete nothing: the
-- sweep's `WHERE created_at < ...` needs an unpinned read as well (#12).
CREATE POLICY rollback_events_retention_read ON rollback_events
  FOR SELECT USING (current_org() IS NULL);

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
