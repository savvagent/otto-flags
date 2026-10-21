# Meter Production Evaluations Implementation Plan

**Goal:** Make every MCP tool free and bill production evaluations on the `otto-flags/evaluations`
meter, as specced in `docs/specs/2026-10-20-meter-production-evaluations-design.md`
(savvagent/otto-flags#140, PR 2 of 2).

**Global constraints:**
- Gates (from CLAUDE.md, run from the worktree):
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`.
    DB tests run against the one shared local Postgres, so never run two at once.
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra, after committing: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'`
    must print nothing.
  - container: `podman build -t otto-flags:check .` (the final run).
- Evidence: pass counts per test binary. A "DATABASE_URL must be set" panic means the variable is
  missing, not a failed test.
- `sqlx::migrate!` has no build.rs: after adding a migration, run `touch crates/flags-core/src/migrate.rs`
  (or `cargo clean -p flags-core`) so the new file is embedded.
- Invariants to keep:
  - append-only migrations that are rollout- and rollback-compatible
  - RLS under `otto_app`, with unpinned access only through `maintenance::begin`
  - composite `(org_id, id)` FKs
  - no SQL outside `flags-core`, runtime queries only
  - the SDK path never calls the platform
  - the SDK REST contract is unchanged
  - the Metering invariant as rewritten by the spec
  - a small shared DB: no per-row fan-out, no connection held across a platform call
- Worktree only; trunk only via PR. Commits are conventional (`<type>(<scope>): <imperative,
  lower-case>`) with a why-body, ending `Refs #140`. The PR body carries `Fixes #140`.
- No attribution of any kind.
- Constants (`crates/flags-core/src/metering.rs`):
  - `FLUSH_EVERY = 10 s`
  - `CLOSE_EVERY = 60 s`
  - `CLOSE_GRACE = 5 min`
  - `CLOSE_BATCH_ORGS = 100`
  - `TX_TIMEOUT = "30s"`, used for both `statement_timeout` and `idle_in_transaction_session_timeout`
  - `METER_EVALUATIONS = "otto-flags/evaluations"`
  - `METER_DECLARATIONS = &[(METER_EVALUATIONS, "Flag evaluations", "evaluation")]`
  - `EVALUATIONS_TOOL = "sdk_evaluations"`

### Task 1: Migration 0007 and the purge

**Model:** sonnet

**Files:**
- `crates/flags-core/migrations/0007_usage_meter.sql` (new)
- `crates/flags-core/src/platform_events.rs`
- `crates/flags-core/tests/db.rs`

- [ ] Write the failing DB tests in `tests/db.rs`:
  - `usage_meter_hourly` is in the tenant-isolation proof. The existing
    `tenant_isolation_is_enforced_on_every_tenant_table` discovers tables, so assert it passes with
    the new table present.
  - The new table needs the maintenance marker when unpinned. Extend the table list in
    `usage_outbox_tables_need_the_maintenance_marker_unpinned`, or add a sibling test, with
    `usage_meter_hourly`.
  - The outbox CHECKs refuse `meter` without `quantity`, `quantity` without `meter`, and
    `quantity = 0`, and accept both NULL or both set, on both outbox tables.
  - The `org.deleted` purge removes the org's `usage_meter_hourly` rows and reports their count in
    its detail.
- [ ] Run the test gate and see these fail.
- [ ] Add `0007_usage_meter.sql` exactly as in the spec's Migration section: `lock_timeout`, the
  rejected table first, the paired CHECKs, the new table with the `served`/`reported` columns, the
  composite FK, the hour index, RLS enable and force, the two policies, and no GRANT.
- [ ] Add `"usage_meter_hourly"` to the purge list in `platform_events.rs`, just before
  `"flag_apps"`, with a one-line comment on why it is there.
- [ ] Touch `migrate.rs`, then run the test gate and see the tests pass. Also run lint and format.
- [ ] Commit `feat(core): add the hourly evaluation meter table and outbox meter columns`.

### Task 2: The evaluation counter and its flush

**Model:** sonnet

**Files:**
- `crates/flags-core/src/metering.rs` (new)
- `crates/flags-core/src/lib.rs` (`pub mod metering;`)
- `crates/flags-core/tests/db.rs`

- [ ] Write failing unit tests in `metering.rs`:
  - `served`/`reported` accumulate per (org, app).
  - `take` empties the counter.
  - `restore` adds counts back without losing concurrent adds.
  - The counts saturate at `u64::MAX`.
- [ ] Write failing DB tests:
  - `flush` upserts `served`/`reported` into hour `date_trunc('hour', now(), 'UTC')` and adds to
    an existing row.
  - It skips orgs in `deleted_orgs` and pairs whose app is missing from `flag_apps`, without
    failing the rest of the batch.
  - Values above `i64::MAX` are clamped.
  - On a DB error, the taken counts are restored.
  - A flush transaction left idle past its timeout is aborted. Drive this with a test-only
    shorter timeout, for example `flush_with_timeout(db, counter, "200ms")` holding the
    transaction open before the UPSERT, and check that the counts are restored.
- [ ] Run and see them fail.
- [ ] Implement in `metering.rs`:
  - `Counts { served: u64, reported: u64 }`
  - `EvalCounter`, a `Mutex<HashMap<(OrgId, FlagAppId), Counts>>`, with `served`, `reported`,
    `take` and `restore`. A poisoned lock recovers with `into_inner`, never a panic on the request
    path.
  - `pub async fn flush(db: &Db, counter: &EvalCounter) -> Result<usize>`:
    - It runs in `maintenance::begin` with `SET LOCAL statement_timeout` and
      `SET LOCAL idle_in_transaction_session_timeout` = `TX_TIMEOUT`.
    - It issues one `INSERT … SELECT FROM unnest(...) JOIN flag_apps … WHERE NOT EXISTS
      (deleted_orgs) ON CONFLICT (org_id, app_id, hour) DO UPDATE` that adds with the numeric
      clamp.
    - On any error it restores and returns the error.
  - Module docs explain the served/reported split and the `GREATEST` rule, citing the spec.
- [ ] Run the tests and see them pass. Run lint and format.
- [ ] Commit `feat(core): count served and reported evaluations in memory and flush them hourly`.

### Task 3: Closing hours into the outbox

**Model:** opus (exactly-once closing under concurrency, and blocked handling)

**Files:**
- `crates/flags-core/src/metering.rs`
- `crates/flags-core/src/usage.rs` (`Meter::billable_blocked`)
- `crates/flags-core/tests/db.rs`

- [ ] Write failing DB tests:
  - Only due hours (`hour + 1 h + CLOSE_GRACE <= now()`) close. Insert the rows with explicit past
    hours.
  - A billed org gets an outbox row with `tool = 'sdk_evaluations'`, `billable = true`,
    `meter = 'otto-flags/evaluations'`, `quantity = GREATEST(served, reported)` and
    `occurred_at = hour`.
  - A blocked org gets `billable = false` with `meter`/`quantity` NULL.
  - Two concurrent `close_due` calls close each row exactly once (total outbox quantity equals the
    total due).
  - A quantity above a test-only cap is split into several rows that sum to it. The cap is a
    parameter defaulting to `otto_resource::MAX_METER_QUANTITY`.
  - Rows with `GREATEST = 0` (both zero) are deleted without an outbox row.
  - At most `CLOSE_BATCH_ORGS` orgs are closed per pass.
- [ ] Run and see them fail.
- [ ] Implement:
  - `usage::Meter::billable_blocked(&self, org) -> bool`: the platform `usage_status` with the
    existing 2 s timeout, then `UsageStatus::billable_blocked()`. Any failure is `false`, logged at
    `warn` with the text-free `Failure` (as `status_for` does). It is never called on a request
    path.
  - `metering::close_due(db, blocked: impl Fn(OrgId) -> Future<bool>, cap) -> Result<usize>`:
    1. In a maintenance transaction with the timeouts, select up to `CLOSE_BATCH_ORGS` distinct
       due org ids, then commit.
    2. With no connection held, look up blocked per org, in sequence. After the first failed
       lookup, treat the remaining orgs in the pass as not blocked. `billable_blocked` returns
       `false` on failure, so a `BlockedLookup` result type is acceptable for telling the two
       apart.
    3. In a maintenance transaction with the timeouts, run one statement:
       `WITH gone AS (DELETE … WHERE due AND org_id = ANY($orgs) RETURNING …) INSERT INTO
       usage_outbox (…) SELECT …`, splitting with `generate_series` over `ceil(q / cap)` and
       choosing metered or unmetered by `org_id = ANY($blocked)`.
    - It logs at info, per pass, the totals billed and unbilled, with no org text.
- [ ] Run the tests and see them pass. Run lint and format.
- [ ] Commit `feat(core): close evaluation hours into the usage outbox, unbilled while blocked`.

### Task 4: Ship meters, and declare them first

**Model:** sonnet

**Files:**
- `crates/flags-core/src/usage.rs`
- `crates/flags-core/tests/db.rs`
- `crates/flags-server/tests/e2e.rs` (mock: `PUT /internal/meters`)

- [ ] Write failing tests:
  - DB: `ship_once` sends `meter`/`quantity` for a metered row. Use the existing shipper test
    harness or the e2e mock, whichever the current shipper tests use.
  - DB: a rejected metered row keeps `meter`/`quantity` in `usage_outbox_rejected`.
  - e2e: the shipper declares `otto-flags/evaluations` before its first `POST /internal/usage`.
  - e2e: a declaration that answers 409, or rejects the meter, holds shipping. It retries, and
    ships once the declaration succeeds.
- [ ] Run and see them fail.
- [ ] Implement:
  - `Row`, the claim `RETURNING`, the `UsageEvent` mapping (`meter`, `quantity as u64`,
    `service: None`), and the rejected-move CTE all carry `meter`/`quantity`.
  - `declare_meters(platform, cfg, shutdown) -> bool`, ported from otto-factory's
    `of_billing::outbox::declare_meters`:
    - capped exponential backoff on `base_backoff`/`max_backoff`
    - an `error` log once a minute, and on the first failure, with the reason and the number of
      rows waiting
    - an incomplete receipt counts as a failure
    - returns `false` on shutdown
  - `run` calls it first and returns without shipping if shutdown came first.
  - Update the module docs (Metering section).
- [ ] Run the tests and see them pass. Run lint and format.
- [ ] Commit `feat(core): declare the evaluations meter and ship metered usage`.

### Task 5: Free MCP tools, the new usage report, and retiring FLAGS_ENFORCE_QUOTAS

**Model:** sonnet

**Files:**
- `crates/flags-core/src/usage.rs`, `crates/flags-core/src/error.rs`
- `crates/flags-mcp/src/{lib.rs,server.rs,error.rs,tools/org.rs}`, the `out` types file
  `WhoAmI`/`UsageOut` live in
- `crates/flags-server/src/{config.rs,lib.rs,main.rs}`
- tests where these live

- [ ] Write failing tests:
  - `BILLABLE` is empty, and every name in the MCP tool registry is not billable (replaces
    `writes_are_billable_and_reads_are_not`).
  - `Status` mapping:
    - `meters` from `UsageStatus::meters`
    - `budget` with `reason`
    - `blockedReason` from the platform, falling back to `budget_exceeded` when
      `billable_blocked()` is true without a reason
    - `enforced` is always true
  - Config: `FLAGS_ENFORCE_QUOTAS=true` still boots, and `warn_retired` names it.
  - e2e: a write tool records `billable = false`. `whoami` and `usage` include `meters`,
    `budget` and `blockedReason` from the mock (extend the mock with settable `meters`, `budget`
    and `blocked_reason`, and keep the legacy-shape path).
- [ ] Run and see them fail.
- [ ] Implement:
  - `BILLABLE: &[&str] = &[]`.
  - `Meter::charge` only records, with no lookup.
  - Remove `Meter::warm`, `enforce`, the `fail_open` map, `status_for`'s quota use,
    `Error::QuotaExceeded` and its MCP mapping, and `Flags::tx`'s warm call.
  - The new `Status`, `MeterStatus` and `BudgetReport`, mirroring otto-factory's
    `of_billing::Status`. The docs say that blocked means evaluations keep serving and are not
    billed.
  - Remove `enforce_quotas` from the flags-mcp `Config` and the server's `Config`. Keep parsing
    `FLAGS_ENFORCE_QUOTAS` only in a `Config::warn_retired()` that logs a startup warning, and
    drop it from the "listening" log.
  - Text:
    - The tool descriptions for `whoami` and `usage` say every tool is free and billing is on
      production evaluations by the SDKs; read `meters`, `budget` and `blockedReason`.
    - The server INSTRUCTIONS billing paragraph (`server.rs:70-71`) is rewritten the same way.
    - The module doc of `org.rs`.
- [ ] Run the tests and see them pass. Run lint and format.
- [ ] Commit `feat(mcp)!: make every tool free and report meters and budget`, with the
  `BREAKING CHANGE:` footer from the spec.

### Task 6: Wire the counter into the SDK path and run the metering task

**Model:** sonnet

**Files:**
- `crates/flags-api/src/{lib.rs,sdk.rs,telemetry.rs}`
- `crates/flags-core/src/metering.rs` (`run`)
- `crates/flags-server/src/{lib.rs,main.rs}`
- `crates/flags-server/tests/e2e.rs`
- the `crates/flags-api/tests` that build `AppState`

- [ ] Write failing e2e tests:
  - An `/api/evaluate` call plus a telemetry batch, then `metering::flush` and `close_due`, with
    the rows' hour moved into the past by a test UPDATE in a maintenance transaction, or a
    test-only hour parameter. Then `ship_once` yields one metered event per app at the mock, with
    quantity `GREATEST(served, reported)`.
  - Cases: 3 served + 5 reported gives 5; 4 served + 0 reported gives 4.
  - Unknown-flag telemetry and 404 evaluations count nothing.
  - A blocked org (mock `blocked_reason = budget_exceeded`) ships unmetered.
  - With the mock reporting `budget_exceeded`, `/api/evaluate` returns the same answer and the
    mock's usage-status hit count for that org does not move during the request. The test does
    not spawn `metering::run`.
- [ ] Run and see them fail.
- [ ] Implement:
  - `AppState` gains `evaluations: Arc<EvalCounter>`, set through `AppState::new` (update every
    caller).
  - `sdk::evaluate` calls `served(org, app, 1)` just before `Ok`.
  - `telemetry::evaluations` calls `reported(org, app, accepted)` after commit, when
    `accepted > 0`.
  - `metering::run(db, meter, counter, shutdown)`:
    - It flushes every `FLUSH_EVERY` and closes every `CLOSE_EVERY`, using
      `meter.billable_blocked` as the lookup.
    - Failures are logged with the text-free reason and backoff in the shipper's style.
    - On shutdown it does one final flush under a 5 s timeout, then logs the count total if any
      remain.
  - `main.rs`:
    - Build one `Arc<EvalCounter>`, pass it to the router and to `metering::run`, spawned beside
      the shipper.
    - On shutdown, await it after the stop signal, as the shipper is awaited.
    - The `Meter` passed is the same platform-backed meter the MCP side uses, or a new one built
      on the same `PlatformClient`.
- [ ] Run the tests and see them pass. Run lint and format.
- [ ] Commit `feat(api): meter served and reported evaluations from the sdk path`.

### Task 7: Docs

**Model:** haiku

**Files:**
- `CLAUDE.md`
- `docs/deploy/fly.md`
- `.env.example`
- `crates/flags-core/src/usage.rs` module doc, if not already done

- [ ] `CLAUDE.md`:
  - Rewrite the Metering invariant per the spec:
    - MCP tools record free rows in their own transaction.
    - Production evaluations are counted in memory, flushed to `usage_meter_hourly`, and closed
      hourly into `usage_outbox` on `otto-flags/evaluations` as `GREATEST(served, reported)`,
      unmetered while blocked.
    - Reads, dry-run evaluation and MCP tools are never billed.
  - Add a clause to "Evaluations are counted, not stored": `usage_meter_hourly` holds counts
    only.
  - Fix the Development Workflow section only if a gate, CI job or reviewer fact changed. None is
    expected.
- [ ] `docs/deploy/fly.md`:
  - `FLAGS_ENFORCE_QUOTAS` is retired.
  - The meter `otto-flags/evaluations` and the `set-service` prerequisite, which is done.
  - The rollback note: the old shipper ships hour rows unmetered during an overlap or after a
    rollback.
  - Idle suspend delays hour closes: an hour closed after the platform's month cutoff bills in
    the month it arrives.
- [ ] `.env.example`: mark `FLAGS_ENFORCE_QUOTAS` as retired.
- [ ] Run the gates (format and lint at least, plus the full test run if any code comment
  changed).
- [ ] Commit `docs: record evaluation metering and the retired quota switch`.

### Task 8: Final gates and record as shipped

**Model:** n/a (orchestrator)

- [ ] Run the full gate set, including `podman build -t otto-flags:check .` and the extra check.
- [ ] After review and before merge, add a docs commit: the spec's `**Status:**` becomes
  `IMPLEMENTED (#<PR>)` and this plan's boxes are ticked.
