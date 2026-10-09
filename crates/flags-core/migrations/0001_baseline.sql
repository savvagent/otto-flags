-- otto-flags domain baseline.
--
-- This database holds flag apps, their SDK keys, flags and their version
-- history, evaluation and error telemetry, and otto-flags' own bookkeeping
-- (usage outbox, lifecycle-event log, tombstones, audit trail). It holds no
-- identity: users, orgs, teams, memberships, tokens, plans, and usage totals
-- belong to the otto platform, in another database, so nothing here has a
-- foreign key to them. An `org_id` or user id in this schema is an opaque uuid
-- the platform issued. The platform's signed lifecycle webhooks
-- (`POST /platform/webhooks`) clean up after deletions.
--
-- Tenant isolation: every table carrying an org's rows has a NOT NULL `org_id`,
-- FORCE ROW LEVEL SECURITY, and a policy named `<table>_tenant_isolation`
-- (`Db::verify_tenant_isolation` discovers tenant tables by that name).
--
-- Append-only from here. sqlx checksums applied migrations; editing this file
-- after a deployment has run it stops that deployment from booting. CI enforces
-- this (`migrations-append-only`).

-- The role every tenant transaction runs as. NOLOGIN: it is never a connection
-- identity, only a `SET LOCAL ROLE` target, and its name is hard-coded in
-- `otto_tenant::Db::begin`.
--
-- Roles are cluster-scoped while migrations are database-scoped, so creation is
-- idempotent and tolerant of two concurrent migrations racing to create it
-- (`#[sqlx::test]` migrates many throwaway databases in one cluster in
-- parallel, and on the shared `otto-db` instance the role is shared with
-- otto-platform and otto-factory). It also tolerates `insufficient_privilege`:
-- managed Postgres does not hand the application CREATEROLE. There, FORCE ROW
-- LEVEL SECURITY below carries the isolation guarantee and
-- `Db::verify_tenant_isolation` proves it at startup, refusing to serve
-- otherwise.
DO $$
BEGIN
  BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'otto_app') THEN
      CREATE ROLE otto_app NOLOGIN;
    END IF;
  EXCEPTION
    WHEN duplicate_object OR unique_violation THEN
      NULL; -- another database in this cluster created it first
    WHEN insufficient_privilege THEN
      RAISE NOTICE
        'could not CREATE ROLE otto_app (no CREATEROLE). Tenant transactions '
        'will run as the connecting role and rely on FORCE ROW LEVEL SECURITY, '
        'which holds only while that role is neither a superuser nor '
        'BYPASSRLS. flags-server verifies this at startup and refuses to serve '
        'otherwise.';
  END;
END $$;

CREATE EXTENSION IF NOT EXISTS "pgcrypto";

CREATE OR REPLACE FUNCTION current_org() RETURNS uuid AS $$
  SELECT NULLIF(current_setting('app.org_id', true), '')::uuid;
$$ LANGUAGE sql STABLE;

-- ------------------------------------------------------------------ flag apps

-- An application registered for flag management: the seam between MCP-driven
-- management and SDK-driven evaluation. `environments` is just the list of
-- names this app evaluates in; per-environment *state* lives on each flag.
CREATE TABLE flag_apps (
  id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  org_id       uuid        NOT NULL,
  name         text        NOT NULL,
  environments text[]      NOT NULL DEFAULT ARRAY['production'],
  -- The client (browser/mobile) key is public by design -- it ships inside
  -- every page that evaluates flags -- so it is kept in clear here to be shown
  -- again. The server key is never stored in clear; see app_keys.
  client_key   text        NOT NULL,
  created_at   timestamptz NOT NULL DEFAULT now(),
  created_by   uuid,
  UNIQUE (org_id, name),
  -- Target of the composite foreign keys below. Foreign-key checks bypass
  -- row-level security, so a plain `REFERENCES flag_apps (id)` would accept
  -- another org's app id; keying on (org_id, id) makes the database refuse it.
  UNIQUE (org_id, id)
);

CREATE INDEX flag_apps_org_idx ON flag_apps (org_id);

-- SDK credentials, by SHA-256 of the key. Deliberately NOT under row-level
-- security: an SDK request carries a key and nothing else, so the org is not
-- known until this table answers (the same reason deleted_orgs is outside it).
-- It holds only hashes and the ids they resolve to.
CREATE TABLE app_keys (
  key_hash   bytea       PRIMARY KEY,
  org_id     uuid        NOT NULL,
  app_id     uuid        NOT NULL,
  kind       text        NOT NULL CHECK (kind IN ('client', 'server')),
  -- The first characters of the key, for telling keys apart in a listing
  -- without being able to use them.
  prefix     text        NOT NULL,
  created_at timestamptz NOT NULL DEFAULT now(),
  FOREIGN KEY (org_id, app_id) REFERENCES flag_apps (org_id, id) ON DELETE CASCADE
);

CREATE INDEX app_keys_app_idx ON app_keys (app_id);

-- ---------------------------------------------------------------------- flags

CREATE TYPE flag_status AS ENUM ('active', 'archived');

CREATE TABLE feature_flags (
  id            uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  org_id        uuid        NOT NULL,
  app_id        uuid        NOT NULL,
  key           text        NOT NULL,
  name          text        NOT NULL,
  description   text,
  status        flag_status NOT NULL DEFAULT 'active',
  -- Per-environment state, keyed by environment name:
  --   {"production": {"enabled": true, "rollout_percentage": 25,
  --                   "rules": [...], "default_variation": "control"}}
  -- Validated and interpreted by flags_core::eval; read and written whole.
  environments  jsonb       NOT NULL DEFAULT '{}'::jsonb,
  -- Variations keyed by name: {"control": {"weight": 50, "configuration": {...}}}.
  variations    jsonb       NOT NULL DEFAULT '{}'::jsonb,
  -- Dynamic configuration served with the flag when no variation supplies one.
  configuration jsonb,
  -- Bumped on every change; flag_versions keeps a snapshot of each.
  version       integer     NOT NULL DEFAULT 1,
  created_at    timestamptz NOT NULL DEFAULT now(),
  updated_at    timestamptz NOT NULL DEFAULT now(),
  archived_at   timestamptz,
  created_by    uuid,
  UNIQUE (app_id, key),
  UNIQUE (org_id, id),
  FOREIGN KEY (org_id, app_id) REFERENCES flag_apps (org_id, id) ON DELETE CASCADE
);

CREATE INDEX feature_flags_org_idx ON feature_flags (org_id);

-- Every version a flag has had, as a full snapshot, so any of them can be
-- inspected or restored (`rollback_flag`). Written in the same transaction as
-- the change it records.
CREATE TABLE flag_versions (
  id            bigserial   PRIMARY KEY,
  org_id        uuid        NOT NULL,
  flag_id       uuid        NOT NULL,
  version       integer     NOT NULL,
  -- create | update | set_environment | archive | restore | rollback
  change        text        NOT NULL,
  snapshot      jsonb       NOT NULL,
  actor_user_id uuid,
  reason        text,
  created_at    timestamptz NOT NULL DEFAULT now(),
  UNIQUE (flag_id, version),
  FOREIGN KEY (org_id, flag_id) REFERENCES feature_flags (org_id, id) ON DELETE CASCADE
);

CREATE INDEX flag_versions_org_idx ON flag_versions (org_id);

-- ------------------------------------------------------------------ telemetry

-- Evaluation counts, folded into hourly buckets as SDK telemetry arrives. Raw
-- per-evaluation rows are deliberately not kept: they would dominate the
-- database and retain end-user context nobody needs. `variation` is '' when
-- the evaluation resolved to none (a primary key column cannot be NULL).
CREATE TABLE flag_eval_hourly (
  org_id      uuid        NOT NULL,
  flag_id     uuid        NOT NULL,
  environment text        NOT NULL,
  hour        timestamptz NOT NULL,
  enabled     boolean     NOT NULL,
  variation   text        NOT NULL DEFAULT '',
  count       bigint      NOT NULL DEFAULT 0,
  PRIMARY KEY (flag_id, environment, hour, enabled, variation),
  FOREIGN KEY (org_id, flag_id) REFERENCES feature_flags (org_id, id) ON DELETE CASCADE
);

CREATE INDEX flag_eval_hourly_org_idx ON flag_eval_hourly (org_id, hour);

-- Errors an SDK reported while evaluating code behind a flag: the evidence
-- flag_health correlates against. Kept raw (truncated on the way in) and swept
-- after a retention window.
CREATE TABLE flag_errors (
  id            bigserial   PRIMARY KEY,
  org_id        uuid        NOT NULL,
  flag_id       uuid        NOT NULL,
  environment   text,
  flag_enabled  boolean     NOT NULL,
  error_type    text        NOT NULL,
  error_message text        NOT NULL,
  stack_trace   text,
  occurred_at   timestamptz NOT NULL,
  received_at   timestamptz NOT NULL DEFAULT now(),
  FOREIGN KEY (org_id, flag_id) REFERENCES feature_flags (org_id, id) ON DELETE CASCADE
);

CREATE INDEX flag_errors_flag_time_idx ON flag_errors (flag_id, occurred_at DESC);
CREATE INDEX flag_errors_org_idx ON flag_errors (org_id);
CREATE INDEX flag_errors_received_idx ON flag_errors (received_at);

-- ----------------------------------------------------------------- usage outbox

-- One row per MCP tool call, written in the tool's own transaction and shipped
-- to the platform's /internal/usage by a background task. The platform dedupes
-- on event_id, so shipping is retried freely.
CREATE TABLE usage_outbox (
  id              bigserial   PRIMARY KEY,
  event_id        uuid        NOT NULL DEFAULT gen_random_uuid() UNIQUE,
  org_id          uuid        NOT NULL,
  user_id         uuid,
  tool            text        NOT NULL,
  billable        boolean     NOT NULL,
  occurred_at     timestamptz NOT NULL DEFAULT now(),
  attempts        integer     NOT NULL DEFAULT 0,
  next_attempt_at timestamptz NOT NULL DEFAULT now(),
  last_error      text
);

CREATE INDEX usage_outbox_due_idx ON usage_outbox (next_attempt_at, id);
CREATE INDEX usage_outbox_org_idx ON usage_outbox (org_id);

-- Usage the platform refused outright, kept with its reason rather than dropped.
CREATE TABLE usage_outbox_rejected (
  id          bigserial   PRIMARY KEY,
  event_id    uuid        NOT NULL,
  org_id      uuid        NOT NULL,
  user_id     uuid,
  tool        text        NOT NULL,
  billable    boolean     NOT NULL,
  occurred_at timestamptz NOT NULL,
  reason      text        NOT NULL,
  rejected_at timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX usage_outbox_rejected_org_idx ON usage_outbox_rejected (org_id, rejected_at DESC);

-- ---------------------------------------------------------- platform event log

-- Lifecycle webhook deliveries already applied, so a redelivery is a no-op.
-- Written in the same pinned transaction as the cleanup it records, and kept
-- after an org.deleted purge so a replay still finds it.
CREATE TABLE platform_events (
  event_id    uuid PRIMARY KEY,
  org_id      uuid        NOT NULL,
  kind        text        NOT NULL,
  received_at timestamptz NOT NULL DEFAULT now()
);

-- ------------------------------------------------------------------ tombstones

-- Orgs the platform has deleted. Checked on every authenticated request (MCP
-- and SDK) so neither a cached introspection nor a still-valid SDK key can
-- write after the purge. Permanent; not under RLS because it is consulted
-- before any org is pinned.
CREATE TABLE deleted_orgs (
  org_id     uuid PRIMARY KEY,
  deleted_at timestamptz NOT NULL DEFAULT now()
);

-- Members the platform has just removed, refused for a few minutes until the
-- platform's own introspection stops vouching for them. Not under RLS, for the
-- same reason as deleted_orgs.
CREATE TABLE removed_members (
  org_id     uuid        NOT NULL,
  user_id    uuid        NOT NULL,
  removed_at timestamptz NOT NULL DEFAULT now(),
  PRIMARY KEY (org_id, user_id)
);

-- ----------------------------------------------------------------- audit trail

-- otto-flags' own domain audit trail (app and key changes), written with
-- otto-tenant's `Tx::audit` in the same transaction as the change. Same shape
-- as the platform's table so the otto-tenant API works unchanged.
CREATE TABLE audit_events (
  id            bigserial PRIMARY KEY,
  org_id        uuid,
  actor_user_id uuid,
  actor_label   text,
  action        text        NOT NULL,
  target_type   text,
  target_id     text,
  ip            text,
  user_agent    text,
  detail        jsonb       NOT NULL DEFAULT '{}'::jsonb,
  created_at    timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX audit_events_org_time_idx ON audit_events (org_id, created_at DESC);
CREATE INDEX audit_events_action_idx ON audit_events (action, created_at DESC);

-- ---------------------------------------------------------- row-level security

DO $$
DECLARE
  t text;
  tenant_tables text[] := ARRAY[
    'flag_apps',
    'feature_flags',
    'flag_versions',
    'flag_eval_hourly',
    'flag_errors',
    'platform_events'
  ];
BEGIN
  FOREACH t IN ARRAY tenant_tables LOOP
    EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
    -- FORCE covers the case where the application role IS the table owner.
    EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
    EXECUTE format(
      'CREATE POLICY %I ON %I USING (org_id = current_org()) WITH CHECK (org_id = current_org())',
      t || '_tenant_isolation', t
    );
  END LOOP;
END $$;

-- Retention sweeps run unpinned, across every org. Policies are permissive and
-- OR together, so these add exactly one thing to the tenant policy above: an
-- unpinned DELETE. A pinned transaction still reaches only its own org's rows.
CREATE POLICY flag_errors_retention ON flag_errors
  FOR DELETE USING (current_org() IS NULL);
CREATE POLICY flag_eval_hourly_retention ON flag_eval_hourly
  FOR DELETE USING (current_org() IS NULL);
CREATE POLICY platform_events_retention ON platform_events
  FOR DELETE USING (current_org() IS NULL);

-- Written by pinned transactions, drained by background tasks that serve every
-- org. A pinned transaction sees and writes only its own org's rows; an
-- unpinned one (current_org() IS NULL, which only background code is) sees all.
ALTER TABLE usage_outbox ENABLE ROW LEVEL SECURITY;
ALTER TABLE usage_outbox FORCE ROW LEVEL SECURITY;
CREATE POLICY usage_outbox_tenant_isolation ON usage_outbox
  USING (org_id = current_org() OR current_org() IS NULL)
  WITH CHECK (org_id = current_org() OR current_org() IS NULL);

ALTER TABLE usage_outbox_rejected ENABLE ROW LEVEL SECURITY;
ALTER TABLE usage_outbox_rejected FORCE ROW LEVEL SECURITY;
CREATE POLICY usage_outbox_rejected_tenant_isolation ON usage_outbox_rejected
  USING (org_id = current_org() OR current_org() IS NULL)
  WITH CHECK (org_id = current_org() OR current_org() IS NULL);

ALTER TABLE audit_events ENABLE ROW LEVEL SECURITY;
ALTER TABLE audit_events FORCE ROW LEVEL SECURITY;
CREATE POLICY audit_events_tenant_isolation ON audit_events
  FOR SELECT USING (org_id = current_org());
CREATE POLICY audit_events_append ON audit_events
  FOR INSERT WITH CHECK (current_org() IS NULL OR org_id = current_org());
-- Append-only: there is deliberately no UPDATE policy. DELETE is reachable
-- only unpinned (the org.deleted purge), never from a request.
CREATE POLICY audit_events_retention ON audit_events
  FOR DELETE USING (current_org() IS NULL);

-- ---------------------------------------------------------------------- grants

DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'otto_app') THEN
    -- `SET LOCAL ROLE otto_app` requires the connecting role to be a member.
    BEGIN
      EXECUTE 'GRANT otto_app TO CURRENT_USER';
    EXCEPTION WHEN insufficient_privilege THEN
      RAISE NOTICE
        'otto_app exists but could not be granted to the migrating role. Tenant '
        'transactions will fall back to FORCE ROW LEVEL SECURITY; flags-server '
        'verifies that at startup.';
    END;

    EXECUTE 'GRANT USAGE ON SCHEMA public TO otto_app';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO otto_app';
    EXECUTE 'GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO otto_app';
    EXECUTE 'GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA public TO otto_app';
    EXECUTE 'ALTER DEFAULT PRIVILEGES IN SCHEMA public '
            'GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO otto_app';
    EXECUTE 'ALTER DEFAULT PRIVILEGES IN SCHEMA public '
            'GRANT USAGE, SELECT ON SEQUENCES TO otto_app';

    EXECUTE 'REVOKE UPDATE, DELETE ON audit_events FROM otto_app';
  END IF;
END $$;
