# Meter production evaluations

**Status:** DRAFT
**Ticket:** savvagent/otto-flags#140 (PR 2 of 2; PR 1, #141, pinned otto-platform v0.10.0)

## Brief

Ticket #140, "Meter production evaluations instead of charging management tools", verbatim (untrusted
text, quoted as data):

> **Problem.** `flags-core`'s `BILLABLE` list (`crates/flags-core/src/usage.rs:40`), such as
> `create_flag`, `set_flag_environment` and `rollback_flag`, charges for agent management. Under the
> platform's new pricing (savvagent/otto-platform#55), management calls are free under fair use.
> otto-flags should bill on production use, which reaches the server today through SDK routes in
> `crates/flags-api/src/lib.rs:95-101` and is not metered at all.
>
> **Approach.**
> - Empty `BILLABLE`: every MCP tool becomes free.
> - Add a production meter fed from the SDK path. Proposed: `otto-flags/evaluations`. Server-side
>   `POST /api/evaluate/{key}` is counted exactly, and SDK-local evaluations are counted from
>   `POST /api/telemetry/evaluations`, aggregated per app per hour into one event with a `quantity`,
>   never one event per evaluation.
> - Bump `otto-tenant`/`otto-resource` to the platform release with meter, quantity and
>   `budget_exceeded`, and declare the meter at startup.
> - When an opt-in hard stop applies, evaluation must keep serving (a budget never turns flags off
>   in production). Only billing records stop. Agent writes remain free.
>
> **Acceptance criteria.**
> - [ ] No MCP tool writes a billable usage event.
> - [ ] Evaluations served by `/api/evaluate` and reported by telemetry ship as hourly aggregated
>   `otto-flags/evaluations` events per app, deduplicated across retries.
> - [ ] Exceeding a budget never changes an evaluation result or its latency.
> - [ ] `whoami` and `usage` report the meter and the budget state.
>
> **Out of scope.** Prices, and the platform-side billing (savvagent/otto-platform#55).
>
> **Risks & open items.**
> - Should the meter be evaluations, or monthly active contexts (the LaunchDarkly-style client
>   MAU)? Telemetry counts are client-reported and can be suppressed. Owner: @robhicks.
> - Blocked on the platform release that adds meters and quantity (savvagent/otto-platform#55).

The ticket's comment says platform v0.10.0 is out. It has `MeterDeclaration`,
`PlatformClient::declare_meters`, `service`/`meter`/`quantity` on `UsageEvent`, and
`meters`/`budget`/`blocked_reason` on `UsageStatus` with `billable_blocked()`. The comment also says
enforcement fails open when the platform can't be reached, and that the production slug
`otto-flags` is already set.

## Premise corrections

1. **"Only billing records stop" needs the platform's semantics.** On v0.10.0 a metered event's
   `quantity` is billed whatever its `billable` flag says (`UsageEvent::billable` docs: "A metered
   event's quantity counts whatever this says"). So "not billing" a blocked org means shipping its
   hour **without a meter**. Setting `billable = false` on a metered event is not enough.
2. **The SDK path must never call the platform** (Invariant), so it cannot know whether an org is
   blocked. The blocked check therefore happens off the request path, when an hour is closed (see
   Architecture), never at evaluation time. That is also what keeps criterion 3 (budget never
   changes a result or its latency) true by construction.
3. **"Counted exactly" for `/api/evaluate` cannot mean a database write per evaluation.** The
   evaluate handler does one read and commits before evaluating
   (`crates/flags-api/src/sdk.rs:102-104`). A write per evaluation on a 5-connection pool, plus a
   hot row per app, would change latency and load. "Exactly" here means every evaluation the
   server answered successfully is counted once, through an in-memory counter flushed every few
   seconds. A machine that crashes (not a graceful stop) loses at most one flush interval of
   counts. That is an undercount: no customer is billed for work it did not get.
4. **`FLAGS_ENFORCE_QUOTAS` is already inert.** Against v0.10.0, `UsageStatus::is_blocked()` is
   always false, and it is off in production. Once `BILLABLE` is empty, nothing is left for it to
   refuse, so it is retired (see Architecture).
5. **The two sources overlap; adding them would bill remote evaluations twice.** By default every
   SDK reports the evaluations the server answered for it, as well as its local and cached ones,
   through `/api/telemetry/evaluations`. Examples: `packages/typescript/src/client.ts:423-447`,
   `node-server` and `python-server`; telemetry is on by default (`enable_telemetry = True`,
   `enableTelemetry = true`). So "count both" from the ticket would count a remote evaluation once
   on `/api/evaluate` and again in telemetry. This spec keeps the two counts apart per (org, app,
   hour) and bills **the larger of the two**, never their sum (see Decisions).
6. **The evaluate endpoint is not `/api/evaluate/{key}` alone.** `POST /api/flags/{key}/evaluate`
   is the same handler (`crates/flags-api/src/lib.rs:95-96`). Both are counted.

## Decisions

- **Billed quantity per (org, app, hour) = `GREATEST(served, reported)`.**
  - `served` counts the successful answers from the evaluate handler.
  - `reported` counts the accepted telemetry evaluation events **sent with the server key**.
    Client-key telemetry is never billed (see "Only server-key telemetry is billed" below).
  - Why the larger of the two is right:
    - With telemetry on (the default), `reported` already includes every served evaluation, plus
      local and cached ones, so `reported` ≥ `served` and the bill is what the SDK reported.
    - With telemetry off or suppressed, `reported` is 0 or low, and the bill falls back to the work
      the server actually did. Turning telemetry off therefore cannot zero a bill for server-side
      evaluation.
  - No SDK changes, and the REST contract is unchanged.
  - **The rule is a per-hour approximation, and it is not exact in either direction.**
    - **Undercount:** in an hour where an app's SDKs mix telemetry on and off, it bills the larger
      source, not the exact union.
    - **Overcount, bounded and small:** an evaluation's `served` count lands when the server
      answers it, but its `reported` count arrives with the SDK's next telemetry batch. The SDKs
      flush every 5 s (`packages/typescript/src/telemetry.ts:11`, `node-server`, `rust-server`),
      plus retries, and both counts are bucketed by server time. An evaluation served in the last
      seconds of hour `H` can therefore count as served in `H` and as reported in `H+1`. When an
      hour's `served` exceeds its `reported`, which happens mostly to apps that evaluate remotely
      with telemetry on, that evaluation is billed in both hours. The overcount is at most the
      served evaluations in one telemetry lag (about 5 s) per app per hour edge, roughly 0.15 % of
      such an app's served volume.
    - This is accepted and documented: removing it would need a correlation id between the two
      sources (an SDK and REST-contract change). It is recorded under Risks, alongside the MAU
      question, for Rob when prices are set.

## Assumptions

- **Meter = evaluations, not monthly active contexts.** The ticket proposes evaluations; MAU is
  its open question, and its owner is Rob. Choosing evaluations matches the ticket's Approach and
  needs no stored end-user identifiers. Storing them would conflict with the invariant "no
  end-user context persisted". The MAU question stays open for Rob as a follow-up ticket.
- **Meter declaration:**
  - key `otto-flags/evaluations`
  - display name `Flag evaluations`
  - unit `evaluation` (the example in otto-resource's own `declare_meters` docs).
- **What counts (the two inputs to the Decision above):**
  - `served`: each successful answer from the evaluate handler (flag found and active, response
    built).
  - `reported`: each telemetry evaluation event the server accepted (`Receipt::accepted`: a known
    flag of the calling app), in a batch authenticated with the app's server (`srv_`) key.
  - Unknown-flag telemetry events, rejected or failed requests, `GET /api/sdk/flags`, the SSE
    stream, and MCP `evaluate_flag` (a dry run, free per the Metering invariant) are not counted.
- **Only server-key telemetry is billed.** This was added after an automated security review
  flagged client-key telemetry as a billing-abuse path. The client (`sdk_`) key is public: it
  ships in every page that evaluates flags. Anyone could post up to 1,000 fake telemetry events
  per request with it and raise the org's bill. So:
  - `served` counts evaluations answered for both key kinds. That is real work the server did,
    and each one costs the caller a request.
  - `reported` counts only batches sent with the secret server key.
  - Client-key telemetry is still recorded in `flag_eval_hourly` for health and auto-rollback,
    unchanged. It never reaches the meter.
  - The cost: a browser or mobile SDK's local and cached evaluations are not billed. Only the
    ones it fetches from the server are. That is an undercount, never an overcount.
- **Bucketing uses the server's receive time** (the database's `now()` when the counts are
  flushed), not the SDK-reported timestamp. That lets an hour close for good (see the hourly
  close), and a client clock cannot move usage between billing months. `flag_eval_hourly` keeps
  using the reported time for health, unchanged.
- **Blocked means `UsageStatus::billable_blocked()`**, which covers both `budget_exceeded` and
  `payment_required`. A blocked org's evaluations keep serving and its closed hours ship unmetered
  (free), the same rule otto-factory applies to tracker closes for a blocked org. If the platform
  cannot answer the status lookup, the hour is billed: enforcement fails open, as the ticket's
  comment says.
- **Run Profile:** CLAUDE.md's Development Workflow section is complete, and no key was assumed.
  It names `docs/deploy/fly.md` and the crate module docs as the details for invariants.

## Goal and success criteria

otto-flags bills on production use instead of agent management. A budget can stop the billing,
never the flags.

- No MCP tool writes a billable usage row. `BILLABLE` is empty, and a test asserts every
  registered tool is free.
- Every successful SDK evaluation and every accepted telemetry evaluation is counted in its own
  source. After its hour closes, each (org, app, hour) ships as one `otto-flags/evaluations` usage
  event with `quantity = GREATEST(served, reported)`. The two sources are never summed, so a
  remote evaluation is not billed once per source. The only double billing is the bounded
  hour-edge lag described in Decisions. The event id is minted once in the outbox, so retries are
  deduplicated by the platform.
- An org that is blocked when its hour closes gets that hour shipped unmetered. Evaluation code
  never reads platform or budget state, which a test and the "SDK path never calls the platform"
  invariant guard.
- `whoami` and `usage` report `meters`, `budget` and `blockedReason`. The legacy fields stay,
  marked deprecated.
- The meter is declared at startup, before any usage ships.

## Scope

**In:**
- `BILLABLE` emptied and quota enforcement removed.
- An in-memory evaluation counter in `flags-core`, fed by the evaluate and telemetry handlers.
- A new hourly table and migration `0007`, plus `meter`/`quantity` on the outbox tables.
- A background metering task that flushes the counter and closes hours into the outbox.
- The shipper ships `meter`/`quantity` and declares the meter first.
- The `whoami`/`usage` report shape.
- `FLAGS_ENFORCE_QUOTAS` retired.
- MCP instruction and tool text.
- Docs: CLAUDE.md invariants, `docs/deploy/fly.md`, `.env.example`.

**Out:**
- Prices, and the platform side.
- A monthly-active-contexts meter.
- Per-key rate limits or caps on metered quantity (Risks).
- Re-declaring on `unknown_meter` and a readiness signal for a stalled declaration. These are
  the same follow-ups otto-factory filed (#224, #225); flags gets its own tickets.
- SDK changes: no SDK or REST contract changes.

## Architecture

### Public-interface changes

| Interface | Change | Additive / breaking |
|---|---|---|
| MCP `whoami`.`usage` and `usage` result | Adds `meters`, `budget` and `blockedReason`. Existing fields are kept and documented as deprecated. `enforced` now always reports `true`. | Additive. `enforced` changes meaning, documented. |
| MCP error code `quota_exceeded` | No longer produced. Against v0.10.0 it could not fire anyway (`is_blocked()` is always false). | Breaking in principle (a code disappears), inert in practice. |
| `FLAGS_ENFORCE_QUOTAS` | Retired: still parsed so a set value does not fail boot, logs a startup warning, and has no effect. | Breaking for anyone relying on `true`, which already did nothing. |
| MCP tool descriptions and server instructions | Text: every tool is free, and billing is on production evaluations. | Additive. |
| SDK REST contract | Unchanged: same routes, bodies, responses and timing. | None. |
| Usage events to the platform | Hour-close events carry `meter`/`quantity`. Per-call MCP events are all `billable: false`. | Additive (the v0.10.0 contract). |

Squash commit footer: `BREAKING CHANGE: FLAGS_ENFORCE_QUOTAS is retired and the quota_exceeded MCP
error is no longer produced; MCP tools are free and production evaluations are metered.` (The
server is continuously deployed and has no release-please versioning, so the footer is a record,
not a version driver.)

### Components and data flow

```
SDK ──POST /api/evaluate/{key}──▶ sdk::evaluate ──ok──▶ EvalCounter.served(org, app, 1)          (memory)
SDK ──POST /api/telemetry/evaluations──▶ telemetry::evaluations ──commit──▶ EvalCounter.reported(org, app, accepted)

metering::run (background, one per machine)
  every FLUSH_EVERY (10 s), and once more on shutdown:
     take the counter's map ──▶ maintenance tx (statement and idle timeouts set): UPSERT
                                usage_meter_hourly(org, app, date_trunc('hour', now(), 'UTC'), served, reported)
                                (skips tombstoned orgs; a pair whose app is gone is refused by the FK and dropped; on failure the counts are added back)
  every CLOSE_EVERY (60 s):
     1. maintenance tx: SELECT DISTINCT org_id of due hours (hour + 1 h + CLOSE_GRACE <= now()), LIMIT 100; commit
     2. per org, no connection held: Meter::billable_blocked(org) → BlockedLookup (timeout 2 s; Failed ⇒ bill)
     3. maintenance tx: DELETE due rows of those orgs RETURNING … ; INSERT INTO usage_outbox
          n = GREATEST(served, reported)
          billed org  → tool 'sdk_evaluations', billable true,  meter 'otto-flags/evaluations', quantity n
          blocked org → tool 'sdk_evaluations', billable false, meter NULL, quantity NULL
        (one statement, so a row is closed exactly once even with two machines)

usage::run (existing shipper)
  first: declare_meters([otto-flags/evaluations]) with backoff until the receipt is complete
  then: ship_once as today, now carrying meter and quantity
```

**`flags_core::metering::EvalCounter`** (new module `crates/flags-core/src/metering.rs`):
- `Mutex<HashMap<(OrgId, FlagAppId), Counts>>`, where `Counts { served: u64, reported: u64 }`.
  Methods: `served(org, app, n)` and `reported(org, app, n)` (both saturating), `take() ->
  HashMap`, and `restore(map)` to add back counts a failed flush took.
- Shared as `Arc<EvalCounter>`. No method blocks on I/O, and the lock is held for one map
  operation.
- The map is bounded by the number of (org, app) pairs evaluated within one flush interval, each
  of which had to present a valid key.

**Handlers (`crates/flags-api`):**
- `AppState` gains `evaluations: Arc<EvalCounter>`.
- `sdk::evaluate` calls `served(org, app, 1)` after the evaluation result is computed, just before
  returning `Ok`.
- `telemetry::evaluations` calls `reported(org, app, receipt.accepted)` after `tx.commit()` succeeds,
  only when `accepted > 0`. A failed commit counts nothing.
- No statement is added to either request transaction.

**`usage_meter_hourly`** (migration `0007`): one row per (org, app, hour) that saw evaluations,
deleted when its hour closes. Bounded by the active apps over the last ~2 hours.

**Hour close (`metering::close_due`):**
- An hour `H` is due when `H + 1 h + CLOSE_GRACE <= now()`, with `CLOSE_GRACE` = 5 min. A flush
  writes `hour = date_trunc('hour', now(), 'UTC')` using the database clock at its transaction
  start.
- Every flush and close transaction starts with `SET LOCAL statement_timeout = '30s'` and
  `SET LOCAL idle_in_transaction_session_timeout = '30s'`. These are enforced by the server, so
  they hold even when the Fly machine is suspended mid-transaction (`auto_stop_machines =
  "suspend"`): a flush transaction that began in hour `H` commits or aborts well before
  `H + 1 h + 5 min`. A late flush therefore normally cannot write into a closed hour.
- **The guarantee is "counts are never summed across sources, and a closed row is never billed
  again", not "one outbox row per hour".** If a write
  ever lands in a closed hour anyway, it creates a new row for that hour, closed later into a
  second outbox event. That event carries only the new counts, because the first row was deleted
  when it closed. Tests assert the weaker, true guarantee.
- A due row's quantity above `otto_resource::MAX_METER_QUANTITY` (10⁹) is split across
  several outbox rows (`generate_series`). That is unreachable in practice, but the platform would
  reject the event otherwise.
- `occurred_at` = `H`, which decides the billing month while that month is open. While a machine
  is running, an hour closes about 65 minutes after it ends, inside the platform's 2-hour
  month-end window. The app scales to zero (`min_machines_running = 0`, suspend), so an idle app
  closes its hours at the next wake. If that wake comes after the month's cutoff, the platform
  bills those hours in the month they arrive (the `UsageEvent::occurred_at` contract). Nothing is
  lost; this is stated, not hidden, in `docs/deploy/fly.md`.
- Each close pass takes at most 100 due orgs. The blocked lookups run in sequence, and the first
  lookup failure in a pass skips the rest of that pass's lookups: those orgs are billed (fail
  open). A platform outage therefore costs at most one 2-second timeout per pass.

**Blocked lookup (`usage::Meter::billable_blocked(org) -> BlockedLookup`):**
- Reuses the meter's platform client (60 s cache) with the existing 2 s timeout.
- Returns `Blocked`, `NotBlocked` or `Failed`. Any failure is `Failed`, which bills (fail open)
  and stops the pass's further lookups. It logs at `warn` with the text-free `Failure` as
  `status_for` does today, and does not remember failures (`fail_open` map removed, see below).
- It is called only from the metering task, never from a request path.

**Shipper (`usage.rs`):**
- `Row` and the claim `RETURNING`, the `UsageEvent` mapping, and the rejected-move CTE carry
  `meter` and `quantity`. `service` is sent as `None`: the platform knows the caller.
- `run` first runs `declare_meters`, which is a port of otto-factory's: it declares
  `METER_DECLARATIONS` with capped exponential backoff, logs at `error` once a minute while
  failing, returns `false` on shutdown, and treats an incomplete receipt as failure.
- Shipping starts only after the declaration succeeds. Rows wait in the outbox meanwhile and
  nothing is lost.

**MCP (`flags-mcp`) and `Meter`:**
- `BILLABLE` becomes `&[]`. The `is_billable` signature stays.
- `Meter::charge` only records (`billable` always false), with no platform lookup.
- `Meter::warm`, `enforce`, the `fail_open` map and `Error::QuotaExceeded` (plus its MCP
  mapping) are removed.
- `Flags::tx` no longer warms.
- `Meter::report` returns the new `Status`.

**`Status`** mirrors otto-factory's `of_billing::Status`:
- `plan`, `meters: Vec<MeterStatus>`, `budget: Option<BudgetReport>` (with `reason`), and
  `blocked_reason: Option<String>`.
- The deprecated legacy fields (`included_ops`, `billable_used`, `remaining`, `total_calls`,
  `period_start`, `warning`, `hard_stop`) stay. `enforced` is now always `true`.
- `blocked_reason` is set only while `billable_blocked()` is true. It is `payment_required` when
  the platform says so, and `budget_exceeded` otherwise, including when the platform gave no
  reason or one this release does not know (as otto-factory's `blocked_code`), so a caller always
  gets a code it can branch on.
- Docs say what blocked means here: evaluations keep serving and are not billed while it holds.

**Server (`flags-server`):**
- `main.rs` builds one `Arc<EvalCounter>`, hands it to the router's `AppState` and to
  `metering::run`, spawned beside the shipper.
- On shutdown, the metering task does a final flush. It is awaited before "stopped", like the
  shipper.
- `config.rs` keeps parsing `FLAGS_ENFORCE_QUOTAS` and warns at startup when it is set
  (`Config::warn_retired`, as in otto-factory). The `enforce_quotas` field and its MCP plumbing
  go.

### Invariants impact

- **Append-only migrations:** a new `0007_usage_meter.sql`; nothing existing is touched.
- **Rollout and rollback compatibility:** everything is additive.
  - The new columns are nullable.
  - The new table is unknown to the previous release.
  - The CHECKs hold for every row the previous release writes (meter and quantity both NULL).
  - The previous release's shipper `RETURNING` omits `meter`/`quantity`. During a rollout overlap,
    or after a rollback, it ships an hour-close row as a plain unmetered event, and that hour's
    billing is lost. The previous release also does not read `usage_meter_hourly`: counts wait
    there and are closed once the new release runs again.
  - Production runs one machine and no price is set yet, so the cost today is zero.
    `docs/deploy/fly.md` records this as a rollback note.
- **Tenant isolation:** `usage_meter_hourly` enables and forces RLS and grants to `otto_app`, with:
  - the tenant policy `org_id = current_org() OR current_org() IS NULL`
  - the restrictive maintenance-marker policy (as 0005 does for the outbox)
  - coverage by `tenant_isolation_is_enforced_on_every_tenant_table`.
  The flush and the close run in `maintenance::begin`, with no raw `.pool()` query.
- **Composite FK:** `(org_id, app_id) REFERENCES flag_apps (org_id, id) ON DELETE CASCADE`.
- **Request paths use `begin_live`:** unchanged. The handlers add no transaction; the counter is
  in memory.
- **No SQL outside `flags-core`**, runtime queries only: all new SQL lives in `metering.rs` and
  `usage.rs`.
- **The SDK path never calls the platform:** it touches only the in-memory counter. The
  `billable_blocked` lookup lives in the background task.
- **The SDK REST contract is stable:** unchanged.
- **Metering invariant:** rewritten in CLAUDE.md:
  - MCP tools record free usage rows in their own transaction.
  - Production evaluations are counted in memory, flushed to `usage_meter_hourly`, and closed
    hourly into `usage_outbox` as `otto-flags/evaluations`.
  - Reads, dry-run evaluation and MCP tools are never billed.
- **Evaluations are counted, not stored:** `flag_eval_hourly` is unchanged (telemetry only).
  `usage_meter_hourly` holds counts only, with no flag, context or end-user data. The invariant
  text gains one clause saying so.
- **The database is small and shared:**
  - A flush is one statement per 10 s per machine.
  - A close is two short transactions per minute.
  - The table is bounded and has no per-row fan-out: each step is one set-based statement.
  - The platform lookup happens with no connection held.
- **Platform webhooks:** the `org.deleted` purge list gains `usage_meter_hourly`, placed before
  `flag_apps`. The FK cascade from `flag_apps` would remove its rows anyway; the explicit entry
  makes the count visible in the purge's detail. A close that commits first moves the rows to
  `usage_outbox`, which the purge clears later in its list. The flush skips orgs in
  `deleted_orgs`, so it never recreates rows after a purge.
- **otto-platform pin:** unchanged (#141).

## Migration

`crates/flags-core/migrations/0007_usage_meter.sql`:

```sql
SET LOCAL lock_timeout = '3s';

ALTER TABLE usage_outbox_rejected
  ADD COLUMN meter text,
  ADD COLUMN quantity bigint,
  ADD CONSTRAINT usage_outbox_rejected_meter_quantity
    CHECK ((meter IS NULL) = (quantity IS NULL) AND (quantity IS NULL OR quantity > 0));
ALTER TABLE usage_outbox
  ADD COLUMN meter text,
  ADD COLUMN quantity bigint,
  ADD CONSTRAINT usage_outbox_meter_quantity
    CHECK ((meter IS NULL) = (quantity IS NULL) AND (quantity IS NULL OR quantity > 0));

CREATE TABLE usage_meter_hourly (
  org_id   uuid        NOT NULL,
  app_id   uuid        NOT NULL,
  hour     timestamptz NOT NULL,
  -- Successful /api/evaluate answers, and accepted telemetry evaluation events. Billed as the
  -- larger of the two, never their sum (telemetry also reports server-answered evaluations).
  served   bigint      NOT NULL DEFAULT 0 CHECK (served >= 0),
  reported bigint      NOT NULL DEFAULT 0 CHECK (reported >= 0),
  PRIMARY KEY (org_id, app_id, hour),
  FOREIGN KEY (org_id, app_id) REFERENCES flag_apps (org_id, id) ON DELETE CASCADE
);
CREATE INDEX usage_meter_hourly_hour_idx ON usage_meter_hourly (hour);

ALTER TABLE usage_meter_hourly ENABLE ROW LEVEL SECURITY;
ALTER TABLE usage_meter_hourly FORCE ROW LEVEL SECURITY;
CREATE POLICY usage_meter_hourly_tenant_isolation ON usage_meter_hourly
  USING (org_id = current_org() OR current_org() IS NULL)
  WITH CHECK (org_id = current_org() OR current_org() IS NULL);
CREATE POLICY usage_meter_hourly_maintenance ON usage_meter_hourly
  AS RESTRICTIVE FOR ALL
  USING (current_org() IS NOT NULL OR current_setting('app.maintenance', true) = 'on')
  WITH CHECK (current_org() IS NOT NULL OR current_setting('app.maintenance', true) = 'on');
-- No GRANT: 0001's ALTER DEFAULT PRIVILEGES already gives otto_app SELECT, INSERT, UPDATE and
-- DELETE on new tables. A bare GRANT would fail on a database without the role (0002 adds a table
-- the same way).
```

Lock order: `usage_outbox_rejected` then `usage_outbox`, matching 0005 and the previous release's
shipper. Adding a CHECK scans the table under the lock. Both outbox tables are small (shipped rows
are deleted, and rejected rows are kept 90 days), and `lock_timeout` makes the migration fail fast
rather than queue.

## Out-of-band

- **Migration (agent):** applies at boot. Confirm `applying migrations` and that boot completes
  (Ship > verify).
- **Platform service slug:** already set in production (`service=otto-flags`, per the ticket
  comment).
  Without it the declaration would retry and nothing would ship.
- **`FLAGS_ENFORCE_QUOTAS`:** if it is set as a Fly env or secret, it now only warns. `fly.toml`
  does not set it. Removing it, if it exists as a secret, is Rob's (`fly secrets unset`), and only
  cosmetic.
- No new secret, config key, scope or recurring cost.

## Error handling and edge cases

- **Flush failure:**
  - The taken counts are added back to the counter, saturating, and retried next tick.
  - It logs at `warn` with a text-free reason.
  - The counter stays bounded by the active (org, app) pairs.
- **Shutdown:** a final flush, bounded by a server-side 5 s timeout (`flush_with(…, "5s", 0)`,
  never an outer tokio timeout: `flush` is not cancel-safe). Counts it cannot write are lost and
  logged with their total.
- **Crash:** at most one interval of counts is lost (an undercount).
- **Close failure:** the transaction rolls back, the rows stay and are retried next minute.
  Nothing is double-closed.
- **Platform lookup failure at close:** the hour is billed (fail open), logged at `warn`.
- **Org or app deleted between flush and close:** the purge deletes the org's rows. At flush,
  pending counter entries are filtered by `deleted_orgs`. A join on `flag_apps` is impossible in a
  maintenance transaction: `flag_apps` has only the pinned tenant policy, so it reads as empty
  unpinned. Instead, the composite FK (whose checks bypass RLS) refuses a gone app, and the flush
  retries that batch row by row in savepoints, dropping the refused rows. So a race
  never fails the whole batch on the FK.
- **App deleted:** its hourly rows cascade away, unbilled. Apps are not normally deleted.
- **Two machines:** each flushes its own counter (the UPSERT adds them). The close's
  `DELETE … RETURNING` takes each row once.
- **Quantity overflow:**
  - The counter saturates at `u64::MAX`.
  - The flush clamps each value to `i64::MAX` before binding.
  - An overflow in the `bigint` UPSERT would fail the statement, so the UPSERT adds with a
    numeric clamp: `LEAST(existing::numeric + new, 9223372036854775807)::bigint`.
- **Unknown meter:** the platform rejects the event, and it lands in `usage_outbox_rejected` with
  its meter kept (re-declaration is a follow-up).

## Testing approach

Gates:
- `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
- clippy, fmt, the migrations-append-only check
- `podman build -t otto-flags:check .`

Unit tests (`metering.rs`, `usage.rs`):
- The counter's add, take, saturation and add-back.
- `BILLABLE` is empty, and every tool in `flags_mcp::tools::ALL` (or the registered list) is free.
- The `Status` mapping, including blocked without a reason giving `budget_exceeded`, and the
  budget reason.
- Declaration retry and success, against the e2e mock.

DB tests (`crates/flags-core/tests/db.rs`):
- Flush upserts and adds, skips tombstoned orgs, and re-adds on failure.
- Close:
  - moves only due hours
  - writes metered rows for a billed org and unmetered rows for a blocked org
  - splits above `MAX_METER_QUANTITY` (via a test-only quantity)
  - never closes twice under concurrent calls
- The `org.deleted` purge removes hourly rows.
- Tenant isolation covers the new table.
- A flush transaction left idle past `idle_in_transaction_session_timeout` is aborted, and its
  counts are restored to the counter (with a short timeout in the test).
- The new table needs the maintenance marker when unpinned, as
  `usage_outbox_tables_need_the_maintenance_marker_unpinned` checks for the outbox.
- The CHECKs refuse half-metered rows.
- The shipper ships `meter`/`quantity`, and the rejected move keeps them.

E2E (`crates/flags-server/tests/e2e.rs`, with the mock platform extended for `PUT /internal/meters`,
`meters`, `budget` and `blocked_reason`):
- An `/api/evaluate` call plus a telemetry batch, then flush and close (with the clock condition
  driven by inserting past hours directly, or a test hook), produce one metered event per app
  with quantity = `GREATEST(served, reported)` at the mock.
- A blocked org's hour ships unmetered.
- An MCP write is recorded as not billable.
- `whoami` and `usage` show `meters`, `budget` and `blockedReason`.
- An evaluation returns the same result while the mock reports `budget_exceeded`, and makes no
  platform call on the request. The test asserts this with the metering task not spawned, so a
  background close cannot touch the mock's usage-status hit counter.
- Billing takes `GREATEST`: an app with 3 served and 5 reported is billed 5, and an app with 4
  served and 0 reported is billed 4.

## Risks and open questions

- **Inflation through public client keys.** Client-key telemetry is not billed (Assumptions), so
  faked reports cannot raise a bill. What remains is that anyone holding an app's `sdk_` key can
  call `/api/evaluate` repeatedly. Each such call is real served work, paid for with a request
  per evaluation. Mitigations:
  - The budget's opt-in hard stop caps the damage: blocked hours ship free.
  - A follow-up ticket for per-key rate limits or a per-app hourly cap (owner Rob, because a cap
    is a pricing decision), before prices are set.
- **The MAU question** (ticket open item, owner Rob): a follow-up ticket.
- **The hour-edge overcount** (see Decisions): bounded by about 5 s of served evaluations per app
  per hour. It goes in the same follow-up as MAU.
- **Blocked orgs evaluate for free**, including `payment_required` ones. The ticket requires
  that flags never turn off, so this is the intended trade-off. It is the same rule as
  otto-factory's free tracker closes.
- **Old-shipper loss during overlap or rollback:** see Invariants. One machine, no price, so the
  cost today is zero.
- **Re-declare on `unknown_meter` and a stalled-declaration signal:** follow-up tickets, mirroring
  otto-factory #224 and #225.
