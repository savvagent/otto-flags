-- flag_evaluations: the raw evidence base for correlation, health
-- monitoring, and risk scoring (design doc §5). This is the ingestion table
-- only — analytics rollup/aggregation tables are deliberately not part of
-- this migration; they get added once a concrete agent workflow
-- (assess_risk, correlate_errors) needs a specific shape to query, per the
-- same "don't build ahead of a real caller" judgment design doc §5 applies to
-- cohort_analysis_system.

CREATE TABLE flag_evaluations (
  id             uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  org_id         uuid NOT NULL,
  app_id         uuid NOT NULL REFERENCES flag_apps (id) ON DELETE CASCADE,
  flag_id        uuid NOT NULL REFERENCES feature_flags (id) ON DELETE CASCADE,
  environment    text NOT NULL,
  variation_key  text NOT NULL,
  -- Arbitrary evaluation-context attributes (user id, targeting attributes
  -- used to resolve the variation) — jsonb because the shape is caller-
  -- defined per flag, not a fixed column set.
  context        jsonb NOT NULL DEFAULT '{}'::jsonb,
  evaluated_at   timestamptz NOT NULL DEFAULT now()
);

CREATE INDEX flag_evaluations_org_id_idx ON flag_evaluations (org_id);
CREATE INDEX flag_evaluations_flag_id_evaluated_at_idx
  ON flag_evaluations (flag_id, evaluated_at DESC);
