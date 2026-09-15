# otto-flags design: MCP-only feature flags on the otto platform

**Status:** Brainstorm, pre-implementation. No server/SDK code exists yet in this repo.
**Supersedes:** savvagent-flags (`~/dev/savvagent-flags`). No production users exist there,
so this is a successor design, not a migration — nothing needs to move at the data layer.
**License:** otto-flags is open source (server + SDKs). The shared otto-platform
(identity/auth/billing/console) is not affected by this and stays closed — see §7.

## 1. Vision

otto-flags is the second member of the otto-* family of agent-facing MCP servers, after
otto-factory. Where savvagent-flags was a human-operated SaaS dashboard with an MCP
integration bolted on the side, otto-flags inverts that: **flag management, targeting,
rollout, and incident response are MCP tools an agent calls**, not a UI a human clicks
through. The one deliberate exception is the flag-evaluation hot path inside a running
customer app (`isEnabled()` on every request) — that stays SDK/REST, because a prod request
can't do an LLM tool-call round trip.

otto-flags does not stand alone. It is built on a shared **otto-platform** — the identity,
auth, tenant-isolation, billing, and console substrate extracted from otto-factory — so
that signing up once gives an org access to every otto-* server. otto-flags itself, being
open source, is a **monorepo**: the MCP server and the client/framework/mobile/server SDKs
(currently `savvagent-sdks`) live together in one public repo (§7).

## 2. otto-platform: what gets extracted from otto-factory

A new repo, `otto-platform` (naming TBD), becomes a Rust workspace holding:

| Extracted from otto-factory | Becomes |
|---|---|
| `of-core` migrations 0001 (identity), 0005 (auth) | shared `users`/`orgs`/`org_members`/`teams`/`org_invites` + TOTP/recovery/magic-links/IdP-SSO/OAuth-client/token tables |
| `of-core` migration 0006 (billing) | shared `plans`/`subscriptions`/`usage_events`/`org_period_usage` |
| `of-core` migration 0007 (RLS) — the `df_app` role, `current_org()`, `FORCE ROW LEVEL SECURITY`, the `<table>_tenant_isolation` policy convention, `Db::verify_tenant_isolation` | a shared `otto-tenant` crate other services' domain crates depend on |
| `of-auth` | shared auth service — OAuth 2.1 AS, TOTP, PATs, resource-indicator (RFC 8707) audience binding |
| `of-web` + `web/` (SvelteKit console) | the single **Otto Console** — signup, org management, billing — with a plugin surface for each otto-* server to register its own config/monitoring panels |

otto-factory keeps its own domain crate (`of-core`'s jobs/repos/leases/trackers tables,
`of-mcp`) but sheds identity/auth/billing/RLS-infra to the shared platform. It becomes the
first real consumer of the extraction — a good forcing function to prove nothing broke.

## 3. Tenant isolation across the family

The requirement: **a tenant of one otto-* service benefits from the others** — one signup,
one org, usable everywhere in the family — without forcing unrelated domain data (jobs vs.
flags) into one database or one blast radius.

**Mechanism:**

- One physical **identity database** (`otto-platform`'s Postgres) holds `orgs`, `users`,
  `org_members`, auth, and billing. This is the single source of truth for "who is this org"
  and "what can they do."
- Each otto-* service keeps its **own domain database** (otto-factory's jobs/repos/leases;
  otto-flags' flags/environments/targeting). They share no tables, only the same `org_id`
  UUID namespace — no cross-database foreign keys, application-level referential integrity
  only.
- Each service's domain database runs the **same RLS pattern**, sourced from the shared
  `otto-tenant` crate: its own `df_app` role, its own `current_org()`, its own
  `tenant_tables` registry, its own `Db::verify_tenant_isolation` boot check. A bug in one
  service's RLS cannot leak another service's data, because the guard and the data are both
  local to that service.
- Tokens are minted by the shared auth service and **audience-bound per service** (the
  `resource` column already on `authorization_codes`/`access_tokens` in `of-core` 0005).
  otto-flags' MCP resource-server middleware validates a token's audience and `org_id`
  claim, then opens its own transaction with `SET LOCAL app.org_id` from that claim — same
  shape of-mcp uses today, pointed at a different audience. A token minted for otto-factory
  cannot be replayed against otto-flags.

## 4. Billing: one plan, family-wide

Decision: **a single, consistent subscription plan across the whole otto-* family. Higher
tiers unlock more functionality, not a separate plan per product.** An org buys one plan
(free/team/business/enterprise, as `of-core` already defines); that plan determines both
usage volume (the existing `included_ops` bucket) and which *capabilities* are available in
every otto-* service the org uses — auto-rollback and predictive risk scoring in otto-flags
gated the same way trackers/GitHub-App sync is gated in otto-factory.

Mechanism: `plans` gains a `features` JSONB column (or a `plan_features` table if the list
grows large enough to want to query it relationally) — e.g.
`{"auto_rollback": true, "predictive_risk_scoring": true, "max_concurrent_agents": 20}`.
Each service checks its own relevant keys against the org's plan; the plan and the
enforcement stay decoupled so a new otto-* service adds entries to this JSONB without a
schema change. Usage metering stays one shared bucket per org — a call to otto-flags and a
call to otto-factory both draw from the same `org_period_usage` counter — consistent with
"one plan" rather than per-product metering.

## 5. otto-flags domain model

Everything below is scoped by the shared `org_id`; none of it duplicates identity, auth, or
billing, all of which now live in otto-platform.

### Carries forward from savvagent-flags, largely as-is

- `applications` (rename candidate: `flag_apps`) — holds SDK/server keys and the
  environments array; the seam between MCP-driven management and SDK-driven evaluation.
- `feature_flags` — key, name, environments JSONB, variations JSONB, status, scope
  (app-level vs. org-wide), version, scheduling fields.
- `flag_evaluations` + analytics aggregation/rollup tables — the evidence base that
  correlation, health monitoring, and risk scoring are computed from.
- `user_segments` — reusable targeting groups.
- `rollback_policies` + `rollback_events` — the auto-rollback differentiator; becomes MCP
  tools (`propose_rollback`, `execute_rollback`).
- `rollout_velocity_policies` + `rollout_progressions` — smart rollout; MCP tools to check
  or advance a rollout (`recommend_rollout_step`, `advance_rollout`).
- `flag_health_baselines` + `flag_drift_alerts`.
- `flag_risk_assessments` + `flag_incident_history` — agent-native by nature: "what's the
  risk of shipping this at 100%?" is a tool call, not a dashboard chart.
- `flag_errors` (with Sentry correlation columns) + `ai_insights`.
- the observability-integration config table (`mcp_integrations` → rename
  `observability_connections`) — otto-flags acting as an MCP *client* to
  `mcp-sentry`/`mcp-datadog`/`mcp-dynatrace`/etc.

### Dropped — now the shared platform's job

`organizations`, `users`, `organization_memberships`, `organization_invitations`,
`organization_join_codes`, `enterprise_sso_configs`, `oauth_states`, `user_sso_identities`,
`payment_billing_system`/`subscriptions`, `system_admin`, `user_account_deletion`,
`launch_waitlist`, `support_system`, `data_retention_audit_action`, and the entire
`rbac_implementation` migration (use the shared `org_role` enum — owner/admin/member —
unless a flags-specific scope proves necessary later; none is assumed here).

### Simplified rather than ported as-is

- **No `archived_flags` shadow table.** savvagent-flags mirrors the whole `feature_flags`
  schema into a second table plus a trigger enforcing mutual exclusion, to keep the "active"
  table small. Replace with a `status`/`archived_at` column on `feature_flags` itself —
  real complexity avoided for a problem that isn't yet a scale problem.
- **No bespoke audit system.** Reuse otto-factory's `0008_audit.sql` shared audit-events
  pattern instead of a second, flags-specific one.
- **`cohort_analysis_system` deferred.** Hold off porting this until a concrete agent
  workflow needs it, rather than carrying it forward speculatively.

## 6. MCP tool surface (sketch, not final)

Management: `create_flag`, `update_flag`, `archive_flag`, `set_targeting_rule`,
`create_segment`.
Rollout: `recommend_rollout_step`, `advance_rollout`, `propose_rollback`,
`execute_rollback`.
Insight: `assess_risk` (pre-deploy), `correlate_errors`, `get_incident_history`,
`get_flag_health`.
Resources: subscribable flag-state and incident-state resources for an agent to watch
without polling.

## 7. Open source & monorepo layout

otto-flags is going open source, and `savvagent-sdks` moves into this repo rather than
staying a separate one — a single public monorepo covering the server and every SDK,
mirroring how otto-factory keeps `client-skills/` alongside its server rather than in a
separate repo.

**Proposed layout:**

```
otto-flags/
├── crates/              # Rust workspace: MCP server + domain logic
│   ├── flags-core/      #   domain model, RLS-scoped queries (of-core equivalent, §5)
│   ├── flags-mcp/       #   MCP tool surface (of-mcp equivalent, §6)
│   └── flags-server/    #   binary: config, migrations, router assembly (of-server equivalent)
├── web/                 # flags-specific console panels (registers into the shared Otto Console)
├── packages/            # moved from savvagent-sdks: client/framework/mobile/server SDKs,
│                         #   plus the mcp-sentry/mcp-datadog/etc. observability integrations
├── examples/             # moved from savvagent-sdks
├── docs/
├── Cargo.toml            # Rust workspace root
├── pnpm-workspace.yaml   # JS/TS workspace root (unchanged from savvagent-sdks)
├── LICENSE
└── README.md
```

**The open-source boundary.** otto-flags going public does not make otto-platform public.
otto-flags' server depends on otto-platform at the *network* boundary (an OAuth resource
server validating tokens minted elsewhere), the same way of-mcp does today — not as a
source dependency — so nothing proprietary needs to be vendored in or open-sourced just to
ship otto-flags. The one thing that needs an explicit call is the shared `otto-tenant` RLS
crate (§3): either it's also open-sourced (it's generic infrastructure with no business
logic in it, so a reasonable candidate) or otto-flags reimplements the same pattern locally
without a shared-crate dependency. Not deciding this here — see open questions.

**CI/versioning stay per-language, not unified.** Keep `savvagent-sdks`' existing
Changesets-based release flow for the JS/TS packages; keep a release-please-style flow (as
otto-factory already uses) for the Rust crates/binary; path-filter CI so a docs-only or
single-SDK change doesn't trigger every language's test suite.

## 8. Explicitly out of scope for this doc

- **No migration plan.** savvagent-flags has no production users; otto-flags is a clean
  build, not a data migration.
- **otto-platform's own internal workspace layout** (crate boundaries, deploy topology) —
  worth its own doc once extraction starts.

## 9. Open questions

- Exact naming for `otto-platform` and whether it deploys as one binary (like
  `of-server`) or as separate auth/console/billing services.
- The Otto Console's plugin interface — how a per-service panel registers its routes/UI
  into the shared console shell.
- Whether the shared usage bucket needs per-service sub-accounting for internal cost
  attribution even though the org only sees one number.
- License choice for the monorepo (savvagent-sdks already ships a LICENSE — carry it
  over as-is, or pick deliberately now that the server is joining it under the same terms).
- Whether `otto-tenant` (the RLS crate) is itself open-sourced, or reimplemented locally
  in otto-flags without a dependency on otto-platform's source.
- Whether moving `savvagent-sdks` into this repo preserves its git history (e.g. via
  `git subtree`) or starts fresh with a clean copy.

## 10. Suggested next steps

1. Scaffold the `otto-platform` workspace by extracting `of-core` migrations
   0001/0005/0006/0007 and `of-auth` out of otto-factory.
2. Point otto-factory at the extracted platform; confirm nothing regresses (isolation
   tests, auth flows).
3. Scaffold otto-flags' own domain crate and database against the shared `otto-tenant`
   RLS crate, starting with `flag_apps` + `feature_flags` + evaluation ingestion.
4. Add the `plans.features` JSONB column and wire one capability (e.g. `auto_rollback`)
   through it end-to-end as the pattern for everything after.
5. Merge `savvagent-sdks` into this repo under `packages/`/`examples/` (§7), choosing
   history-preserving vs. fresh-copy per the open question above.
