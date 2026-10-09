-- flag_apps: an application registered for flag management. Holds the
-- SDK/server keys and environment list a running app authenticates with —
-- the seam between MCP-driven management (this crate) and SDK-driven
-- evaluation (the evaluation server, not built yet), per
-- docs/specs/2026-09-15-otto-flags-design.md §5.
--
-- org_id lives directly on this table, not joined in from elsewhere: every
-- tenant-scoped table in this database does, because the row-level-security
-- policy added in 0004_rls.sql compares org_id = current_org() on the table
-- itself. There is no foreign key to an `orgs` table — that table lives in
-- otto-platform's own database, and design doc §3 is explicit that no
-- cross-database foreign key exists; org_id is just a UUID in the shared
-- namespace, checked at the application layer.

CREATE EXTENSION IF NOT EXISTS "pgcrypto";

CREATE TABLE flag_apps (
  id           uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  org_id       uuid NOT NULL,
  name         text NOT NULL,
  -- A flat list of environment names this app evaluates flags in (e.g.
  -- {production, staging}) — just names. Per-environment *state* for a given
  -- flag (enabled, rollout percentage, targeting) lives on the flag itself in
  -- feature_flags.environments instead, since it varies per flag rather than
  -- per app.
  environments text[] NOT NULL DEFAULT ARRAY['production'],
  client_key   text NOT NULL,
  server_key   text NOT NULL,
  created_at   timestamptz NOT NULL DEFAULT now(),
  UNIQUE (client_key),
  UNIQUE (server_key)
);

CREATE INDEX flag_apps_org_id_idx ON flag_apps (org_id);
