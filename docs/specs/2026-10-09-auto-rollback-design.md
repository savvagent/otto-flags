# Auto-rollback, gated by the plan — Design

**Status:** Approved (spec critique, 2 rounds), 2026-10-09. Not yet planned.
**Issue:** [#11](https://github.com/savvagent/otto-flags/issues/11). Depends on
[savvagent/otto-platform#30](https://github.com/savvagent/otto-platform/issues/30).
**Builds on:** [the product design](2026-09-15-otto-flags-design.md) §4, §5, §10 step 4, and
[the v1 plan](../plans/2026-10-09-build-and-deploy.md).

## 1. Brief

Quoted from #11:

> Design doc §4 and §10 step 4: wire the first plan-gated capability end to end, as the pattern
> every later paid capability follows. The capability is auto-rollback, the differentiator named
> in §5 (`rollback_policies` + `rollback_events`, tools `propose_rollback` / `execute_rollback`).
>
> Depends on savvagent/otto-platform#30 (`plans.features`, exposed through `usage-status`).
>
> An agent attaches a rollback policy to a flag in one environment. When the flag's own telemetry
> shows it is hurting (error rate with the flag on, against the rate with it off, over enough
> evaluations to mean something), otto-flags turns the flag off in that environment by itself,
> records why, and tells subscribed SDKs immediately. Only orgs whose plan includes
> `auto_rollback` can create or run policies.
>
> v1 already has the pieces this builds on: `flag_health` computes on/off error rates and suggests
> "Consider rolling back"; `rollback_flag` and `set_flag_environment` act; `flag_versions` records
> every change; `pg_notify` pushes changes to SDK streams.

**Acceptance criteria (verbatim from #11):**

- [ ] A policy can be created, changed, listed, and removed over MCP, scoped to one flag and
  environment, requiring `flags:write` (list: `flags:read`); creating or changing one is refused
  with an upgrade pointer when the plan lacks `auto_rollback`.
- [ ] A policy that trips turns the flag off in that environment only, as a new flag version whose
  history names the policy and the observed numbers, and SDK streams receive the change.
- [ ] Every trip, and every trip that was suppressed (not entitled, observe-only mode), is recorded
  and readable over MCP.
- [ ] An agent can ask what a policy would do right now without changing anything (dry run).
- [ ] The decision rule agrees with `flag_health`'s assessment; forged telemetry from a public
  client key cannot trip a policy unless the policy opts in.
- [ ] New tables are tenant-isolated (RLS, composite FKs) and purged on `org.deleted`.
- [ ] Works on a machine that suspends when idle (no reliance on a timer alone).

## Goal & success criteria

Make "turn it off when it breaks" something an agent sets up once per flag and environment rather
than something it has to be watching for, and do it through the plan-feature mechanism every later
paid capability (risk scoring, rollout velocity) will reuse.

- A policy on a flag whose on-error-rate crosses its threshold disables the flag in that
  environment within one checker pass of the telemetry batch that crossed it (target: under 60 s
  of the batch arriving, on an awake machine).
- The disable is an ordinary flag version (`change = 'auto_rollback'`): `flag_history` shows it,
  `rollback_flag` can undo it, SSE subscribers see it.
- An org without `auto_rollback` cannot create a policy, and an existing policy of an org that
  lost it records `not_entitled` instead of acting.
- Anyone holding only the app's public client key cannot cause a trip on a default policy.
- No new recurring cost: no new machine, no `min_machines_running` change, bounded new tables.

## Premise corrections

1. **`execute_rollback` is not a new tool.** Manual rollback already exists twice:
   `rollback_flag` (restore a whole version, `crates/flags-core/src/flags.rs:434`) and
   `set_flag_environment` (`enabled: false` is the kill switch, `flags.rs:355`). A third way to do
   the same thing would be a second definition of "roll back". The automatic path is the new part;
   `propose_rollback` becomes the dry run.
2. **"Rollback" here means disabling one environment, not restoring a version.** `rollback_flag`
   restores the full snapshot, every environment at once (`flags.rs:466-477`). An automated action
   triggered by production telemetry must not also rewrite staging, so the automatic action is
   the per-environment kill switch. Reverting one environment to an earlier state is out of scope
   (§Scope).
3. **The checker cannot be a timer.** The Fly machine suspends when idle (`fly.toml`:
   `auto_stop_machines = "suspend"`, `min_machines_running = 0`); a suspended process runs no
   timers. It is also the only time a check could matter: telemetry is what wakes the machine,
   and with no telemetry there is nothing new to judge. So checks are driven by telemetry arrival
   (§4).
4. **Client-key error reports are untrusted input.** `POST /api/telemetry/errors` accepts the
   public `sdk_…` key (`crates/flags-api/src/telemetry.rs:119-139`), and `flag_errors` does not
   record which kind of key reported a row; neither does `flag_eval_hourly`, and the evaluations
   endpoint accepts the public key too. `flag_health` already labels reported text as untrusted
   for reading (`telemetry.rs:211-215`); for *acting*, the source of both errors and evaluations
   must be recorded and filtered (§3, §6), or forged client-key evaluations could push a sample
   past its minimums or open the "off has no errors" branch.
5. **Version changes need an actor that is not a user.** `ChangeMeta.actor` is a `UserId`
   (`flags.rs:129-133`) and `record_version` writes it (`flags.rs:565-571`), but
   `flag_versions.actor_user_id` is nullable (`0001_baseline.sql:146`). A system change records
   `NULL` there and names the policy in `reason` and in `rollback_events`. `ChangeMeta.actor`
   becomes `Option<UserId>` and `record_version` takes it as such; the MCP tools in
   `crates/flags-mcp/src/tools/flags.rs` pass `Some(caller.user_id)`, and only the checker passes
   `None`.
6. **Errors often carry no environment.** Evaluations without one default to `"production"`
   (`crates/flags-api/src/telemetry.rs:105-106`), but errors are stored with a NULL environment
   (`telemetry.rs:76-84, 122-131`), and the node-server SDK sends none
   (`packages/node-server/src/telemetry.ts:120-128`). `flag_health` counts NULL-environment errors
   in every environment (`flags-core/src/telemetry.rs:279, 292`). The checker instead treats a
   NULL environment as `"production"`, mirroring the evaluation default: counting NULL everywhere
   would let staging errors trip a production policy, and ignoring NULL would mean a default
   server-SDK setup never trips.

## Scope

**In:**
- otto-platform contract (built under otto-platform#30, consumed here): `plans.features` and
  `UsageStatus::features` / `feature_enabled`.
- Migration `0002_auto_rollback.sql`: `rollback_policies`, `rollback_events`,
  `flag_errors.reporter`, `flag_eval_hourly.server_count`.
- `flags-core`: a pure decision rule shared with `flag_health`'s assessment; policy and event
  queries; the entitlement check; the system-actor change path; the checker.
- `flags-mcp`: `set_rollback_policy`, `remove_rollback_policy`, `list_rollback_policies`,
  `propose_rollback`, `rollback_events`.
- `flags-api`: record the reporter key kind on error and evaluation telemetry; notify the
  checker after a telemetry commit.
- `flags-server`: spawn the checker; a periodic pass while awake as a backstop.
- Org-deleted purge of the new tables.

**Out:**
- Reverting one environment to an earlier version's state (only "disable" in v1 of this feature).
- Rollout velocity / `advance_rollout`, risk scoring, baselines and drift alerts (§5 of the
  design; they reuse the entitlement pattern built here).
- Correlation with observability tools (`packages/mcp-*`).
- Notifications outside otto-flags (email, Slack). Trips are visible via MCP and the SSE stream.
- Per-subscription feature overrides on the platform.
- Any change to the SDK REST contract or to evaluation bucketing.

## 2. The entitlement: `plans.features` (otto-platform#30)

Built in otto-platform; this section fixes the contract otto-flags relies on.

- `plans.features jsonb NOT NULL DEFAULT '{}'`. Keys are owned by the service that reads them.
  Seeded in the same platform migration:

  | Plan | `features` |
  |---|---|
  | free | `{}` |
  | team | `{"auto_rollback": true}` |
  | business | `{"auto_rollback": true}` |
  | enterprise | `{"auto_rollback": true}` |

- `GET /internal/orgs/{org}/usage-status` adds `"features": {...}`.
- `otto_resource::UsageStatus` gains `#[serde(default)] pub features: BTreeMap<String, Value>` and
  `pub fn feature_enabled(&self, key: &str) -> bool` (true only for JSON `true`).
- Released as otto-platform 0.5.0; otto-flags bumps both pinned crates (`otto-tenant`,
  `otto-resource`) to that rev in its own commit (Repository Conventions: one rev for both).

Reusing `usage-status` keeps one cached platform call (`USAGE_STATUS_TTL` = 60 s,
`otto-resource/src/client.rs:26`) for both quota and features. A plan change therefore takes
effect within 60 s; no `plan.changed` webhook handling is needed.

In otto-flags, `flags_core::entitlements` wraps it:

```rust
pub const AUTO_ROLLBACK: &str = "auto_rollback";

pub enum Entitlement { Granted, Denied { plan: String }, Unknown }

impl Entitlements {
    /// Bounded by a 2 s timeout, never called with a pooled connection held.
    pub async fn check(&self, org: OrgId, feature: &str) -> Entitlement;
}
```

It shares the `PlatformClient` already held by `usage::Meter`, but none of the meter's behaviour:
it ignores `FLAGS_ENFORCE_QUOTAS` (`Meter::enforce`) and has no fail-open window
(`Meter::status_for`). A timeout or platform error is `Unknown`, which every caller treats as not
entitled (fail closed). It is **never** called on the SDK request path (Invariant 6): only from
MCP tools and the background checker.

## 3. Data model + migration

New file `crates/flags-core/migrations/0002_auto_rollback.sql`. Nothing in `0001_baseline.sql`
changes (Rule 6).

```sql
-- Which kind of SDK key reported an error. Auto-rollback counts only server
-- reports unless a policy opts in: a client key is public, so anyone can
-- report anything with it. NULL for rows written before this column existed,
-- treated as client.
ALTER TABLE flag_errors ADD COLUMN reporter text
  CHECK (reporter IN ('client', 'server'));

-- How many of a bucket's evaluations a server key reported (count stays the
-- total). A column rather than a primary-key change, so the release still
-- running during a rolling deploy keeps working: its INSERT ... ON CONFLICT
-- leaves server_count at 0 (counted as client), and the new release adds
-- EXCLUDED.server_count in the same upsert.
ALTER TABLE flag_eval_hourly ADD COLUMN server_count bigint NOT NULL DEFAULT 0
  CHECK (server_count >= 0);

CREATE TYPE rollback_mode AS ENUM ('enforce', 'observe');

-- One policy per flag and environment.
CREATE TABLE rollback_policies (
  id                     uuid PRIMARY KEY DEFAULT gen_random_uuid(),
  org_id                 uuid          NOT NULL,
  app_id                 uuid          NOT NULL,
  flag_id                uuid          NOT NULL,
  environment            text          NOT NULL,
  mode                   rollback_mode NOT NULL DEFAULT 'enforce',
  -- Decision thresholds (see flags_core::rollback::decide).
  min_evaluations        bigint        NOT NULL DEFAULT 100  CHECK (min_evaluations >= 1),
  min_errors             bigint        NOT NULL DEFAULT 10   CHECK (min_errors >= 1),
  max_error_ratio        double precision NOT NULL DEFAULT 2.0 CHECK (max_error_ratio > 1.0),
  max_error_rate         double precision CHECK (max_error_rate > 0 AND max_error_rate <= 1),
  window_hours           integer       NOT NULL DEFAULT 1    CHECK (window_hours BETWEEN 1 AND 24),
  include_client_reports boolean       NOT NULL DEFAULT false,
  -- Set when the policy trips; cleared when the environment is turned back on.
  -- A tripped policy does not act again, so a rollback cannot flap.
  tripped_at             timestamptz,
  last_checked_at        timestamptz,
  created_by             uuid,
  created_at             timestamptz   NOT NULL DEFAULT now(),
  updated_at             timestamptz   NOT NULL DEFAULT now(),
  UNIQUE (flag_id, environment),
  UNIQUE (org_id, id),
  FOREIGN KEY (org_id, app_id)  REFERENCES flag_apps (org_id, id)     ON DELETE CASCADE,
  FOREIGN KEY (org_id, flag_id) REFERENCES feature_flags (org_id, id) ON DELETE CASCADE
);

CREATE INDEX rollback_policies_org_idx ON rollback_policies (org_id);
-- The checker's lookup: armed policies for a flag.
CREATE INDEX rollback_policies_flag_idx ON rollback_policies (flag_id) WHERE tripped_at IS NULL;

-- What policies did, or would have done. Kept 90 days (the evaluation
-- retention), swept unpinned like flag_errors.
CREATE TABLE rollback_events (
  id            bigserial   PRIMARY KEY,
  org_id        uuid        NOT NULL,
  policy_id     uuid        NOT NULL,
  flag_id       uuid        NOT NULL,
  environment   text        NOT NULL,
  -- rolled_back | would_roll_back (observe mode) | not_entitled | entitlement_unknown
  outcome       text        NOT NULL,
  -- The numbers the decision was made on: counts, rates, thresholds.
  observed      jsonb       NOT NULL,
  -- The flag version the rollback created (rolled_back only).
  flag_version  integer,
  created_at    timestamptz NOT NULL DEFAULT now(),
  FOREIGN KEY (org_id, policy_id) REFERENCES rollback_policies (org_id, id) ON DELETE CASCADE,
  FOREIGN KEY (org_id, flag_id)   REFERENCES feature_flags (org_id, id)     ON DELETE CASCADE
);

CREATE INDEX rollback_events_flag_idx ON rollback_events (flag_id, created_at DESC);
CREATE INDEX rollback_events_org_idx ON rollback_events (org_id);
CREATE INDEX rollback_events_created_idx ON rollback_events (created_at);

-- Tenant isolation, as in the baseline: FORCE RLS and a policy named
-- <table>_tenant_isolation so Db::verify_tenant_isolation finds them.
ALTER TABLE rollback_policies ENABLE ROW LEVEL SECURITY;
ALTER TABLE rollback_policies FORCE ROW LEVEL SECURITY;
CREATE POLICY rollback_policies_tenant_isolation ON rollback_policies
  USING (org_id = current_org()) WITH CHECK (org_id = current_org());
-- The checker's backstop finds due policies across every org, unpinned, then
-- pins per org (begin_live) to act. Policies are permissive and OR together, so
-- this adds exactly one thing: an unpinned SELECT. A pinned transaction still
-- sees only its own org's policies, and nothing unpinned can write them. Same
-- shape as the baseline's unpinned retention policies (0001_baseline.sql:312-320).
CREATE POLICY rollback_policies_checker ON rollback_policies
  FOR SELECT USING (current_org() IS NULL);

ALTER TABLE rollback_events ENABLE ROW LEVEL SECURITY;
ALTER TABLE rollback_events FORCE ROW LEVEL SECURITY;
CREATE POLICY rollback_events_tenant_isolation ON rollback_events
  USING (org_id = current_org()) WITH CHECK (org_id = current_org());
CREATE POLICY rollback_events_retention ON rollback_events
  FOR DELETE USING (current_org() IS NULL);

-- Grants: the baseline's ALTER DEFAULT PRIVILEGES already covers tables and
-- sequences the migrating role creates later. Grant explicitly anyway, guarded
-- as the baseline guards it, so the migration does not depend on the
-- migrating role being the one that ran the baseline.
DO $$
BEGIN
  IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'otto_app') THEN
    EXECUTE 'GRANT SELECT, INSERT, UPDATE, DELETE ON rollback_policies, rollback_events TO otto_app';
    EXECUTE 'GRANT USAGE, SELECT ON SEQUENCE rollback_events_id_seq TO otto_app';
  END IF;
END $$;
```

**Rolling-deploy compatibility.** Every change is additive. During the rollout the old release
inserts into `flag_errors` without `reporter` (gets NULL, treated as client), upserts
`flag_eval_hourly` without touching `server_count` (stays 0, treated as client), and never touches
the new tables. The new release boots, migrates under the advisory lock, and serves.

**Bounded growth (Invariant 15).** At most one policy per (flag, environment). Events: at most one
`rolled_back` per trip, and suppressed outcomes are deduplicated: a policy records
`not_entitled`, `entitlement_unknown`, or `would_roll_back` at most once per hour
(checked against `rollback_events` for that policy). Events older than 90 days are deleted by
`telemetry::sweep` (`crates/flags-core/src/telemetry.rs:389`), which `sweep_loop` already runs
hourly (`crates/flags-server/src/main.rs:101`); the `rollback_events_retention` policy allows
exactly that unpinned delete.

**Org deletion.** `platform_events::apply` purges children before parents by explicit table list
(`crates/flags-core/src/platform_events.rs:191-199`). Add `rollback_events` and
`rollback_policies` before `flag_errors` in that list. The cascades would also remove them, but
the list is how the purge reports what it deleted.

## 4. The checker

### Decision rule (pure)

`flags_core::rollback::decide(sample: &Sample, policy: &Thresholds) -> Decision`, where `Sample`
is `{ evals_on, evals_off, errors_on, errors_off }` over the policy's window. It trips when all
of:

1. `evals_on >= min_evaluations` and `errors_on >= min_errors`, and
2. any of
   - `rate_on > max_error_rate` (if set), or
   - `evals_off > 0`, `errors_off > 0`, and `rate_on > rate_off * max_error_ratio`, or
   - `evals_off > 0` and `errors_off == 0` (on is erroring, off is not).

With the defaults this is exactly `telemetry::assess`'s "Consider rolling back" condition
(`crates/flags-core/src/telemetry.rs:353-380`: 100 evaluations, 10 errors, 2x). `assess` is
refactored to call `decide` with default thresholds, so the advice an agent reads from
`flag_health` and the automatic action cannot disagree. `assess`'s rates are already `None` with
no evaluations on a side (`telemetry.rs:311-313`), so with the defaults and no `max_error_rate`
the two match exactly. `assess` keeps its wording; its tests are kept and pin the shared cases.

With no off traffic (a flag at 100%), only `max_error_rate` can trip a policy. That is allowed
(off traffic can appear later), but `set_rollback_policy` returns a `warning` saying so whenever
`max_error_rate` is unset, and `propose_rollback` reports `can_trip: false` with the reason.

### Sample

One query per flag, both sides aligned to the same start,
`since = date_trunc('hour', now()) - (window_hours - 1) hours`, so `window_hours = 1` means "this
hour so far". This deliberately differs from `flag_health`, which looks back `window_hours` from
now with an hour-truncated start for evaluations and an exact start for errors
(`telemetry.rs:250-263, 285-290`); the checker aligns both sides to the truncated start so the
rate's numerator and denominator cover the same span. The thresholds are shared with
`assess`; the window is not.

- **Source.** Unless the policy sets `include_client_reports`: evaluations are summed from
  `server_count`, errors from `reporter = 'server'` rows (NULL counts as client). With it,
  evaluations use `count` and errors use every row. So a default policy is judged on
  server-reported telemetry only: a client key can supply neither the errors nor the evaluations
  it is judged on.
- **Environment.** Evaluations match `environment = $env`. Errors match `environment = $env`, or
  `environment IS NULL` when `$env = 'production'` (premise 6).

### When it runs

- **On telemetry.** `flags-api`'s two telemetry handlers commit, then `try_send` the
  `(org, flag ids touched)` onto a bounded `tokio::mpsc` channel (capacity 1024) owned by the
  checker. The send never blocks the SDK request and never touches the platform; a full channel
  drops the hint (the backstop picks it up) and increments a counter logged once a minute.
- **Debounce.** The checker coalesces hints per flag and checks a flag at most once per 30 s.
  A hint for a flag checked less than 30 s ago is deferred to the end of that interval (trailing
  edge), never dropped, so the batch that crossed a threshold is always checked within 30 s.
- **Plumbing.** `telemetry::record_evaluations` and `record_errors` return the ids of the flags
  they touched alongside the `Receipt`, so the handler can send the hint without a second query.
- **Backstop.** While the process is awake, every 5 minutes it checks every armed policy whose
  `last_checked_at` is older than 5 minutes, at most 200 per pass, oldest first. This covers
  dropped hints and policies created after their telemetry arrived.

### Acting on a trip

For one flag, in this order:

1. Open a `begin_live(db, org, None)` transaction for the flag's org (it refuses deleted orgs;
   never `Db::begin`), read the flag's armed policies, and take each one's sample.
2. If `decide` does not trip: update `last_checked_at`, commit, done.
3. If it trips: **commit and drop the transaction**, then check the entitlement (network; no
   pooled connection held across it, as `Meter::warm` does).
4. Reopen `begin_live`, lock the flag (`lock`, `FOR UPDATE`), re-read the policy (still armed?
   still this mode?) and re-run `decide` on a fresh sample; the world may have moved during the
   network call.
5. Then by outcome:
   - `Granted` and `enforce`: if the environment is already disabled, mark the policy tripped
     without a new version. Otherwise apply `EnvPatch { enabled: Some(false), .. }` through the
     same code `set_environment` uses, with change `auto_rollback`, actor `None`, and reason
     `auto-rollback by policy <id>: error rate on <x>% vs off <y>% over <n> evaluations`
     (numbers only; never reported text, Invariant 12). This writes `flag_versions` and
     `pg_notify` in the same transaction (Invariant 10). Set `tripped_at`, insert a `rolled_back`
     event with the new version.
   - `Granted` and `observe`: insert `would_roll_back` (deduplicated); do not set `tripped_at`.
   - `Denied`: insert `not_entitled` (deduplicated).
   - `Unknown` (platform unreachable or timed out): insert `entitlement_unknown`
     (deduplicated); do not act. See Risks.
6. Update `last_checked_at` (every outcome, so suppressed policies are not re-picked on every
   backstop pass), and commit.

Auto-rollback writes are **not metered**: no user called a tool, and the platform's usage events
name a user. (Policy management through MCP is.)

### Re-arming

A tripped policy stays tripped until the environment is turned back on. `set_environment`, when
it changes `enabled` from false to true, clears `tripped_at` on that (flag, environment)'s policy
in the same transaction. So does a `rollback_flag` that re-enables it. An agent that turns the flag
back on without fixing the cause gets one more trip, not a loop.

## 5. MCP surface

All in a new `crates/flags-mcp/src/tools/rollback.rs`, registered in `tools/mod.rs` (`NAMES` and
the router), following the existing tools' shape (`caller` → `require_scope` → `tx` → `charge` →
domain call → commit).

| Tool | Scope | Billable | Plan-gated | Does |
|---|---|---|---|---|
| `set_rollback_policy` | `flags:write` | yes | yes | Create or replace the policy for (app, flag, environment); omitted thresholds take the defaults. |
| `remove_rollback_policy` | `flags:write` | no | no | Delete it (events cascade). Free and not plan-gated, so a downgraded or over-quota org can always clean up. |
| `list_rollback_policies` | `flags:read` | no | no | Policies for an app, or one flag; includes armed/tripped state. |
| `propose_rollback` | `flags:read` | no | no | Dry run: the current sample, the thresholds, and what `decide` says, for one policy or for ad-hoc thresholds on a flag and environment. Changes nothing. |
| `rollback_events` | `flags:read` | no | no | Recent events for a flag or app, newest first, at most 200. |

Plan gating in `set_rollback_policy` calls `Entitlements::check` before the transaction opens
(as `Flags::tx` warms the meter first, `crates/flags-mcp/src/server.rs:101-108`). `Denied` →
a new `Error::FeatureNotInPlan { feature, plan, upgrade_url }` (code `feature_not_in_plan`, not
retriable), modelled on `QuotaExceeded` (`crates/flags-core/src/error.rs:50-57`). `Unknown` → a
new `Error::EntitlementUnavailable` (code `platform_unavailable`, retriable), since a timeout is not
an `otto_resource::Error` and cannot reuse `Error::Platform`; creating a policy fails closed.
`Entitlements` is built with the same `upgrade_url` `usage::Meter` holds (`usage.rs:58`).

`usage::BILLABLE` gains `set_rollback_policy` only; the existing test
`every_named_tool_is_routed_and_metered_consistently` covers the routing.

`Error::FeatureNotInPlan` needs its arms in `Error::code()` / `retriable()`
(`crates/flags-core/src/error.rs`) and a case in `flags-mcp`'s
`every_error_carries_a_code_and_a_retriable_flag` test (`crates/flags-mcp/src/error.rs`).

Outputs reuse `out.rs` envelopes (an object root, never a bare array). `rollback_events` and
`propose_rollback` contain only numbers, ids, and enum values, no reported text.

## 6. Isolation, keys, and the SDK contract

- Both new tables: RLS + `_tenant_isolation` policy + composite FKs; covered by
  `tenant_isolation_is_enforced_on_every_tenant_table` (Invariants 1 and 4).
- No SQL outside `flags-core`; the checker lives in `flags-core::rollback` and is spawned by
  `flags-server` (Invariant 3).
- The SDK path gains no platform call (Invariant 6): the telemetry handlers only send an
  in-process hint.
- The SDK REST contract is unchanged (Invariant 7): error telemetry already identifies the key;
  `reporter` and `server_count` are derived server-side from the key's `KeyKind`, never from a request field.
- Client keys see nothing new (Invariant 5).
- Bucketing untouched (Invariant 13).

## 7. Testing

- **Unit (`flags-core/src/rollback.rs`):** `decide` across the boundary cases (sample below
  minimums; ratio trip; zero-off-errors trip; no off traffic without and with `max_error_rate`);
  `assess` and `decide` agree on every case `assess`'s tests already cover.
- **DB (`crates/flags-core/tests/db.rs`, `#[sqlx::test]`):** policy CRUD is org-isolated; the
  tenant-isolation test covers the new tables; the unpinned backstop query sees due policies
  of every org while a pinned transaction sees only its own; client-reported errors or
  evaluations do not trip a default policy, and do trip one with `include_client_reports`;
  server-key evaluation batches increment `server_count`; NULL-environment errors count toward a
  `production` policy and not a `staging` one; a trip writes one `auto_rollback` version
  with a NULL actor, sets `tripped_at`, inserts one `rolled_back` event, and touches only that
  environment; a tripped policy does not act again; turning the environment back on re-arms it;
  observe mode writes `would_roll_back` once per hour and never changes the flag; `org.deleted`
  purges the new tables; the 90-day sweep removes old events.
- **E2E (`crates/flags-server/tests/e2e.rs`, mock platform):** the mock's `usage-status` gains
  `features`. An org without `auto_rollback` gets `feature_not_in_plan` from
  `set_rollback_policy`; with it, the agent sets a policy, an app reports server-key errors over
  the SDK API, and within the debounce the flag is off in that environment, an SSE subscriber
  received the change, and `rollback_events` shows the trip. The SDK reports errors without an
  `environment` (as node-server does), to exercise the production default. A `usage-status` body
  without `features` (older platform) is treated as not entitled, and an unreachable platform
  as not entitled.
- **MCP:** `set_rollback_policy` without `max_error_rate` returns the no-off-traffic warning;
  `remove_rollback_policy` succeeds for an org whose plan lacks the feature.

## Assumptions

1. **Every paid plan gets `auto_rollback` (team, business, enterprise); free does not.**
   Decided by Rob on 2026-10-09. It is a seeded value in the platform migration, so it can move
   between plans later without a deploy of either service.
2. **The automatic action is "disable this environment".** It is the narrowest reversible
   change (premise 2), and the v1 plan already names the kill switch as a rollback form.
3. **Thresholds mirror `flag_health`'s advice** (100 evaluations, 10 errors, 2x), so enabling a
   policy with no tuning automates what agents are already told. The window differs (§4 Sample).
4. **Server-key telemetry only, by default** (premise 4), for errors and evaluations alike.
   Browser-only apps must opt in with `include_client_reports`, accepting that their public key
   can be used to trip it.
5. **Entitlement is checked at trip time, not only at creation,** so a downgrade stops
   automation within the 60 s cache, and a policy left behind is inert rather than deleted.
6. **Fail closed on an unknown entitlement** (confirmed by Rob on 2026-10-09). If the platform cannot answer, no rollback happens
   and `entitlement_unknown` is recorded. Paying for a capability and having it silently not
   work is bad; an unentitled org getting it is worse for the pattern this establishes. See
   Risks.
7. **Reuse `usage-status` rather than a new platform endpoint,** keeping one cached call.
8. **No new scope.** Policies are flag configuration; `flags:write` already covers changing a
   flag's environment, which is what a policy does.

9. **A NULL error environment means production** (premise 6), mirroring evaluations.

## Error handling & edge cases

- **Flag archived after a policy was set:** an archived flag evaluates off already; the checker
  skips it (no event). Restoring the flag resumes checks.
- **Environment removed from the app:** `set_environment` would refuse the patch
  (`flags.rs:363-371`); the checker records nothing and skips the policy. `list_rollback_policies`
  marks it `environment_missing`.
- **Version conflict:** the checker locks the flag itself and does not pass
  `expected_version`; an agent's concurrent `expected_version` write fails with the usual
  `VersionConflict` and re-reads.
- **Environment already off when the policy trips:** mark tripped, no new version, no event
  noise beyond the `rolled_back` event with `flag_version = NULL`.
- **Clock skew in reported `occurred_at`:** `record_errors` already bounds it (`sane`); the
  sample uses the stored values.
- **Org deleted between hint and check:** `begin_live` refuses; the hint is dropped.
- **Burst of telemetry across many flags:** the debounce plus one transaction per flag bounds
  connections to one at a time from the checker (it runs flags sequentially).

## Risks & open questions

- **Platform outage suppresses rollbacks** (assumption 6). Mitigation if it bites: remember the
  last `Granted` per org in memory for up to 24 h and use it when the platform is unreachable.
  Deferred until there is a paying org to protect.
- **Detection latency on a suspended machine** is bounded by telemetry flush intervals in the
  SDKs, not by the checker. Fine for v1; document it.
- **Hourly evaluation buckets** make "this hour so far" the smallest window. A spike early in an
  hour is diluted by nothing (the hour has just begun), but late in the hour it is diluted by up
  to an hour of healthy traffic. Acceptable for v1; a finer bucket is a separate change.
- **Forging with a leaked server key** can trip a policy. A leaked server key already allows far
  worse (reading every rule); rotation is the remedy.
- **Platform release ordering:** otto-flags must not merge until otto-platform 0.5.0 is released
  and deployed; otherwise every check is `Denied` (missing `features` deserializes empty).
- **Image rollback after `0002` fails at boot.** `MIGRATOR` uses sqlx's default
  `ignore_missing = false` and migrations run at startup, so the previous image refuses a
  database that has `0002` applied (`VersionMissing`). Recovery is roll-forward (fix and
  redeploy), not `fly deploy` of the old image. This is true of every future migration too; making
  the migrator tolerate unknown newer migrations is a separate decision, not taken here.
- **A policy without `max_error_rate` on a flag at 100%** cannot trip. Resolved as a warning
  rather than a refusal (§4 Decision rule), since off traffic may come back.

## Out-of-band

- **otto-platform 0.5.0** (otto-platform#30) released and deployed before the otto-flags PR
  merges; the otto-flags PR bumps the pin to that rev in a separate commit.
- **No new Fly secret or env var, no new scope, no platform re-registration.**
- **No SDK package change, no changeset.**
- **Docs:** `docs/deploy/fly.md` gains a short "Auto-rollback" note (plan gating, the
  telemetry-driven checker on a suspending machine); `README.md`'s tool list gains the five tools.
