-- Row-level security for otto-flags' own domain database — its own copy of
-- otto-tenant's 0004_rls.sql pattern (see docs/specs/2026-09-15-otto-flags-
-- design.md §3: "each service's domain database runs the same RLS pattern
-- ... its own otto_app role, its own current_org(), its own tenant_tables
-- registry"). This is a distinct physical database from otto-platform's, so
-- the role and function have to be created here too — they do not carry over
-- from otto-platform's database.
--
-- The role name (`otto_app`) and the `app.org_id` session variable are not
-- free choices: `otto_tenant::Db::begin` (the Rust code this migration backs,
-- reused unmodified from otto-platform) hardcodes both. Anything else here
-- can vary per service; those two strings cannot.

DO $$
DECLARE
  have_role boolean;
BEGIN
  BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'otto_app') THEN
      CREATE ROLE otto_app NOLOGIN;
    END IF;
  EXCEPTION WHEN insufficient_privilege THEN
    RAISE NOTICE
      'could not CREATE ROLE otto_app (no CREATEROLE). Tenant transactions '
      'will run as the connecting role and rely on FORCE ROW LEVEL SECURITY, '
      'which holds only while that role is neither a superuser nor '
      'BYPASSRLS. flags-server verifies this at startup and refuses to serve '
      'otherwise.';
  END;

  have_role := EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'otto_app');

  IF have_role THEN
    BEGIN
      EXECUTE 'GRANT otto_app TO CURRENT_USER';
    EXCEPTION WHEN insufficient_privilege THEN
      RAISE NOTICE
        'otto_app exists but could not be granted to the migrating role. '
        'Tenant transactions will fall back to FORCE ROW LEVEL SECURITY; '
        'flags-server verifies that at startup.';
    END;

    EXECUTE 'GRANT USAGE ON SCHEMA public TO otto_app';
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO otto_app';
    EXECUTE 'GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO otto_app';
    EXECUTE 'GRANT EXECUTE ON ALL FUNCTIONS IN SCHEMA public TO otto_app';
    EXECUTE 'ALTER DEFAULT PRIVILEGES IN SCHEMA public '
            'GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO otto_app';
    EXECUTE 'ALTER DEFAULT PRIVILEGES IN SCHEMA public '
            'GRANT USAGE, SELECT ON SEQUENCES TO otto_app';
  END IF;
END $$;

CREATE OR REPLACE FUNCTION current_org() RETURNS uuid AS $$
  SELECT NULLIF(current_setting('app.org_id', true), '')::uuid;
$$ LANGUAGE sql STABLE;

DO $$
DECLARE
  t text;
  -- Every table in THIS database whose rows belong to exactly one tenant.
  -- otto-flags has no users/orgs/auth tables at all (those live in
  -- otto-platform's database) so, unlike otto-tenant's own 0004_rls.sql,
  -- there is nothing to deliberately exclude here.
  tenant_tables text[] := ARRAY[
    'flag_apps',
    'feature_flags',
    'flag_evaluations'
  ];
BEGIN
  FOREACH t IN ARRAY tenant_tables LOOP
    EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
    EXECUTE format('ALTER TABLE %I FORCE ROW LEVEL SECURITY', t);
    EXECUTE format(
      'CREATE POLICY %I ON %I USING (org_id = current_org()) WITH CHECK (org_id = current_org())',
      t || '_tenant_isolation', t
    );
  END LOOP;
END $$;
