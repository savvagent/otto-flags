-- feature_flags: key, name, per-environment state, and variations for one
-- flag, scoped to one flag_apps row. Per design doc §5's "simplified rather
-- than ported as-is": savvagent-flags kept a whole second `archived_flags`
-- table mirroring this one plus a trigger enforcing mutual exclusion, to keep
-- the "active" table small. That is real complexity for a problem this
-- database does not have yet — a status/archived_at column on this table
-- does the same job.

CREATE TYPE flag_status AS ENUM ('active', 'archived');

CREATE TABLE feature_flags (
  id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  org_id       uuid NOT NULL,
  app_id       uuid NOT NULL REFERENCES flag_apps (id) ON DELETE CASCADE,
  key          text NOT NULL,
  name         text NOT NULL,
  description  text,
  status       flag_status NOT NULL DEFAULT 'active',
  -- Per-environment state (enabled, rollout percentage, targeting rules),
  -- keyed by the environment name — e.g. {"production": {"enabled": true,
  -- "rolloutPercent": 50}, "staging": {"enabled": true}}. jsonb rather than a
  -- normalized table: the shape varies by targeting-rule type and is read
  -- and written as a whole by the MCP tools that own it (set_targeting_rule,
  -- advance_rollout), never queried by sub-field from SQL.
  environments jsonb NOT NULL DEFAULT '{}'::jsonb,
  -- Variation definitions this flag can resolve to, keyed by variation name
  -- — e.g. {"control": {"value": false}, "treatment": {"value": true}}.
  variations   jsonb NOT NULL DEFAULT '{}'::jsonb,
  -- Bumped on every update. Exists so a caller can detect "someone else
  -- changed this flag since I last read it" (optimistic concurrency) without
  -- otto-flags needing a lock; not yet enforced by any query in this crate.
  version      integer NOT NULL DEFAULT 1,
  created_at   timestamptz NOT NULL DEFAULT now(),
  updated_at   timestamptz NOT NULL DEFAULT now(),
  archived_at  timestamptz,
  UNIQUE (app_id, key)
);

CREATE INDEX feature_flags_org_id_idx ON feature_flags (org_id);
CREATE INDEX feature_flags_app_id_idx ON feature_flags (app_id);
