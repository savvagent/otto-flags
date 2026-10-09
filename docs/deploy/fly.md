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
