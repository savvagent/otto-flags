# Maintenance transactions run as the tenant role

**Status:** IMPLEMENTED (#78)
**Issue:** #73

## 1. Brief

From #73 (found by the security review of #72):

> `flags_core::maintenance::begin` opens its transaction as the pool's own role and only sets
> `app.maintenance = 'on'`. Production's pool connects as the superuser attach role, which
> row-level security does not apply to, so the marker-requiring policies (`0003`'s retention reads
> and maintenance deletes, `0002`'s checker policy, and `0005`'s usage-outbox policies) constrain
> nothing in production: background code and a request path that forgot to pin both run
> RLS-exempt. Tenant transactions drop into `otto_app` with `SET LOCAL ROLE`; maintenance
> transactions do not.
>
> **Acceptance criteria**
> - [ ] `maintenance::begin` also issues `SET LOCAL ROLE <tenant role>` (when the tenant role can
>   be assumed, as `otto_tenant::Db::begin` decides), so every maintenance transaction is subject
>   to RLS.
> - [ ] Every maintenance caller (retention sweeps, rollback checker backstop, usage shipper) keeps
>   working under that role; e2e/db tests run them on the default superuser test pool and prove
>   they still see across orgs.
> - [ ] `docs/deploy/fly.md`'s RLS notes stop saying the marker changes nothing in production.

## 2. Premise corrections

- "a request path that forgot to pin ... run RLS-exempt": that stays true after this change. A
  request path that forgot to pin does not go through `maintenance::begin` at all; it opens a
  transaction on `db.pool()` (or `Db::begin_unpinned`), which still runs as the pool's role. This
  change makes the *maintenance* side honour the policies; it cannot make an unpinned
  `db.pool()` query subject to RLS under a superuser pool. The marker policies keep a forgotten
  pin out only under a non-exempt pool role. Not widened here (Stop & Escalate 13: a follow-up,
  if wanted, is "route every unpinned transaction through a role drop").
- "as `otto_tenant::Db::begin` decides": that decision is `Db::tenant_role_assumable`, which is
  private to `otto-tenant` (pinned rev `fa1ae08`). `maintenance::begin` cannot call it. It
  re-asks the catalog the same question with the same query, against the same role
  (`Db::tenant_role()`, public). Changing `otto-tenant` would be an otto-platform pin bump, which
  the section says is its own PR.

## 3. Assumptions

- **The probe runs on every maintenance transaction, uncached.** `Db` keeps its own probe result
  in a private `OnceCell`; flags-core has nowhere per-`Db` to cache one (a process-global cache
  would be wrong across the several `Db`s tests build over different pools). Maintenance
  transactions are few (the shipper's two per batch, one per retention statement, one per checker
  scan), so one catalog lookup each is negligible on the 5-connection pool. The probe and the
  marker go in one statement, so a maintenance transaction costs one extra round trip
  (`SET LOCAL ROLE`) over today, and none when the role is not assumable.
- **Role name interpolation is safe.** `SET LOCAL ROLE` takes no bind parameter; the name comes
  from `Db::tenant_role()`, which `otto-tenant` validates (`^[a-z_][a-z0-9_]*$`, ≤ 63 bytes)
  before it can be set, exactly as `Db::begin` relies on.
- **Not assumable means no role change**, as in `Db::begin`, **unless the pool's role is exempt
  from RLS** (superuser or BYPASSRLS): then `begin` fails (`sqlx::Error::Configuration`). A server
  only boots when `verify_tenant_isolation` passes, which with the role not assumable already
  requires a non-exempt pool role, but `Db` checks that once and caches it, while this checks every
  transaction: a superuser whose `otto_app` membership is revoked at runtime fails closed instead
  of sweeping unconstrained (added after PR review, #78). So every maintenance transaction that
  runs is subject to RLS.
- Run Profile: every value comes from `CLAUDE.md` > Development Workflow; nothing assumed.

## 4. Goal & success criteria

Every maintenance transaction runs as the tenant role when it can be assumed, so the
marker-requiring policies constrain the background code in production as they do in tests.

- A maintenance transaction on the default superuser test pool reports `current_user` =
  `otto_app` and `app.maintenance` = `on`.
- On a `Db` whose tenant role cannot be assumed, `maintenance::begin` still opens (no role change),
  as `Db::begin` does, when the pool's role is subject to RLS, and refuses when it is exempt.
- The role and the marker end with the transaction, committed or rolled back.
- On the default superuser test pool, the retention sweeps, the platform-events sweep (including
  removed-member tombstones), the checker's `due_policies` (already covered by
  `due_policies_sees_every_org_unpinned`), and `ship_once` still act on rows of two different
  orgs.
- A maintenance transaction is refused what `otto_app` is refused (proved by a `DELETE` on
  `audit_events`, from which `otto_app` has DELETE revoked: SQLSTATE `42501`).
- `docs/deploy/fly.md` describes the marker policies as enforced in production.

## 5. Scope

In: `crates/flags-core/src/maintenance.rs`; tests in `crates/flags-core/src/maintenance.rs` (unit,
needs `pub(crate)` access), `crates/flags-core/tests/db.rs`, `crates/flags-server/tests/e2e.rs`;
`docs/deploy/fly.md`; the `CLAUDE.md` invariant line naming `maintenance::begin`.

Out: unpinned `db.pool()` use outside `maintenance::begin` (premise correction above); the
`otto-tenant` pin; any migration; #74 (rejected-usage purge and retention), which builds on this.

## 6. Architecture

`crates/flags-core/src/maintenance.rs`, `begin(db)` (as shipped, after PR review):

```rust
let mut tx = db.pool().begin().await?;
let (_, assumable, pool_exempt, tenant_exempt): (String, bool, bool, bool) = sqlx::query_as(
    "SELECT set_config('app.maintenance', 'on', true), \
            EXISTS (SELECT 1 FROM pg_catalog.pg_roles r WHERE r.rolname = $1 \
                      AND pg_catalog.pg_has_role(current_user, r.oid, 'USAGE')), \
            EXISTS (SELECT 1 FROM pg_catalog.pg_roles r WHERE r.rolname = current_user \
                      AND (r.rolsuper OR r.rolbypassrls)), \
            EXISTS (SELECT 1 FROM pg_catalog.pg_roles r WHERE r.rolname = $1 \
                      AND (r.rolsuper OR r.rolbypassrls))",
)
.bind(db.tenant_role())
.fetch_one(&mut *tx)
.await?;
let runs_exempt = if assumable { tenant_exempt } else { pool_exempt };
if runs_exempt {
    return Err(sqlx::Error::Configuration(/* names the role, no SQL text */));
}
if assumable {
    sqlx::query(&format!("SET LOCAL ROLE {}", db.tenant_role()))
        .execute(&mut *tx)
        .await?;
}
Ok(tx)
```

`set_config(..., true)` is the transaction-local form of `SET LOCAL`. The marker and the probes
are separate select-list columns, so all are always evaluated (folding them into one boolean
expression would let Postgres skip `set_config`). The probes run before the role changes, as the
pool's own role; the assumability probe is the question `Db` asks (`current_user` of the
connection). Exemption (`rolsuper`, `rolbypassrls`) is a role attribute, not inherited through
membership, so each role is checked by name: the transaction is refused when the role it would
run as is exempt. `error::db_reason` logs that refusal as `configuration`.

The signature and visibility are unchanged (`pub(crate)`, same return type), so every caller is
untouched: `platform_events::sweep` (`platform_events.rs:114`), `telemetry::sweep`
(`telemetry.rs:745`), `rollback::due_policies` (`rollback.rs:1004`), and `usage::ship_once` /
`reschedule` (`usage.rs:298`, `:379`, `:446`).

What each caller needs as `otto_app`, all already in place (the existing `otto_app_db` tests run
them as `otto_app` today):

| Caller | Statements | Policy / grant |
|---|---|---|
| `telemetry::sweep` | `DELETE` on `flag_errors`, `flag_eval_hourly`, `rollback_events` with a `WHERE` | 0003/0002 marker reads + deletes; baseline grants |
| `platform_events::sweep` | `DELETE` on `platform_events`; `DELETE` on `removed_members` (no RLS) | 0003; baseline `ALL TABLES` grant |
| `due_policies` | `SELECT` on `rollback_policies` | 0002 `rollback_policies_checker`; 0002 grant |
| `ship_once` / `reschedule` | `UPDATE … FOR UPDATE SKIP LOCKED`, `DELETE` on `usage_outbox`; `INSERT` into `usage_outbox_rejected` (sequence) | baseline permissive + 0005 restrictive; baseline table and sequence grants |

No public interface changes (no endpoint, tool, config key, scope, wire field, or schema).

## 7. Invariants impact

- **Tenant isolation is RLS under `otto_app`, proven**: strengthened. Maintenance transactions
  now run under `otto_app` too, so the unpinned policies are what grant them access.
- **Request paths open transactions with `begin_live`; unpinned cross-org access only through
  `maintenance::begin`**: unchanged rule; the `CLAUDE.md` line gains "as the tenant role".
- **No SQL outside flags-core; runtime queries only**: holds (`query_as`, `query` with a
  `format!` of a validated identifier, as `otto-tenant` does).
- **Small shared database**: one extra round trip per maintenance transaction; no new connection
  held, no fan-out.
- **Append-only migrations / rollout compatibility**: no migration. During rollout the previous
  release keeps running maintenance as the pool role, which the policies already allow; after a
  rollback the same.

## 8. Migration

None.

## 9. Out-of-band

None: no secret, config key, scope, or registration. Deploy-safety: production's `otto_app`
must hold the grants in the table above. Production logs `tenant isolation enforced as role
"otto_app"` and serves pinned tool writes on these tables today, and the sequence and
`removed_members` grants come from the same baseline statements; post-deploy verification reads
the logs for shipper, sweep, and checker failures (SQLSTATE `42501`, or `reason="configuration"`
for the refusal).

## 10. Error handling & edge cases

- The probe or `SET LOCAL ROLE` failing returns `sqlx::Error` from `begin`, as the marker statement
  could before; every caller already handles a `begin` error (logs and retries next tick).
- The role the transaction would run as is exempt from RLS: `begin` returns
  `sqlx::Error::Configuration` (reason `configuration`), and the callers log and retry as above;
  the shipper keeps its rows. It never reaches a tool or SDK response: only background loops call
  these functions.
- A pool whose connections already `SET ROLE otto_app` at session level (the tests' `otto_app_db`):
  `current_user` is `otto_app`, a member of itself, so the probe is true and `SET LOCAL ROLE
  otto_app` is a no-op.
- `Db::with_tenant_role` with another role: honoured, since the probe and the `SET` use
  `db.tenant_role()`.
- `SET LOCAL` ends with the transaction; nothing leaks to the next user of the connection.

## 11. Testing approach

Gates: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`,
`cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check`, the
migrations extra gate, `podman build -t otto-flags:check .` (crates changed).

- Unit, `crates/flags-core/src/maintenance.rs` (`#[sqlx::test(migrator = "crate::MIGRATOR")]`,
  since `begin` is `pub(crate)`):
  - on the default pool: `current_user` = `otto_app`, `current_setting('app.maintenance')` =
    `on`, and `DELETE FROM audit_events` fails with `42501`;
  - not assumable (`with_tenant_role("flags_no_such_role")`): on a pool whose sessions run as
    `otto_app` (subject to RLS) `begin` succeeds as that role with the marker set; on the
    superuser pool it fails with `sqlx::Error::Configuration`;
  - on a one-connection pool, after commit and after rollback, the pool's role is back and the
    marker is unset.
- `crates/flags-core/tests/db.rs`: a new test on the default (superuser) pool with two orgs:
  `telemetry::sweep`, `platform_events::sweep` delete both orgs' expired rows (and removed-member
  tombstones) and keep fresh ones. `due_policies` across orgs on that pool is the existing
  `due_policies_sees_every_org_unpinned`.
- `crates/flags-server/tests/e2e.rs`: `ship_once` on the default pool ships usage rows from two
  orgs (both delivered, outbox empty), and a rejected one lands in `usage_outbox_rejected`.

## 12. Risks & open questions

- A grant production lacks would fail a maintenance caller after deploy (shipper retries with
  backoff, nothing is lost; sweeps retry next tick). Mitigated by the post-deploy log check;
  fixing a missing grant would be a new migration.
- The probe query is a second copy of `otto-tenant`'s. If `otto-tenant` changes its decision, this
  one would not follow. Recorded in the doc comment; a later pin bump could expose the probe.
