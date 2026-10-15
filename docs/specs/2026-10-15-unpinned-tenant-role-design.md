# Unpinned queries run as the tenant role

**Status:** IMPLEMENTED (#100)
**Issue:** #80

## 1. Brief

From #80 (found by the reviews of #78):

> After #73, pinned transactions (`begin_live`) and maintenance transactions
> (`maintenance::begin`) both run as `otto_app`, so row-level security applies to them. A query
> run directly on `db.pool()`, or through `Db::begin_unpinned`, still runs as the pool's own role.
> In production that role is the superuser attach role, which RLS does not apply to. Today every
> such query touches only the tables deliberately left outside RLS: `keys::resolve_hash`
> (`app_keys`, `deleted_orgs`), `platform_events::revoked` (`deleted_orgs`, `removed_members`), the
> tombstone inserts in `org_deleted` and `member_removed`, and `health`, `migrate`, the `notify`
> listener. So nothing leaks now. But a future raw-pool query that names a tenant table by mistake
> would read every org's rows in production, unconstrained.
>
> **Acceptance criteria**
> - [ ] Unpinned access in flags-core goes through one helper that drops into the tenant role (or
>   refuses an RLS-exempt role it cannot leave), as `maintenance::begin` does but without the
>   maintenance marker. The raw `db.pool()` uses above move onto it, or each one records why it
>   is exempt.
> - [ ] A test (or a grep-based check in `crates/flags-core/tests/db.rs`) fails when a new
>   `db.pool()` query appears outside the allowed sites.
>
> **Out of scope:** changing the deliberate non-RLS tables (`app_keys`, `deleted_orgs`,
> `removed_members`).

## 2. Premise corrections

- `Db::begin_unpinned` has no caller in this repo (it is used only inside otto-platform's own
  crates). The check below still refuses a new one.
- `health` and the `notify` listener's pool are in `flags-server` (`health.rs`, `main.rs`), not
  flags-core. The check covers every workspace crate's `src`, so they are recorded as allowed
  sites there.

## 3. Assumptions

- **No migration.** `0001_baseline.sql` grants `SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN
  SCHEMA public TO otto_app`, which covers `app_keys`, `deleted_orgs`, and `removed_members`
  (created in the same migration); `0004` already relies on `otto_app` writing `deleted_orgs`.
  None of the three has RLS enabled, so `otto_app` reads and writes them unconstrained, exactly as
  the pool role did. No role or grant change on production is needed.
- **The probe is shared, not copied again.** `maintenance::begin`'s catalog probe (a copy of
  otto-tenant's private `tenant_role_assumable`, plus the exemption checks) moves into the new
  module as the one implementation; `maintenance::begin` becomes a call to it with the marker on.
  #79 later replaces the probe with otto-tenant's own.
- **Read-only lookups commit** rather than drop the transaction, so the connection goes back to
  the pool clean without sqlx's deferred rollback.
- Run Profile: every key is set in the section; none assumed.

## 4. Goal and success criteria

Every unpinned query in the server runs as a role row-level security applies to, or sits at a
recorded allowed site that touches no table a tenant role could be constrained on.

- `flags_core::unpinned::begin(db)` opens an unpinned transaction as `db.tenant_role()` when
  assumable, keeps a non-exempt pool role otherwise, and refuses (`sqlx::Error::Configuration`)
  when the transaction would run exempt from RLS. It sets no marker.
- `keys::resolve_hash`, `platform_events::revoked`, and the two tombstone writes use it; their
  behavior is unchanged (existing `tests/db.rs` and `e2e.rs` tests stay green).
- Inside it, a tenant table returns no rows even on the superuser test pool (proof the role drop
  happened and no marker is set).
- `no_raw_pool_queries_outside_the_allowed_sites` in `tests/db.rs` fails on any new `.pool()` or
  `begin_unpinned` in non-test code of any workspace crate's `src` outside the allowlist.

## 5. Scope

**In:** new `crates/flags-core/src/unpinned.rs`; `maintenance.rs` delegates to it; the four call
sites; the check; `CLAUDE.md` invariant line and `docs/deploy/fly.md` RLS note.

**Out:** the non-RLS tables themselves; otto-tenant changes (#79); `PgPool` values passed around
outside `Db` (the notify listener takes a cloned pool; it only `LISTEN`s); `flags-server` test
code; performance tuning of the extra round trips (see Risks).

## 6. Architecture

`crates/flags-core/src/unpinned.rs` (new, `pub(crate)` module):

```rust
pub(crate) async fn begin(db: &Db) -> sqlx::Result<Transaction<'static, Postgres>>  // no marker
pub(crate) async fn begin_as_tenant_role(db: &Db, maintenance: bool) -> sqlx::Result<…>  // shared
```

`begin_as_tenant_role` is today's `maintenance::begin` body (`maintenance.rs:48-91`) with two
changes. The marker is conditional: `CASE WHEN $2 THEN set_config('app.maintenance', 'on', true)
END` (a volatile function inside a CASE arm is evaluated only when the arm is taken), decoded as
`Option<String>`. And the role switch is folded into the same statement, after review flagged the
round trips on the unauthenticated SDK path: the probes are a `MATERIALIZED` CTE, so they read
`current_user` before the outer select list runs `CASE WHEN assumable THEN set_config('role', $1,
true) END`, which is `SET LOCAL ROLE` with a bind parameter instead of interpolated SQL. A refused
result returns `Err` and the dropped transaction rolls the switch back. The error text names the
kind ("maintenance" or "unpinned"). `maintenance::begin` becomes
`crate::unpinned::begin_as_tenant_role(db, true)`; only it passes `true`. Its tests stay where
they are.

Call sites:

| Site | Tables | Change |
|---|---|---|
| `keys::resolve_hash` (`keys.rs:124`) | `app_keys`, `deleted_orgs` | `unpinned::begin`, query, commit |
| `platform_events::revoked` (`platform_events.rs:64`) | `deleted_orgs`, `removed_members` | same; `revoked_on(&mut tx, …)` |
| `org_deleted` tombstone (`platform_events.rs:179`) | `deleted_orgs` | same; committed before the purge transaction, as now |
| `member_removed` tombstone (`platform_events.rs:243`) | `removed_members` | same |
| `migrate::migrate` (`migrate.rs:48`) | schema | **exempt**: migrations must run as the migrating role (DDL, `GRANT`, the `SECURITY DEFINER` purge function owner) |
| `maintenance` / `unpinned` helper | — | **the helper itself** (one `.pool().begin()`) |
| `flags-server` `readyz` (`health.rs:51`) | none (`SELECT 1`) | **exempt**: asks about the connection, touches no table, must not fail on role config so `/readyz` reports DB reachability |
| `flags-server` `main.rs` (`PgPoolOptions`, the listener's pool clone) and `notify.rs` (`PgPool`) | none (`LISTEN`) | **exempt**: builds the pool that becomes the `Db`; the listener's dedicated connection only `LISTEN`s |

`flags-api` `stream.rs` `still_valid`: a `Configuration` error from the key re-check now ends the
stream, like a gone key. It is persistent, not an outage, and while it lasts a rotated key could
not be re-checked at all.

Check: a plain `#[test]` in `tests/db.rs` walks `crates/*/src/**/*.rs` from
`CARGO_MANIFEST_DIR/..`, normalizes line endings, drops each file's trailing `#[cfg(test)] mod
tests` (asserting it is the last item: after its header only its closing brace sits at column 0),
drops comment lines, and counts `.pool()`, `begin_unpinned`, `audit_global(`, `audit_for_org(`
(otto-tenant helpers that write on the pool), and `PgPool` per file. The result must equal the
allowlist exactly (path to count, each with its reason in a comment), so a new site fails and a
removed one prompts trimming the list.

No public interface changes: the helper is `pub(crate)`; `resolve_hash`/`revoked` keep their
signatures and errors (`Error::Db`).

## 7. Invariants impact

- **Tenant isolation, RLS under `otto_app`:** strengthened; unpinned non-maintenance queries now
  run as `otto_app` and read no tenant row (no pinned org, no marker). Writes are what
  each tenant table's policies allow unpinned: `audit_events_append` admits an unpinned INSERT, as
  it does for any `otto_app` transaction today; no caller of the helper writes a tenant table.
- **Request paths use `begin_live`; unpinned cross-org only via `maintenance::begin`:** unchanged.
  `unpinned::begin` grants no cross-org access to tenant tables; it is for the pre-org tables.
  `CLAUDE.md` gets one clause naming it.
- **Small shared database:** no new connection or fan-out; each lookup holds its pooled connection
  for a few more round trips (BEGIN, probe with role switch, query, COMMIT).
- **SDK path never calls the platform:** unchanged (database only).
- **Migrations:** none.

## 8. Migration

None (grants already cover the three tables; see Assumptions).

## 9. Out-of-band

None: no secret, config key, scope, or migration. Rollback-compatible trivially (no schema change).

## 10. Error handling and edge cases

- Pool role exempt and tenant role not assumable, or tenant role exempt: key resolution,
  revocation checks, and webhook tombstones fail with `Error::Db(Configuration)`, answered as
  each surface answers any non-transient database error (a 5xx; the webhook's makes the platform
  retry) instead of running
  unconstrained. Startup's `verify_tenant_isolation` already refuses that configuration, so this
  is reachable only after a runtime role change; it fails closed, as maintenance does.
- `revoked` failing is already answered 503, never "not revoked".
- The tombstone commit precedes the purge transaction exactly as before, so ordering and
  idempotency are unchanged.

## 11. Testing

- Unit (`unpinned.rs`, `#[sqlx::test]`): runs as the tenant role with no marker; a tenant table
  (`flag_apps`) with a row returns none inside it; the tombstone tables are readable and writable;
  an exempt role that cannot assume the tenant role is refused.
- `maintenance.rs` tests unchanged (they now exercise the shared path with the marker).
- Existing callers' tests in `tests/db.rs` and `flags-server/tests/e2e.rs` cover behavior.
- `tests/db.rs`: `no_raw_pool_queries_outside_the_allowed_sites`.
- Gates per the section; DB tests against the dedicated local Postgres.

## 12. Risks and open questions

- Latency and pool hold time: an SDK key resolution goes from one round trip to four (BEGIN,
  probe with role switch, query, COMMIT), before any key is proven valid. Accepted: the public
  `sdk_` client key already lets anyone drive the full `begin_live` evaluate path, which costs
  more; on Fly's private network to a co-located Postgres the added time is low single-digit
  milliseconds. Rate limiting or a negative key cache is a separate concern, not taken here;
  caching the probe waits for #79 (otto-tenant's own probe).
- The check is textual: an alias (`let p = db.pool()` is caught, but a pool passed on from an
  allowed site is not) or a reformatted call (`Db::pool(&db)`) escapes it. It guards the common
  mistake the issue names.
