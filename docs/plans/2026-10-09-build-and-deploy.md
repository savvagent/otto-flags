# otto-flags v1: build and deploy

**Date:** 2026-10-09. **Goal:** take otto-flags from a schema scaffold to a deployed resource
server of the otto platform: an MCP surface agents use to manage flags, and the SDK REST
surface running apps use to evaluate them.

## Where it starts

- `crates/flags-core`: four migrations (`flag_apps`, `feature_flags`, raw `flag_evaluations`,
  RLS) and extension traits on `otto_tenant::Tx`. Never applied to any deployed database.
- `crates/flags-server`: boots, migrates, proves tenant isolation, then idles.
- No GitHub repo, no CI, no deploy. Dependency pinned to otto-platform `80b17eb`
  (pre-cutover).

## Decisions

1. **Resource server, exactly like otto-factory.** Tokens are introspected at the platform
   through `otto-resource`; lifecycle webhooks (`org.deleted`, `member.removed`,
   `team.deleted`) are verified and applied idempotently; MCP usage goes to a
   `usage_outbox` shipped to the platform. Pin `otto-tenant` and `otto-resource` to one
   otto-platform rev.
2. **Squash the migrations into one baseline.** Nothing has been applied anywhere, so this is
   the last free chance; append-only from then on (CI-enforced, as in otto-platform).
3. **Keep the existing SDK REST contract** (`docs/SDK-DEVELOPER-GUIDE.md`) so the SDKs in
   `packages/` work unchanged: `POST /api/flags/{key}/evaluate` (plus the
   `POST /api/evaluate/{key}` spelling the Rust SDK uses), `GET /api/sdk/flags`,
   `GET /api/sdk/enterprise-flags` (always empty in v1: no org-wide flags yet),
   `GET /api/flags/stream` (SSE), `POST /api/telemetry/evaluations`,
   `POST /api/telemetry/errors`. Keys are `sdk_…` (client, public) and `srv_…` (server,
   secret), sent as `Authorization: Bearer` or `X-SDK-Key`.
4. **SDK keys are stored hashed** (SHA-256) in `app_keys`, a lookup table outside RLS (the
   org is not known until the key resolves, the same reason `deleted_orgs` is outside it).
   The server key is shown once, at creation or rotation. The client key is public and is
   also kept in clear on `flag_apps` so it can be shown again.
5. **Evaluations are counted, not stored.** Raw per-evaluation rows would fill the shared
   512 MB `otto-db` and keep end-user context nobody needs. Telemetry is folded into hourly
   rollups (`flag_eval_hourly`). Counting happens on SDK telemetry only, never on the
   evaluate endpoint, because the SDKs report every evaluation (cache hits included) and
   counting both would double-count. Error reports are kept raw (truncated) for 14 days for
   correlation, then swept.
6. **Every flag change is versioned** in `flag_versions` (full snapshot + actor + reason), so
   `rollback_flag` can restore any earlier version, and an agent can read history.
7. **Evaluation semantics (v1)**, per environment in `feature_flags.environments`:
   `{"enabled": bool, "rollout_percentage": 0..100, "rules": [...], "default_variation": str}`.
   Archived or disabled → off. Rules evaluated in order, first match wins
   (`{attribute, operator, values, enabled, variation}`; operators `in`, `not_in`,
   `equals`, `not_equals`, `contains`, `starts_with`, `ends_with`, `gt`, `gte`, `lt`,
   `lte`, `exists`). Otherwise the percentage rollout, bucketed deterministically by
   SHA-256 of `flag key + identifier` (user_id → anonymous_id → session_id). Variations are
   weighted, bucketed independently. Segments are deferred.
8. **Scopes:** `flags:read`, `flags:write`, `apps:admin`. Defaults `flags:read,flags:write`.
   App creation and key rotation also require the caller to be an org owner/admin *now*
   (fresh member lookup at the platform, the lesson from otto-factory#200).
9. **Metering:** MCP writes are billable, reads and dry-run evaluation are free. The SDK
   evaluation path is not metered in v1 (quotas off by default, like otto-factory).
10. **Realtime:** flag writes `pg_notify('flag_changes', …)` in the same transaction; one
    `PgListener` per process fans out to SSE subscribers filtered by org and app.
11. **Deploy:** Fly app `otto-flags` (shared-cpu-1x, 256 MB, suspend when idle,
    `min_machines_running = 0`), database `otto_flags` on the shared `otto-db`, public host
    `otto-flags.savvagent.com`, resource URI `https://otto-flags.savvagent.com/mcp`.
    No console in v1 (VISION.md: agents manage flags; org admin is the platform's console).
12. **CI:** fmt, clippy `-D warnings`, `cargo test` against Postgres, append-only
    migrations, Docker build. JS packages keep their own Changesets flow, run only when
    `packages/` changes. Deploy runs on pushes to `main` after CI passes.

## MCP tools (v1)

| Tool | Scope | Billable |
|---|---|---|
| `whoami`, `usage` | any | no |
| `list_apps` | flags:read | no |
| `create_app`, `rotate_app_keys` | apps:admin + owner/admin role | yes |
| `list_flags`, `get_flag`, `flag_history` | flags:read | no |
| `create_flag`, `update_flag`, `archive_flag` | flags:write | yes |
| `set_flag_environment` (enable, rollout %, rules, default variation) | flags:write | yes |
| `rollback_flag` (to a version, or kill switch) | flags:write | yes |
| `evaluate_flag` (dry run with a context) | flags:read | no |
| `flag_health` (evaluation counts, error counts and rate, by window) | flags:read | no |

## Crates

- `flags-core`: schema, domain queries, evaluation engine (pure), platform events, usage
  outbox, realtime notify/listen, key hashing.
- `flags-mcp`: resource-server auth, MCP service and tools.
- `flags-api`: SDK REST surface and `/platform/webhooks`.
- `flags-server`: config, health, router assembly, background tasks, binary.

## Steps

1. Commit scaffold; add `.gitignore` entries; create `savvagent/otto-flags`; push.
2. Bump otto-platform pin; add `otto-resource`; squash migrations into a baseline.
3. flags-core domain + evaluation engine + tests.
4. flags-mcp.
5. flags-api.
6. flags-server assembly, config, Dockerfile, fly.toml, compose.yaml, `.env.example`.
7. CI workflow; deploy docs (`docs/deploy/fly.md`); README/design-doc refresh.
8. Deploy: create app, create + attach `otto_flags`, register the resource at the platform
   (`resource register`, `rotate-secret`, `set-webhook`), set secrets, deploy, cert for
   `otto-flags.savvagent.com` (DNS at Namecheap is a manual step), verify end to end.

## Deferred

Segments, org-wide ("enterprise") flags, `plans.features` gating, auto-rollback policies,
risk scoring and observability-integration correlation (`packages/mcp-*`), a console panel,
release-please.
