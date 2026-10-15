# Tenant and maintenance transactions resolve names in public only

**Status:** DRAFT
**Issue:** #97

## 1. Brief

From #97 (found by the security review of the #53 fix):

> #53 qualified the audit purge function call. The tenant transactions still use unqualified
> table names (`deleted_orgs`, `platform_events`, the purge loop's tables, and every tenant
> query), so Postgres resolves them through `otto_app`'s `search_path` (default
> `"$user", public`). A schema named `otto_app` holding same-named tables would redirect them, as
> #53 described for the function. Creating such a schema already needs a privileged role, so this
> is defense in depth, not a live hole.

Acceptance criteria (#97):

- Tenant and maintenance transactions resolve names with `public` only: for example
  `SET LOCAL search_path = public` where the transaction assumes `otto_app`, or a role-level
  setting applied by the human-only path in `docs/deploy/fly.md`. The spec picks one.
- A DB test with an `otto_app` schema shadowing a tenant table shows queries still reach `public`.
- No migration, or an additive one only.

Caller constraints: prefer the in-transaction setting over `ALTER ROLE` (a role change on the
production database is human-only); fold it into an existing statement rather than adding round
trips, because #104 is about the SDK key path's round-trip count.

## 2. Premise corrections

- "Where the transaction assumes `otto_app`" is not the only case. Both `otto_tenant::Db::begin`
  and `crate::unpinned::begin_as_tenant_role` keep the pool's own role when the tenant role cannot
  be assumed (a managed Postgres role subject to RLS). That role's `search_path` has the same
  `"$user"` entry. The setting is therefore applied on every tenant, unpinned, and maintenance
  transaction, whether or not the role switch happened.
- Pinned transactions are opened by `otto_tenant::Db::begin` (an otto-platform crate, pinned by
  rev). This repo cannot change that function, so the setting is applied in flags-core right after
  it, before any caller query runs. `Db::begin` itself runs only `SET LOCAL ROLE` and
  `pg_catalog.set_config`, neither of which resolves a relation through `search_path`.
- `pg_temp` is searched before every listed schema for relations unless it is listed explicitly.
  The value chosen lists it last, as migration `0004_audit_purge.sql` does for the purge
  function's own `search_path`.

## 3. Assumptions

- **Value:** `pg_catalog, public, pg_temp`. `pg_catalog` is searched first even when omitted, so
  this resolves relations, functions, and types in `public` only (after the built-ins), and
  `pg_temp` last. It matches `0004_audit_purge.sql:24`, so there is one spelling in the repo.
- **Mechanism:** `set_config('search_path', <value>, true)`, transaction-local (the same as
  `SET LOCAL`), chosen over `ALTER ROLE otto_app SET search_path` because the agent controls it,
  it applies to the non-assumed fallback role too, it needs no production role change (human-only),
  and it ends with the transaction like the role and marker do. No hand-off.
- **Round trips:** the SDK key path and every maintenance transaction gain none (a new column on
  the existing probe statement). Every request transaction (`begin_live`) gains none (a new column
  on the existing lifecycle-lock statement). The webhook handlers' pinned transactions and the
  boot-time purge check gain one statement each; they run once per platform event or once per
  boot.
- Run Profile values were all read from the section; none assumed.

## 4. Goal and success criteria

Every transaction flags-core opens against the database resolves unqualified names in `public`,
never in a schema named after the current role or created later on the search path.

- `crate::unpinned::begin_as_tenant_role` (and so `unpinned::begin`, `maintenance::begin`) sets
  the path in its existing probe statement.
- `platform_events::begin_live` sets it in its existing lifecycle-lock statement.
- Every other pinned transaction goes through one helper that sets it; a source-scan test refuses
  a new bare `db.begin(` elsewhere.
- DB tests with an `otto_app` schema shadowing `deleted_orgs`, `app_keys`, `platform_events`, and
  `flag_apps` show the unpinned, maintenance, request, and webhook paths read and write `public`.
- No migration; no change to round trips on the SDK key path.

## 5. Scope

In: the transaction-opening code in `crates/flags-core/src` and its tests.

Out:
- `ALTER ROLE … SET search_path` on production (not needed; would be human-only).
- Schema-qualifying every query (thousands of call sites; the setting covers them).
- The migrator's connection (runs as the migrating role, not a tenant transaction) and the
  `LISTEN` connection in `notify.rs` (touches no table).
- `/readyz`'s `SELECT 1` (touches no table).
- otto-tenant itself (a change there belongs to otto-platform).

## 6. Architecture

New constant, defined once, in `crates/flags-core/src/unpinned.rs` (the module already owns the
"every transaction drops into the tenant role" rules):

```rust
/// The `search_path` every flags-core transaction runs with (#97). …
pub(crate) const SEARCH_PATH: &str = "pg_catalog, public, pg_temp";
```

1. **Unpinned and maintenance** (`unpinned.rs:50`, `begin_as_tenant_role`): the probe's outer
   select list gains a column `set_config('search_path', $3, true)`, bound to `SEARCH_PATH`,
   unconditional (no CASE: it applies whether or not the role is switched). The decoded tuple
   gains one `Option<String>`/`String` slot that is ignored. If the transaction is then refused,
   the rollback discards the setting with everything else.
2. **Request transactions** (`platform_events.rs:94`, `begin_live`): the lifecycle-lock statement
   becomes `SELECT pg_advisory_xact_lock_shared($1, hashtext($2::text)), set_config('search_path',
   $3, true)`. The lock statement's own names are `pg_catalog` functions, so it does not matter
   that it is parsed before the setting applies. The tombstone re-check that follows runs under
   the new path.
3. **Other pinned transactions**: a new `pub(crate) async fn begin_pinned(db: &Db, org: OrgId) ->
   Result<Tx<'static>>` in `platform_events.rs` runs `db.begin(org)` then
   `SELECT set_config('search_path', $1, true)`. The three webhook handlers (`team.deleted`,
   `org_deleted`, `member_removed`; `platform_events.rs:148, 192, 267`) and
   `audit_purge::check` (`audit_purge.rs:218`) call it instead of `db.begin`. `begin_live` keeps
   its own `db.begin` (it folds the setting).
4. **Guard**: a new source-scan test in `crates/flags-core/tests/db.rs`,
   `no_pinned_begin_outside_the_search_path_helpers`, counts `db.begin(` in every crate's
   non-test `src` (same walker as `no_raw_pool_queries_outside_the_allowed_sites`, factored into a
   shared function) and allows exactly `flags-core/src/platform_events.rs: 2` (`begin_live` and
   `begin_pinned`). The pool-begin site in `unpinned.rs` is already pinned to one by the existing
   scan.

No public interface changes: `begin_pinned` and `SEARCH_PATH` are `pub(crate)`; `begin_live`'s
signature is unchanged. No wire, MCP, or SDK change.

## 7. Invariants impact

- **Tenant isolation is RLS under `otto_app`, proven**: strengthened; no policy, grant, or role
  change. The setting is transaction-local and ends at commit or rollback, like the role and the
  marker, so it never reaches the next user of a pooled connection (a test pins this).
- **Request paths open transactions with `begin_live`**: unchanged; `begin_pinned` is
  `pub(crate)` and used only by webhook handlers and the boot check, which already called
  `Db::begin` directly.
- **No SQL outside `flags-core`**: holds; every change is in flags-core.
- **The database is small and shared**: no extra round trip on the SDK, request, or maintenance
  paths; one on rare webhook and boot paths.
- **Append-only migrations / rollback compatibility**: no migration.
- **SDK key resolution never calls the platform**: unchanged.

## 8. Migration

None.

## 9. Out-of-band

None. No secret, config key, scope, or role change. (The rejected alternative,
`ALTER ROLE otto_app SET search_path = pg_catalog, public, pg_temp`, would have been a production
role change for Rob.)

## 10. Error handling and edge cases

- `set_config` cannot fail on a valid literal; if the statement errors, the existing `?` returns
  the error and the dropped transaction rolls back, as today.
- A refused unpinned transaction rolls back explicitly before the error is returned, discarding
  the setting.
- A schema named `public` missing or renamed would break every query today too; not a new case.
- `pg_temp` last: a temporary table of the same name created in the same session can no longer
  shadow a `public` table.

## 11. Testing approach

Gates: `DATABASE_URL=postgres://flags:flags@localhost:<port>/otto_flags cargo test --workspace`,
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`, the
migrations-unchanged check, and `podman build -t otto-flags:check .` (crates changed).

New tests:
- `unpinned.rs` unit tests: `begin` and `maintenance::begin` report
  `current_setting('search_path') = SEARCH_PATH`; it ends with the transaction (extend
  `the_role_and_marker_end_with_the_transaction`).
- `tests/db.rs`, shared fixture `shadow_schema(pool, tables)`: as the superuser test role,
  `CREATE SCHEMA otto_app AUTHORIZATION otto_app` and `CREATE TABLE otto_app.<t> (LIKE public.<t>)`
  owned by `otto_app`, with planted rows. Then:
  - `unpinned_lookups_ignore_a_shadowing_schema`: planted `otto_app.deleted_orgs` row for org X;
    `platform_events::revoked(X, user)` is false; planted `otto_app.app_keys` row for a fresh key;
    `keys::resolve` of that key is not found.
  - `request_transactions_ignore_a_shadowing_schema`: planted `otto_app.deleted_orgs` row;
    `begin_live(X)` succeeds and a `flag_apps` read in it sees the `public` row, not a planted
    `otto_app.flag_apps` row.
  - `maintenance_transactions_ignore_a_shadowing_schema`: planted old `otto_app.platform_events`
    row; `platform_events::sweep` deletes the old `public` row and leaves the planted one.
  - `webhooks_ignore_a_shadowing_schema`: `team.deleted` delivered twice records its marker in
    `public.platform_events` (second is `Duplicate`) and writes nothing to the shadow table.
  Each must fail before the fix (verified red first).
- Source scan `no_pinned_begin_outside_the_search_path_helpers`.

## 12. Risks and open questions

- A future transaction opener that bypasses both helpers: caught by the source scans.
- otto-tenant could later add its own `search_path` handling (#79 moves the role probe there);
  the flags-core setting stays harmless if it does.
