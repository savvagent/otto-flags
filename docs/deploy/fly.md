# Hosting: Fly.io

otto-flags runs as one Fly app, `otto-flags` (org `savvagent`, region `iad`), serving every
surface on one port: the MCP endpoint agents use, the SDK API running apps use, and the
platform's webhooks.

## Topology

```
  otto-platform  (https://otto.savvagent.com)         otto-flags  (https://otto-flags.savvagent.com)
  OAuth AS, orgs, members, billing                     POST /mcp          agents, bearer tokens
  db: otto_platform ◄──── same instance ────►          /api/...           SDKs, sdk_/srv_ keys
                                                       db: otto_flags     apps, flags, versions, telemetry
        ▲  POST /oauth/introspect, GET /internal/orgs/... ◄──── per MCP request (cached 60 s)
        │  POST /internal/usage                          ◄──── background shipper (usage_outbox)
        └─► POST /platform/webhooks (org.deleted, member.removed, team.deleted) ─────►
```

The SDK path (`/api/...`) never calls the platform: an SDK key resolves against this
database alone, so a platform outage does not affect flag evaluation.

## Infrastructure

- **App** `otto-flags`: shared-cpu-1x, 256 MB, `auto_stop_machines = "suspend"`,
  `min_machines_running = 0`. Idle costs nothing; the first request after idle resumes the
  machine. Raise `min_machines_running` to 1 once production apps depend on it.
- **Database** `otto_flags` on `otto-db`, the unmanaged Fly Postgres shared with
  `otto_platform` and `otto_factory`. The attach role is a superuser on the whole
  instance, so isolation inside this database rests on `SET LOCAL ROLE otto_app`, which
  `flags-server` proves at boot (`tenant isolation enforced as role "otto_app"`). The pool
  is capped at 5 connections (`FLAGS_DB_MAX_CONNECTIONS`) because the instance is small and
  shared. Split onto a dedicated instance before real customers.
- **Migrations** are otto-flags' own (`crates/flags-core/migrations`) and run at boot under
  an advisory lock. Never run the platform's migrations here.
- **The migrating role must stay exempt from RLS** (superuser or `BYPASSRLS`), as the attach
  role is. Migration `0004` (#21) makes it the owner of `purge_deleted_org_audit`, the only way
  the `org.deleted` purge can remove a deleted org's append-only audit trail. Under an owner
  that is not exempt, the function refuses (SQLSTATE `55000`), so `flags-server` checks the
  owner at boot, next to the isolation proof (#40): a healthy boot logs `audit purge ready:
  public.purge_deleted_org_audit(uuid) is owned by "…" (superuser) and executable by "otto_app"`, and
  any other state ends the boot with `refusing to serve: an org.deleted could not purge the
  org's audit trail: …`, naming the fix (the deploy fails; check `fly status` as under When a migration fails). The same check refuses a
  missing function, or a tenant role without `EXECUTE` on it. If it reports the function missing while
  `_sqlx_migrations` records `0004` as applied, migrations will not re-run it: `0004` creates it
  unqualified, so it landed in the first schema on the migrating role's `search_path`, or it was
  dropped. The refusal names the catalog query that finds it; then, as a superuser and
  human-only, either move 0004's function back (`ALTER FUNCTION
  <schema>.purge_deleted_org_audit(uuid) SET SCHEMA public`) or re-run `0004_audit_purge.sql`
  with `SET search_path = public` first. Before moving off the superuser
  attach role (for example onto a dedicated instance), either give the new migrating role
  `BYPASSRLS` (only when tenant transactions still drop into `otto_app`; where `otto_app`
  cannot be assumed, the migrating role is also the tenant role and the isolation proof refuses
  it), or reassign the function to a role that has it (`ALTER FUNCTION
  public.purge_deleted_org_audit(uuid) OWNER TO …`, human-only), or run migrations separately as such
  a role (`FLAGS_RUN_MIGRATIONS=false`; the serving role then needs table privileges, which
  migrations grant only to `otto_app`, so in practice it must be a member of the migrating
  role). A non-exempt owner is not supported; see
  `docs/specs/2026-10-14-purge-owner-startup-check-design.md`. This rules out a managed
  Postgres that offers no superuser or `BYPASSRLS` role at all: there, nothing can own the
  function and the server will not start.

## Secrets

| Secret | What | Where from |
|---|---|---|
| `DATABASE_URL` | Postgres URL for `otto_flags` | `fly postgres attach` |
| `FLAGS_INTROSPECTION_SECRET` | `otto_rs_…`, authenticates every call to the platform | `resource rotate-secret` |
| `FLAGS_PLATFORM_WEBHOOK_SECRET` | `otto_whsec_…`, verifies platform webhooks | `resource set-webhook` |

Everything else is in `fly.toml`'s `[env]`.

## First deploy (done 2026-10-09)

```bash
# 1. App and database
fly apps create otto-flags --org savvagent
fly postgres attach otto-db -a otto-flags --database-name otto_flags   # sets DATABASE_URL

# 2. Register the resource server at the platform (run on the platform's machine)
fly ssh console -a otto-platform -C 'otto-platform-server resource register \
  https://otto-flags.savvagent.com/mcp --name otto-flags \
  --scopes flags:read,flags:write,apps:admin --default-scopes flags:read'
fly ssh console -a otto-platform -C 'otto-platform-server resource rotate-secret \
  https://otto-flags.savvagent.com/mcp'                 # prints otto_rs_… once
fly ssh console -a otto-platform -C 'otto-platform-server resource set-webhook \
  https://otto-flags.savvagent.com/mcp https://otto-flags.savvagent.com/platform/webhooks'
                                                        # prints otto_whsec_… once

# 3. Secrets, then deploy
fly secrets set -a otto-flags --stage \
  FLAGS_INTROSPECTION_SECRET='otto_rs_…' FLAGS_PLATFORM_WEBHOOK_SECRET='otto_whsec_…'
fly deploy -a otto-flags --remote-only

# 4. Hostname
fly certs add otto-flags.savvagent.com -a otto-flags
# DNS (Namecheap): A and AAAA records for otto-flags -> the app's IPs (`fly ips list -a otto-flags`)
```

The scope list is `flags_core::scopes::KNOWN` and the defaults `flags_core::scopes::DEFAULT`;
keep the registration in step when a release adds a scope. `FLAGS_RESOURCE_URI` must be
exactly the registered URI: it is the token audience and the HTTP Basic user on every
platform call.

## Deploys

Every push to `main` that passes CI deploys (`.github/workflows/ci.yml`, job `deploy`), with
`FLY_API_TOKEN` a deploy token scoped to this app (`fly tokens create deploy -a otto-flags`).
A hand deploy is `fly deploy -a otto-flags --remote-only`.

## When a migration fails

Migrations run at boot, before the server binds its port (`crates/flags-server/src/main.rs`),
under sqlx's advisory lock. This section is about a boot that stops there. A boot that gets past
migrations and then logs `refusing to serve: …` is a different failure: the tenant-isolation
proof or the audit purge check (see Infrastructure).

**Agents diagnose; Rob acts.** Every database write, hand deploy, and deploy re-run below is
**Rob's** (an agent runs a deploy re-run only when Rob asks): an agent stops at the read-only
diagnostics and hands over the exact statement or command. Rob runs SQL as the attach role
through `fly postgres connect -a otto-db -d otto_flags`, which is interactive (it hangs when run
non-interactively, so an agent cannot use it). That role is a superuser on an instance that also
hosts `otto_platform` and `otto_factory`, so every statement below is scoped to `otto_flags`.
Never practise these steps on production; the tests named below run them against a scratch
database.

### What a failure leaves behind

sqlx commits each migration together with its `_sqlx_migrations` row (`flags_core::MIGRATOR`
has no `-- no-transaction` migration, and `no_migration_runs_outside_a_transaction` keeps it so).
A failure rolls back that migration and its row, so:

- **The failed migration leaves nothing: no row, none of its changes.** sqlx never writes a
  `success = false` row on Postgres, so a real failure does not produce `Dirty`. Earlier
  migrations in the same boot committed on their own and stay applied; they are additive
  (`CLAUDE.md` Invariants), so the previous release runs against them.
- **The previous release boots normally.** It does not know the failed version and there is no
  row for it (`ignore_missing`, `crates/flags-core/src/migrate.rs`).
- **The new release retries the migration from scratch on every boot** and fails the same way
  until the cause is gone.
- **Every later push to `main` fails to deploy the same way**: its image carries the same failed
  migration first in line. Those changes are merged, not live, until recovery.

`a_failed_migration_leaves_nothing_and_the_previous_release_boots`
(`crates/flags-core/tests/db.rs`) pins all of this.

The new machine's log shows `applying migrations`, then the process exits with
`migrations failed`, whose cause chain includes
`while executing migration <N>: error returned from database: <message>`. The log carries the
database's message, not its SQLSTATE. The machine never binds a port, so `/readyz` fails and the
deploy fails. Exiting also ends the session holding sqlx's advisory lock, which sqlx does not
release itself after a failure.

### Diagnose (read-only)

```bash
fly logs -a otto-flags --no-tail    # the version <N> and the database's message
fly releases -a otto-flags          # which release failed
fly status -a otto-flags            # is any machine still on the failed release?
```

Do not assume the deploy rolled back. "The previous release keeps serving" holds only for
machines Fly did not move to the new image. A machine left on it fails every boot, and with
`min_machines_running = 0` the proxy may start it for a request; on a one-machine app that is an
outage. Moving it back is a hand deploy, **Rob's**, run from an up-to-date `main` checkout so
`fly.toml` matches:

```bash
fly releases -a otto-flags --image   # the image of the last release that booted
fly deploy -a otto-flags --image <that image ref>
```

Rob, in `fly postgres connect`:

```sql
SELECT version, description, success, encode(checksum, 'hex') AS checksum, installed_on
FROM _sqlx_migrations ORDER BY version;

-- For a lock wait: sessions on this database, who blocks whom, and who holds an advisory lock
-- (sqlx's migration lock). Share pids, states, and ages, never query text: it can hold data.
SELECT a.pid, a.usename, a.application_name, a.state, now() - a.xact_start AS xact_age,
       a.wait_event_type, pg_blocking_pids(a.pid) AS blocked_by,
       EXISTS (SELECT 1 FROM pg_locks l
               WHERE l.pid = a.pid AND l.locktype = 'advisory' AND l.granted) AS holds_advisory
FROM pg_stat_activity a
WHERE a.datname = 'otto_flags' AND a.pid <> pg_backend_pid()
ORDER BY a.xact_start NULLS LAST;
```

After a real failure, `<N>` has no row and no row has `success = false`.

### Recover

Match the cause from the log's message. "Re-run the newest deploy" below means, for Rob (or an
agent he asks), re-running the failed jobs of the **newest** CI run on `main`: each run deploys
its own commit, so an older run would deploy older code. Wait for that run to finish first
(`gh run rerun --failed` refuses a run in progress; it re-runs every failed job in the run, not
only `deploy`):

```bash
gh run list -R savvagent/otto-flags --workflow CI --branch main --event push --limit 1 \
  --json databaseId,headSha,status
gh run rerun <databaseId> -R savvagent/otto-flags --failed
```

**A transient cause** (`canceling statement due to lock timeout`, a dropped connection, a machine
stopped mid-boot): there is nothing to clear. Migrations that take strong locks set
`SET LOCAL lock_timeout` (as `0002`, `0003`, and `0005` do) so a lock wait fails the boot
quickly instead of queueing every query behind it. If one session keeps blocking, ending it is
Rob's call, scoped so a mistyped pid cannot reach another database (no row returned means it was
not on `otto_flags`):

```sql
SELECT pg_terminate_backend(pid) FROM pg_stat_activity
WHERE pid = <pid> AND datname = 'otto_flags' AND pid <> pg_backend_pid();
```

Then re-run the newest deploy.

**The SQL fails on production data** (a constraint the rows violate, a duplicate; messages such as
`violates check constraint`, `violates not-null constraint`, `could not create unique index`):
two ways out.

1. *Preferred: fix the data so `<N>` applies as written.* The fix PR's author writes the
   statement, scoped by primary key or `org_id`, with the SELECT that shows the offending rows
   and the expected row count. Rob runs it in a transaction (`BEGIN; …;`), compares the
   `UPDATE n` / `DELETE n` tag with the expected count, and only then `COMMIT;` (else
   `ROLLBACK;`). As the superuser attach role the statement bypasses RLS and the flag-change path:
   a fix that changes flag state must also write its `flag_versions` row and
   `pg_notify('flag_changes', …)`, or be made through the MCP tools instead. Then re-run the
   newest deploy. Every database then has the same history.
2. *When `<N>` must never run as written:* record `<N>` as applied without running it, and ship
   the corrected change as the next migration, `<N+1>`. `<N+1>` must converge both states:
   databases where `<N>` did run (CI, local, any fresh database) and production, where it did
   not. Use `IF EXISTS` / `IF NOT EXISTS` guards and data statements that are correct either way,
   and say in the PR that `<N>` is recorded, not run, in production. Order matters:
   1. Merge the PR with `<N+1>`. Its deploy fails on `<N>` like the others.
   2. `fly status` shows no machine on any release that contains `<N>` but not `<N+1>` (move one
      back as above first). Once `<N>` is recorded, such a machine would boot cleanly, skip `<N>`,
      and serve code that expects `<N>`'s schema.
   3. Rob records `<N>` as applied, with the checksum of the file at the merge commit of the
      `<N+1>` PR (what the image embeds):

      ```bash
      git fetch origin
      git show <merge sha>:crates/flags-core/migrations/<NNNN>_<name>.sql | sha384sum | cut -d' ' -f1
      ```

      ```sql
      BEGIN;
      INSERT INTO _sqlx_migrations (version, description, success, checksum, execution_time)
      VALUES (<N>, '<name with _ as spaces>', true, decode('<hex>', 'hex'), -1)
      RETURNING version, encode(checksum, 'hex');
      -- The returned hex must equal the sha384sum output; else ROLLBACK;
      COMMIT;
      ```

      sqlx compares only the version and the checksum (SHA-384 of the file's bytes, which
      `migration_checksums_are_the_sha384_of_their_files` pins); the description is cosmetic. A
      wrong checksum makes every release that knows `<N>` refuse with `VersionMismatch`; the undo
      is `DELETE FROM _sqlx_migrations WHERE version = <N> AND success = true;`.
   4. Immediately re-run the newest deploy. It skips `<N>` and applies `<N+1>`.

   Recording `<N>` before `<N+1>` is merged would let the failed release boot code that expects
   `<N>`'s schema against a database without it.

**A `success = false` row (`Dirty`)**: sqlx does not write one, but a hand-written row or another
tool can. Every release, old and new, then refuses to boot with
`migration <N> is partially applied`. Machines already running keep serving until they restart
(resuming from suspend does not re-run migrations). Confirm nothing of `<N>` is half applied
(compare the schema with the file), then Rob clears it, and the next boot runs `<N>` again:

```sql
DELETE FROM _sqlx_migrations WHERE version = <N> AND success = false RETURNING version;
```

`a_failed_unknown_migration_still_refuses_to_boot` pins both the refusal and the statement.

### Never

- **Edit, rename, delete, or renumber the failed migration, or reuse its number.** sqlx
  checksums applied migrations, CI and local databases may already have applied it, CI's
  `migrations-append-only` job refuses the change, and `versions_are_contiguous_from_one`
  refuses a gap. The fix is always data, a recorded row, or a new migration.
- **Run a migration's SQL by hand** to get past it. Objects it creates belong to whoever runs it
  (`0004`'s `purge_deleted_org_audit` must be owned by an RLS-exempt role, and the boot check
  refuses otherwise), and its row still has to be recorded with the right checksum.
- **Change the migrating role** (the superuser attach role) as part of a recovery: see
  Infrastructure.
- **Boot the new release with `FLAGS_RUN_MIGRATIONS=false`** against a schema without `<N>`: its
  code expects `<N>`'s schema.
- **Add a `-- no-transaction` migration** without revisiting this section: sqlx runs one
  outside any transaction it opens and records its row separately, so "leaves nothing" no longer
  holds (`no_migration_runs_outside_a_transaction` refuses one until then).

### Afterwards

Verify as for any deploy (`CLAUDE.md`, Ship > verify): `fly status` shows every `app` machine on
the new release, `/readyz` returns `{"status":"ready"}`, and the boot log shows
`applying migrations`, `tenant isolation enforced as role "otto_app"`, and
`the otto platform accepted this service's credential`.

## Auto-rollback

A policy per (flag, environment) disables that environment as an ordinary flag version (change
`auto_rollback`, no user actor) when the error rate with the flag on exceeds the rate with it off.
It shows in `flag_history` and is undone with `rollback_flag` or `set_flag_environment`; turning
the environment back on re-arms the policy (setting the policy again does not). Defaults: at
least 100 evaluations and 10 errors with the flag on, at least 100 evaluations with it off, and
an on-rate more than 2x the off-rate (or off traffic with no errors at all). The window is
hour-aligned: the current hour so far plus the `windowHours - 1` hours before it (`windowHours`
1-24, default 1, which is this hour so far). An optional `maxErrorRate` trips on the on-rate
alone, and is the only trigger with less off traffic than that (a rollout at or near 100%). Only
server-key reports count unless a policy sets `includeClientReports` (owner or admin only). Mode
`observe` records what would have happened without changing the flag.

- **Plan-gated.** Every paid plan (team, business, enterprise) includes `auto_rollback` through
  the platform's `plans.features` (otto-platform 0.5.0, savvagent/otto-platform#30); free does
  not. A successful answer is cached for 60 s by the platform client; a failed lookup is
  remembered for 10 s, so an outage costs one 2 s timeout per org per 10 s. It fails closed: a
  timeout or platform error means no rollback, recorded as `entitlement_unknown`. Creating or
  changing a policy is billed; removing, listing, `propose_rollback`, and `rollback_events` are
  free.
- **When it checks.** When SDK telemetry arrives, plus a 5-minute pass while the machine is
  awake. That works with `auto_stop_machines = "suspend"` and changes no cost. Detection latency
  is the SDK's telemetry flush interval, plus up to the 30 s per-flag debounce, plus any checks
  queued ahead of it (flags are checked one at a time; each check gives up after 15 s, and waits
  at most 2 s for a flag's row lock, and is then retried).
- **Outcomes** in `rollback_events`: `rolled_back`, `would_roll_back` (observe), `not_entitled`,
  `entitlement_unknown`. Suppressed outcomes are recorded at most once per hour per policy.
  Events are kept 90 days. A policy the checker could not act on (its patch failed) records no
  event: it stays armed, shows `lastErrorCode` and `lastErrorAt` in `list_rollback_policies`
  (`last_error_code` in the table), and is retried; its next successful check clears them.
- **No new secret, env var, scope, or recurring cost.**
- **Rolling the image back.** #31 (merged before this change) makes the migrator ignore applied
  migrations an image does not know, so an image built from #31 onward boots against the newer
  schema: rolling back to the release just before auto-rollback works, and that release simply
  ignores the auto-rollback tables (no policy is checked while it serves; policies and events
  are kept). An image from before #31 still refuses to boot once `0002` and `0003` are applied,
  so going back that far means rolling forward instead. `0002` and `0003` are additive, so the
  older release's writes still satisfy them.
- Migration `0003` (#12) gives the retention sweeps an unpinned read so they work under a role
  subject to RLS. Every unpinned policy added by `0002` and `0003` also requires the
  transaction-local marker `app.maintenance = 'on'` (#27), which only the background sweeps, the
  checker's backstop, and the usage shipper set. Migration `0005` (#68) does the same for
  `usage_outbox` and `usage_outbox_rejected`, for every command. Those background transactions
  (`maintenance::begin`) drop into `otto_app` with `SET LOCAL ROLE`, as tenant transactions do
  (#73), so in production too the marker policies are what let them see across orgs; if the pool's
  role were exempt from RLS and could not assume `otto_app` (or `otto_app` itself were exempt),
  they fail rather than run unconstrained, logged with `reason="configuration"`, so that reason
  from the shipper, sweeps, or checker backstop after a role change points here. Queries
  that cannot pin an org (SDK key resolution, the revocation check, the lifecycle tombstones) run
  in `unpinned::begin`, which drops into `otto_app` the same way but sets no marker, so they see
  no tenant row (#80); request paths pin through `begin_live`. Still run as the attach role:
  migrations, the startup isolation check and the role probes (catalog reads), `/readyz`'s
  `SELECT 1`, and the notify listener's `LISTEN` connection. A release from before #73 runs
  its background work as the attach role, so it keeps working during the rollout overlap and
  after a rollback. Under a pool role subject to RLS, a release from before `0005` ships no usage
  while it runs against that schema, and the rows wait in the outbox until a newer release ships
  them. Ad-hoc SELECT diagnostics as the superuser attach role bypass RLS and need neither the
  marker nor the role; as a role subject to RLS they need `SET LOCAL app.maintenance = 'on'` in
  their transaction to see across orgs.

Diagnostics (SELECT-only):

```sql
SELECT outcome, count(*) FROM rollback_events GROUP BY 1;
SELECT count(*) FILTER (WHERE tripped_at IS NULL) AS armed, count(*) AS total FROM rollback_policies;
```

## Checking it

```bash
curl -s https://otto-flags.savvagent.com/readyz
curl -s https://otto-flags.savvagent.com/.well-known/oauth-protected-resource
fly logs -a otto-flags   # look for "the otto platform accepted this service's credential"
```

Then point an MCP client at `https://otto-flags.savvagent.com/mcp`: it is sent to the
platform to sign in and `whoami` answers with the platform's records.

Usage backlog (should drain to 0 within seconds of activity):

```sql
SELECT count(*), max(attempts), max(last_error) FROM usage_outbox;
```

`last_error` names the last failure and its HTTP status (for example `http_status 502`,
`transport`, `unauthorized`, `not_found`; a miscounted receipt stores `decode`, and the counts
are in the `error` log); the failure's text is logged only at `debug`.
