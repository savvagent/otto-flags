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
  purge_deleted_org_audit(uuid) is owned by "…" (superuser) and executable by "otto_app"`, and
  any other state ends the boot with `refusing to serve: an org.deleted could not purge the
  org's audit trail: …`, naming the fix (the deploy then rolls back). The same check refuses a
  missing function, or a tenant role without `EXECUTE` on it. Before moving off the superuser
  attach role (for example onto a dedicated instance), either give the new migrating role
  `BYPASSRLS` (only when tenant transactions still drop into `otto_app`; where `otto_app`
  cannot be assumed, the migrating role is also the tenant role and the isolation proof refuses
  it), or reassign the function to a role that has it (`ALTER FUNCTION
  purge_deleted_org_audit(uuid) OWNER TO …`, human-only), or run migrations separately as such
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
- Migration `0003` (#12) gives the retention sweeps an unpinned read so they work under a
  non-exempt pool role. Every unpinned policy added by `0002` and `0003` also requires the
  transaction-local marker `app.maintenance = 'on'` (#27), which only the background sweeps and
  the checker's backstop set, so a request path that forgot to pin an org reads nothing. Ad-hoc
  diagnostics as a non-exempt role need `SET LOCAL app.maintenance = 'on'` in their
  transaction to see across orgs.

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
