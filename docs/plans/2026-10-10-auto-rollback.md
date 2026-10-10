# Auto-rollback, gated by the plan — Implementation Plan

**Spec:** [`docs/specs/2026-10-09-auto-rollback-design.md`](../specs/2026-10-09-auto-rollback-design.md)
(approved, 2 critique rounds).
**Issue:** [#11](https://github.com/savvagent/otto-flags/issues/11). Platform side done:
savvagent/otto-platform#30 → PR #31, released as **0.5.0 at `fa1ae08c8d5760c1df9752281a078192032e7d24`**
(tag `v0.5.0`), deployed, migration 0016 verified in production on 2026-10-10.
**Branch / worktree:** `feat/auto-rollback`, `.claude/worktrees/feat-auto-rollback`.

## Goal

An agent attaches a rollback policy to one flag in one environment. When server-reported
telemetry shows the flag hurting (the same rule `flag_health` uses to say "Consider rolling
back"), otto-flags disables that environment by itself, as an ordinary flag version that SSE
subscribers see, and records the trip. Only orgs whose plan has `auto_rollback` can create a
policy or have one act. This is the first plan-gated capability and fixes the pattern
(`flags_core::entitlements`) later ones reuse.

## Architecture

```text
SDK  POST /api/telemetry/{evaluations,errors}      (flags-api, hot path, no platform call)
       │ record (reporter = key kind) → armed flags in the same tx → commit
       │ Hints::send(org, armed flags)             (try_send, never blocks, never awaits)
       ▼
flags_core::rollback::Checker  (one tokio task, spawned by flags-server)
       │ Schedule: per-flag 30 s trailing-edge debounce;  backstop every 5 min (≤ 200 due)
       ▼
check_flag(db, entitlements, org, flag)
   tx 1 (begin_live): armed policies + samples → decide → last_checked_at  → commit
   network:           Entitlements::check (2 s timeout, no pooled connection held)
   tx 2 (begin_live): lock flag → re-read policy → re-sample → decide →
                      enforce: apply_env_patch(enabled=false, change="auto_rollback", actor=None)
                               (flag_versions + pg_notify in this tx), tripped_at, event
                      observe / denied / unknown: deduplicated event only
MCP  set/remove/list_rollback_policies, propose_rollback, rollback_events (flags-mcp)
       set_rollback_policy: Entitlements::require before the tx opens (fail closed)
```

`flags-core` owns every statement, the decision rule, the entitlement wrapper, and the
checker. `flags-api` only passes the key kind and sends an in-process hint. `flags-mcp` adds
one tool file. `flags-server` spawns the checker and threads its `Hints` into the API state.

## Tech Stack

Rust stable (MSRV 1.88, edition 2021), axum 0.8, sqlx 0.8 runtime queries on Postgres 16,
rmcp 2.0.0, tokio (mpsc, time, watch); `otto-tenant` / `otto-resource` pinned to otto-platform
`fa1ae08c8d5760c1df9752281a078192032e7d24` (0.5.0). No TypeScript change.

## Global Constraints

- **Gates, run from the worktree root before every commit that touches code:**
  ```bash
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets -- -D warnings
  DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace
  ```
  `DATABASE_URL` points at the local compose Postgres only (port 15434), never at Fly.
- **Migrations are append-only.** The only schema change is the new file
  `crates/flags-core/migrations/0002_auto_rollback.sql`. `0001_baseline.sql` is not touched —
  not even its comments. Check: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/'`
  prints nothing.
- **No SQL outside `flags-core`**, and every statement is a runtime `sqlx::query` /
  `query_as` / `query_scalar` — never a `query!` macro. `crates/flags-core/tests/db.rs` may
  use raw SQL to arrange and inspect state (it already does for the isolation proof).
- **Request paths and the checker open tenant transactions with `platform_events::begin_live`,**
  never `Db::begin` directly. The only unpinned statements are the backstop's due-policy
  `SELECT` and the retention `DELETE`, both via `db.pool()`, both allowed by explicit
  `current_org() IS NULL` policies.
- **The SDK path never calls the platform** and its wire contract does not change: the
  receipt JSON stays `{accepted, unknown_flags}`; `reporter` / `server_count` come from the
  resolved key's `KeyKind`, never from a request field.
- **Client keys see nothing new.** No new SDK route or field.
- **No network call while a pooled connection is held** (entitlement checks happen between
  transactions).
- **Bucketing (`flags_core::eval`) is not touched.**
- **Commits:** conventional (`feat: …`, `build: …`, `test: …`, `docs: …`), lower-case,
  imperative, a why-body, last line `Fixes #11` on the commit that completes the issue and
  `Refs #11` on the others. **No attribution of any kind** — no `Co-Authored-By`, no
  "Generated with", no 🤖, in commits, code comments, docs, or the PR body.

## Plan-level decisions (beyond the spec)

These resolve details the spec leaves open; each is small and called out so review can push
back.

1. **Hints carry only flags with an armed policy.** The spec has the telemetry handler send
   every touched flag id. Instead, the handler asks `rollback::armed(&mut tx, &flags)` — one
   indexed `SELECT` inside the telemetry transaction it already holds, before commit — and
   sends only those. For the common org with no policies this saves the checker a
   `begin_live` transaction (advisory lock + tombstone check) per flag per 30 s on a 5-connection
   pool (Invariant 15). Still no platform call and no second connection on the SDK path.
2. **`rollback_events.outcome` gets a `CHECK`** listing the four outcomes, like `reporter`'s.
   Additive; the Rust `Outcome` enum stays the source of truth.
3. **Re-arming lives in `commit_change`**, not separately in `set_environment` and
   `rollback_flag`: it compares each environment's stored `enabled` with the new one and clears
   `tripped_at` for every environment that went off → on. One definition covers both callers
   (and any future one).
4. **Replacing a policy keeps its `tripped_at`.** Armed/tripped follows the environment's
   switch, not the policy's thresholds; re-arming a policy whose environment is still off would
   only record a no-op trip.
5. **Policy changes are audited** (`flags.rollback_policy.set`, `flags.rollback_policy.removed`)
   through `Tx::audit`, per `audit.rs`: the trail is for what has no history table of its own,
   and a policy has none (`rollback_events` records trips, not edits).
6. **Suppressed-outcome dedup is per (policy, outcome)** within the last hour.
7. **Skipped policies still advance `last_checked_at`** (archived flag, environment removed
   from the app), so the backstop does not re-pick them every pass. They record no event.
8. **The entitlement source is a trait** (`EntitlementSource`), implemented by
   `Entitlements` and by `Entitlement` itself, so `db.rs` can drive `check_flag` with a fixed
   answer and no platform.
9. **Thresholds have one Rust definition** (`Thresholds::default()`); `set_policy` always
   writes every column explicitly. The migration's column defaults mirror it, and a test pins
   that they agree.

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `Cargo.toml` | Pin both otto-platform crates to `fa1ae08…` (0.5.0). |
| **Modify.** `Cargo.lock` | Follows the pin. |
| **Create.** `crates/flags-core/migrations/0002_auto_rollback.sql` | `flag_errors.reporter`, `flag_eval_hourly.server_count`, `rollback_mode`, `rollback_policies`, `rollback_events`, RLS, unpinned checker/retention policies, grants. |
| **Create.** `crates/flags-core/src/rollback.rs` | Decision rule (`decide`), thresholds, policy/event types, `RollbackExt` (policy CRUD, events, sample), `armed`, `rearm`, `check_flag`, `due_policies`, `Schedule`, `Hints`, `Checker`. |
| **Create.** `crates/flags-core/src/entitlements.rs` | `AUTO_ROLLBACK`, `Entitlement`, `EntitlementSource`, `Entitlements` (`check`, `require`). |
| **Modify.** `crates/flags-core/src/lib.rs` | `pub mod entitlements; pub mod rollback;` and the module-doc list. |
| **Modify.** `crates/flags-core/src/error.rs` | `FeatureNotInPlan`, `EntitlementUnavailable` + `code()` / `retriable()` / `is_internal()` arms. |
| **Modify.** `crates/flags-core/src/flags.rs` | `ChangeMeta.actor: Option<UserId>`; `record_version` takes `Option<UserId>`; `pub(crate) apply_env_patch(…, change)`; `pub(crate) lock_by_id`; re-arm in `commit_change`; `FlagVersion.change` doc names `auto_rollback`. |
| **Modify.** `crates/flags-core/src/telemetry.rs` | `record_*` take a `KeyKind` and return `Recorded { receipt, flags }`; `server_count` and `reporter` written; `assess` delegates to `rollback::decide`; `sweep` deletes old `rollback_events`. |
| **Modify.** `crates/flags-core/src/platform_events.rs` | Purge list gains `rollback_events`, `rollback_policies` before `flag_errors`. |
| **Modify.** `crates/flags-core/src/audit.rs` | Two new actions. |
| **Modify.** `crates/flags-core/tests/db.rs` | `meta()` uses `Some(actor)`; new tests (Tasks 2, 3, 5, 6, 8, 10). |
| **Modify.** `crates/flags-api/src/lib.rs` | `AppState.hints: rollback::Hints`; `AppState::new` takes it. |
| **Modify.** `crates/flags-api/src/telemetry.rs` | Pass `sdk.0.kind`; ask `armed` before commit; `hints.send` after commit; return `recorded.receipt`. |
| **Create.** `crates/flags-mcp/src/tools/rollback.rs` | The five tools. |
| **Modify.** `crates/flags-mcp/src/tools/mod.rs` | Module, router, `NAMES`. |
| **Modify.** `crates/flags-mcp/src/tools/out.rs` | `PolicyOut`, `PoliciesOut`, `RemovedPolicyOut`, `ProposalOut`, `EventsOut`. |
| **Modify.** `crates/flags-mcp/src/tools/flags.rs` | `meta()` passes `Some(caller.user_id)`. |
| **Modify.** `crates/flags-mcp/src/server.rs` | `Flags` holds `Entitlements`; `INSTRUCTIONS` mention auto-rollback. |
| **Modify.** `crates/flags-mcp/src/lib.rs` | Build `Entitlements` beside `Meter` in `router`. |
| **Modify.** `crates/flags-mcp/src/error.rs` | Map the two new errors; extend the code/retriable test. |
| **Modify.** `crates/flags-core/src/usage.rs` | `BILLABLE` gains `set_rollback_policy`. |
| **Modify.** `crates/flags-server/src/lib.rs` | `router(…, hints, …)`; `pub fn spawn_checker`. |
| **Modify.** `crates/flags-server/src/main.rs` | Spawn the checker; stop it on shutdown. |
| **Modify.** `crates/flags-server/tests/e2e.rs` | Mock `usage-status` serves per-org `features` (or omits it, or 503s); new e2e tests. |
| **Modify.** `docs/deploy/fly.md`, `README.md`, `docs/SDK-DEVELOPER-GUIDE.md` | Auto-rollback note; tool names; "server-key telemetry drives auto-rollback". |
| **Modify.** `docs/specs/2026-10-09-auto-rollback-design.md` | Status line: planned (Task 0 only). |

## Task Order & Rationale

0 records the plan. 1 (pin) first: everything later compiles against `UsageStatus::features`.
2 (migration) before any SQL that reads the new columns. 3 (reporter) before 5 (sample reads
`reporter`/`server_count`). 4 (pure rule) is independent but must precede 5 and 8. 5 (policy
domain) before 6 (re-arm needs the table and the CRUD to test with). 7 (entitlements) before 8
(checker) and 11 (MCP). 8 (check one flag) before 9 (the loop that calls it). 10 (purge/sweep)
once events can be written. 11 (MCP) and 12 (wiring) before 13 (e2e, which needs both). 14
docs last.

---

### Task 0: Record the plan

**Files:** `docs/plans/2026-10-10-auto-rollback.md` (this file), `docs/specs/2026-10-09-auto-rollback-design.md`.

- [ ] Set the spec's status line to `**Status:** Approved (spec critique, 2 rounds), 2026-10-09. Planned: [the plan](../plans/2026-10-10-auto-rollback.md).`
- [ ] Commit: `docs: plan auto-rollback` — body: the plan's task order and the nine plan-level decisions in one paragraph; `Refs #11`.

### Task 1: Pin otto-platform 0.5.0

**Files:** Modify `Cargo.toml`, `Cargo.lock`, `crates/flags-server/tests/e2e.rs`.
**Interfaces:** produces `otto_resource::UsageStatus::{features, feature_enabled}` for Tasks 7, 13.

- [ ] In `Cargo.toml`, set both `otto-tenant` and `otto-resource` to
      `rev = "fa1ae08c8d5760c1df9752281a078192032e7d24"` (one rev for both; leave the comment).
- [ ] `cargo update -p otto-tenant -p otto-resource` then `cargo build --workspace --all-targets`.
      Expect exactly one failure: the `UsageStatus { … }` literal in `e2e.rs`'s
      `usage_status` mock lacks `features`.
- [ ] Add `features: Default::default(),` to that literal (Task 13 replaces the mock).
- [ ] Run the three gates. All existing tests pass.
- [ ] Confirm the diff between the old and new rev touches only `otto-resource/src/types.rs`
      (`gh api repos/savvagent/otto-platform/compare/a5d169f…fa1ae08 --jq '.files[].filename'`)
      and say so in the commit body.
- [ ] Commit: `build: pin otto-platform 0.5.0 for plan features` — why: auto-rollback reads
      `UsageStatus::features`; 0.5.0 is released and deployed; the only API change is the
      additive `features` field. `Refs #11`.

### Task 2: Migration `0002_auto_rollback.sql`

**Files:** Create `crates/flags-core/migrations/0002_auto_rollback.sql`. Modify `crates/flags-core/tests/db.rs`.
**Interfaces:** produces the tables and columns every later task reads.

- [ ] **Failing test first.** In `db.rs` add
      `#[sqlx::test(migrator = "flags_core::MIGRATOR")] async fn auto_rollback_tables_are_tenant_isolated(pool: PgPool)`:
      create an app + flag in org A (existing helpers), then with raw SQL in a `db.begin(a)`
      transaction `INSERT INTO rollback_policies (org_id, app_id, flag_id, environment) … RETURNING id`
      and one `rollback_events` row (`outcome 'would_roll_back'`, `observed '{}'`). Assert:
      org B (`db.begin(b)`) sees 0 rows in both tables; an unpinned
      `SELECT count(*) FROM rollback_policies` on `db.pool()` sees 1; inserting in org B a
      `rollback_policies` row naming org A's `flag_id` fails (composite FK); an
      `outcome 'bogus'` insert fails (CHECK); `flag_errors.reporter = 'browser'` fails (CHECK).
      (The column-defaults test needs `Thresholds`, so it lands in Task 4.)
- [ ] Run: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-core --test db auto_rollback_tables_are_tenant_isolated` → fails (relation does not exist).
- [ ] Write `0002_auto_rollback.sql` exactly as spec §3, plus:
  - `outcome text NOT NULL CHECK (outcome IN ('rolled_back', 'would_roll_back', 'not_entitled', 'entitlement_unknown'))` (decision 2);
  - a leading comment block: what the migration adds, that `flag_versions.change` gains the
    value `auto_rollback` (the baseline's comment listing change values cannot be edited), and
    the rolling-deploy compatibility paragraph from spec §3 in two sentences.
  - Keep the spec's index set, RLS (`ENABLE` + `FORCE` + `<table>_tenant_isolation`),
    `rollback_policies_checker` (`FOR SELECT USING (current_org() IS NULL)`),
    `rollback_events_retention` (`FOR DELETE USING (current_org() IS NULL)`), and the guarded
    `GRANT` block (tables + `rollback_events_id_seq`).
- [ ] Run the new test → passes. Run
      `… cargo test -p flags-core --test db tenant_isolation_is_enforced_on_every_tenant_table` → passes
      and its summary lists `rollback_policies` and `rollback_events` (print it with
      `-- --nocapture` once and check).
- [ ] Run the three gates and the append-only check (prints nothing).
- [ ] Commit: `feat: add the auto-rollback schema` — why: policies and events per flag and
      environment, tenant-isolated with composite FKs; the reporter columns let the checker
      ignore public-key telemetry; every change additive so the old release keeps working
      during the rollout. `Refs #11`.

### Task 3: Record which kind of key reported telemetry

**Files:** Modify `crates/flags-core/src/telemetry.rs`, `crates/flags-api/src/telemetry.rs`, `crates/flags-core/tests/db.rs`.
**Interfaces:**
```rust
// flags_core::telemetry
pub struct Recorded { pub receipt: Receipt, /// Flags this batch counted against, deduplicated.
                      pub flags: Vec<FlagId> }
pub async fn record_evaluations(tx: &mut Tx<'_>, app: FlagAppId, reporter: KeyKind, events: Vec<EvalEvent>) -> Result<Recorded>;
pub async fn record_errors(tx: &mut Tx<'_>, app: FlagAppId, reporter: KeyKind, events: Vec<ErrorEvent>) -> Result<Recorded>;
```

- [ ] **Failing test first.** Update the existing `telemetry_rolls_up_and_feeds_health` call
      sites to the new signatures (pass `KeyKind::Server`). Add
      `async fn telemetry_records_the_reporting_key_kind`: record 3 evaluations with
      `KeyKind::Server` and 2 with `KeyKind::Client` for the same (flag, env, hour, enabled,
      variation); assert the bucket's `count = 5` and `server_count = 3` (raw SQL in a pinned
      tx). Record one error per kind; assert `reporter` is `'server'` / `'client'`. Assert
      `Recorded.flags` lists the touched flag once and excludes unknown keys.
- [ ] Run `… cargo test -p flags-core --test db telemetry_` → compile failure.
- [ ] Implement: thread `reporter` through; evaluations bind an extra `$8::bigint[]`
      (`server_count` per bucket = the bucket's count if `reporter == Server` else 0) and the
      upsert adds `server_count = flag_eval_hourly.server_count + EXCLUDED.server_count`;
      errors bind `reporter.as_str()`. Collect touched flag ids (dedup, sorted).
- [ ] `flags-api/src/telemetry.rs`: pass `sdk.0.kind`, return `Json(recorded.receipt)`. Wire
      response unchanged.
- [ ] Run the db tests, then the three gates.
- [ ] Commit: `feat: record whether a server or client key reported telemetry` — why: the
      client key is public, so auto-rollback must be able to judge on server-reported
      telemetry only; derived from the resolved key, not the request. `Refs #11`.

### Task 4: The decision rule, shared with `flag_health`

**Files:** Create `crates/flags-core/src/rollback.rs` (pure part only). Modify `crates/flags-core/src/lib.rs`, `crates/flags-core/src/telemetry.rs`.
**Interfaces:**
```rust
pub struct Thresholds { pub min_evaluations: i64, pub min_errors: i64, pub max_error_ratio: f64,
                        pub max_error_rate: Option<f64>, pub window_hours: i32,
                        pub include_client_reports: bool }      // Default = 100, 10, 2.0, None, 1, false
pub struct Sample { pub evals_on: i64, pub evals_off: i64, pub errors_on: i64, pub errors_off: i64 }
impl Sample { pub fn rate_on(&self) -> Option<f64>; pub fn rate_off(&self) -> Option<f64>; }
pub enum Trigger { MaxErrorRate, ErrorRatio, OffHasNoErrors }
pub enum Decision { BelowMinimums, Healthy, Trip(Trigger) }
pub fn decide(sample: &Sample, t: &Thresholds) -> Decision;
/// false when there is no off traffic and no max_error_rate: only max_error_rate can trip then.
pub fn can_trip(sample: &Sample, t: &Thresholds) -> bool;
impl Thresholds { pub fn validate(&self) -> Result<()>; }   // mirrors the migration's CHECKs, Error::Invalid
```
All derive `Debug, Clone, PartialEq, Serialize, schemars::JsonSchema` (camelCase) so Task 11
can return them.

- [ ] **Failing tests first** (unit, in `rollback.rs`): below `min_evaluations` →
      `BelowMinimums`; below `min_errors` → `BelowMinimums`; ratio trip (1 000/1 000 evals,
      50/5 errors); exactly 2.0x does **not** trip (strict `>`); zero-off-errors trip;
      no off traffic without `max_error_rate` → `Healthy` and `can_trip == false`; with
      `max_error_rate = 0.01` and rate 0.05 → `Trip(MaxErrorRate)`; `validate` refuses
      `max_error_ratio <= 1.0`, `window_hours = 25`, `max_error_rate = 0`.
      Plus an agreement test: for every `(Counts, Counts)` pair in `telemetry`'s existing
      tests and the cases above, `assess` says "Consider rolling back" **iff**
      `decide(…, &Thresholds::default())` is `Trip(_)`.
- [ ] Run `cargo test -p flags-core --lib rollback` → fails (module missing).
- [ ] Implement `decide` exactly as spec §4 "Decision rule". Refactor `telemetry::assess` to
      build a `Sample` from its `Counts` and match on `decide(&sample, &Thresholds::default())`
      for its two "Consider rolling back" branches; keep its "No evaluations", "too few", "No
      sign", and "Not enough data" wording and its existing tests unchanged.
- [ ] Add `policy_column_defaults_match_the_rust_thresholds` to `db.rs`: insert a policy with
      only the required columns (raw SQL, pinned) and compare its threshold columns with
      `Thresholds::default()`.
- [ ] Run `cargo test -p flags-core`, then the three gates.
- [ ] Commit: `feat: share one rollback decision rule with flag_health` — why: the advice an
      agent reads and the action the checker takes must not disagree. `Refs #11`.

### Task 5: Policies, events, and samples in `flags-core`

**Files:** Modify `crates/flags-core/src/rollback.rs`, `crates/flags-core/src/audit.rs`, `crates/flags-core/tests/db.rs`.
**Interfaces:**
```rust
#[sqlx(type_name = "rollback_mode", rename_all = "lowercase")] pub enum Mode { Enforce, Observe }
pub struct Policy { id: Uuid, org_id, app_id: FlagAppId, flag_id: FlagId, environment: String, mode: Mode,
                    #[sqlx(flatten)]-or-explicit thresholds…, tripped_at, last_checked_at,
                    created_by: Option<UserId>, created_at, updated_at }   // FromRow
pub enum PolicyState { Armed, Tripped, EnvironmentMissing }
pub struct PolicyView { #[serde(flatten)] pub policy: Policy, pub flag_key: String, pub state: PolicyState }
pub enum Outcome { RolledBack, WouldRollBack, NotEntitled, EntitlementUnknown }  // snake_case text
pub struct Observation { pub window_start: DateTime<Utc>, pub sample: Sample, pub rate_on: Option<f64>,
                         pub rate_off: Option<f64>, pub thresholds: Thresholds, pub decision: Decision }
pub struct Event { id: i64, policy_id: Uuid, flag_id: FlagId, flag_key: String, environment: String,
                   outcome: Outcome, observed: serde_json::Value, flag_version: Option<i32>, created_at }
pub trait RollbackExt {   // impl for Tx<'_>, same shape as FlagsExt
  fn set_policy(&mut self, app: &FlagApp, flag: &FeatureFlag, environment: &str, mode: Mode,
                t: Thresholds, actor: UserId) -> impl Future<Output = Result<PolicyView>> + Send;
  fn remove_policy(&mut self, app: &FlagApp, flag: &FeatureFlag, environment: &str, actor: UserId)
                -> impl Future<Output = Result<bool>> + Send;
  fn list_policies(&mut self, app: &FlagApp, flag: Option<&FeatureFlag>) -> … Result<Vec<PolicyView>>;
  fn find_policy(&mut self, flag: FlagId, environment: &str) -> … Result<Option<Policy>>;
  fn rollback_events(&mut self, app: &FlagApp, flag: Option<&FeatureFlag>, limit: i64) -> … Result<Vec<Event>>;
  fn sample(&mut self, flag: FlagId, environment: &str, t: &Thresholds) -> … Result<(DateTime<Utc>, Sample)>;
}
/// Of `flags`, those with an armed policy. Used inside the telemetry transaction (decision 1).
pub async fn armed(tx: &mut Tx<'_>, flags: &[FlagId]) -> Result<Vec<FlagId>>;
```
Policy ids stay plain `Uuid` (no new newtype): nothing else takes one, and an `id` from
`list_rollback_policies` is only ever read back by a human or agent.

- [ ] **Failing tests first** (`db.rs`):
  - `rollback_policies_are_org_isolated_and_replaced_in_place`: `set_policy` validates the
    environment against the app (unknown env → `invalid_argument` naming the app's envs) and
    the thresholds (`validate`); a second `set_policy` for the same (flag, env) replaces mode
    and thresholds, keeps `id`, `created_by`, and `tripped_at` (decision 4); `list_policies`
    in org B is empty; `remove_policy` returns `true` then `false`; one audit row per set and
    per remove (`audit_events` action names).
  - `list_policies_reports_tripped_and_missing_environments`: set `tripped_at` by raw SQL →
    `Tripped`; remove the env from the app with `update_app` → `EnvironmentMissing`.
  - `a_default_policy_is_judged_on_server_reports_only`: record client-key evaluations and
    errors that would trip, `sample` with defaults → all zeros; same with
    `include_client_reports = true` → counts the client rows; server-key rows always count.
  - `null_environment_errors_count_for_production_only`: errors with `environment: None`
    count toward `sample(…, "production", …)` and not `"staging"`.
  - `the_sample_window_starts_at_the_top_of_the_hour`: an evaluation bucket for the previous
    hour is excluded at `window_hours = 1` and included at `2`; `window_start` equals
    `date_trunc('hour', now())` for 1.
  - `armed_lists_only_flags_with_an_untripped_policy`.
  - `rollback_events_are_newest_first_and_capped`: insert events by raw SQL; `limit` clamps to
    1..=200.
- [ ] Run `… cargo test -p flags-core --test db rollback` → compile failure.
- [ ] Implement. `set_policy` is `INSERT … ON CONFLICT (flag_id, environment) DO UPDATE SET
      mode, thresholds…, updated_at = now()` writing every threshold column explicitly
      (decision 9), then `self.audit(Entry::new(action::ROLLBACK_POLICY_SET).target("flag",
      flag.id.to_string()) …)` following `apps.rs`'s audit calls. `sample` is two queries
      (evaluations and errors) with `since` computed in SQL as
      `date_trunc('hour', now()) - make_interval(hours => $n - 1)` and the source/environment
      filters from spec §4 "Sample". Every statement names `org_id = $1` as well as relying on
      RLS, like the existing modules. Add `ROLLBACK_POLICY_SET = "flags.rollback_policy.set"`
      and `ROLLBACK_POLICY_REMOVED = "flags.rollback_policy.removed"` to `audit::action`.
- [ ] Run the db tests, then the three gates.
- [ ] Commit: `feat: store rollback policies and read their samples` — why: one policy per
      flag and environment; samples count server reports unless the policy opts in, and treat
      an unnamed error environment as production. `Refs #11`.

### Task 6: System-actor changes and re-arming

**Files:** Modify `crates/flags-core/src/flags.rs`, `crates/flags-mcp/src/tools/flags.rs`, `crates/flags-core/tests/db.rs`.
**Interfaces:**
```rust
pub struct ChangeMeta { pub actor: Option<UserId>, pub reason: Option<String>, pub expected_version: Option<i32> }
/// set_environment's body with the change name as a parameter. `set_environment` = this with "set_environment".
pub(crate) async fn apply_env_patch(tx: &mut Tx<'_>, app: &FlagApp, key: &str, environment: &str,
                                    patch: EnvPatch, meta: &ChangeMeta, change: &str) -> Result<FeatureFlag>;
/// Lock a flag by id (the checker has no app/key in hand). None if it no longer exists.
pub(crate) async fn lock_by_id(tx: &mut Tx<'_>, flag: FlagId) -> Result<Option<FeatureFlag>>;
// rollback.rs
pub(crate) async fn rearm(tx: &mut Tx<'_>, flag: FlagId, before: &Value, after: &Value) -> Result<()>;
```

- [ ] **Failing tests first** (`db.rs`): change the `meta()` helper to `actor: Some(actor)`
      and the three inline `ChangeMeta { actor, … }` literals to `Some(actor)`. Add
      `turning_an_environment_back_on_rearms_its_policy`: set a policy on production, set
      `tripped_at` by raw SQL and disable production with `set_environment`; enabling
      **staging** leaves production's policy tripped; enabling production clears it; trip it
      again, then `rollback_flag` to a version where production was on → cleared;
      `update_flag` (no `enabled` change) leaves a tripped policy tripped.
- [ ] Run `… cargo test -p flags-core --test db rearms` → fails.
- [ ] Implement: `actor: Option<UserId>` through `record_version` (`create_flag` passes
      `Some(actor)`); move `set_environment`'s body into `apply_env_patch`; in `commit_change`
      read the locked row's stored `environments` before the `UPDATE`
      (`SELECT environments FROM feature_flags WHERE org_id = $1 AND id = $2` — the row is
      already locked by the caller) and call `rollback::rearm` after `record_version`;
      `rearm` collects environments whose `enabled` went `false/absent → true` and runs one
      `UPDATE rollback_policies SET tripped_at = NULL, updated_at = now() WHERE org_id = $1 AND
      flag_id = $2 AND environment = ANY($3) AND tripped_at IS NOT NULL` (skip when empty).
      Update `FlagVersion.change`'s doc comment to list `auto_rollback` and say
      `actor_user_id` is `None` for it.
- [ ] `flags-mcp/src/tools/flags.rs`: `meta()` sets `actor: Some(caller.user_id)`.
- [ ] Run the db tests, then the three gates.
- [ ] Commit: `feat: let a flag change come from the system and re-arm on re-enable` — why:
      the checker has no user; a tripped policy should stay quiet until someone turns the
      environment back on, then guard it again. `Refs #11`.

### Task 7: Entitlements

**Files:** Create `crates/flags-core/src/entitlements.rs`. Modify `crates/flags-core/src/lib.rs`, `crates/flags-core/src/error.rs`, `crates/flags-mcp/src/error.rs`.
**Interfaces:**
```rust
pub const AUTO_ROLLBACK: &str = "auto_rollback";
#[derive(Debug, Clone, PartialEq, Eq)] pub enum Entitlement { Granted, Denied { plan: String }, Unknown }
pub trait EntitlementSource: Send + Sync {
    fn check(&self, org: OrgId, feature: &str) -> impl Future<Output = Entitlement> + Send;
}
impl EntitlementSource for Entitlement { /* returns self.clone(): fixed answer for tests */ }
#[derive(Clone)] pub struct Entitlements { platform: Arc<PlatformClient>, pub upgrade_url: String }
impl Entitlements {
    pub fn new(platform: Arc<PlatformClient>, upgrade_url: impl Into<String>) -> Self;
    /// Ok, or FeatureNotInPlan / EntitlementUnavailable. For MCP tools, before a tx opens.
    pub async fn require(&self, org: OrgId, feature: &str) -> Result<()>;
}
impl EntitlementSource for Entitlements { /* 2 s timeout around platform.usage_status; Ok → from_status; Err/timeout → Unknown (logged warn) */ }
pub(crate) fn from_status(status: &UsageStatus, feature: &str) -> Entitlement;
// error.rs
FeatureNotInPlan { feature: String, plan: String, upgrade_url: String }  // "feature_not_in_plan", not retriable
EntitlementUnavailable { feature: String }                                // "platform_unavailable", retriable
```

- [ ] **Failing tests first.** Unit tests in `entitlements.rs`: `from_status` with
      `{"auto_rollback": true}` → `Granted`; `{}` / `false` / `"yes"` → `Denied { plan }`
      (plan as `usage::plan_name` capitalises it — make `plan_name` `pub(crate)` and reuse).
      In `flags-mcp/src/error.rs`'s `every_error_carries_a_code_and_a_retriable_flag`, add
      both new variants; add a test that `FeatureNotInPlan`'s message contains the upgrade
      URL and the plan, and `EntitlementUnavailable` is `retriable: true`.
- [ ] Run `cargo test -p flags-core --lib entitlements && cargo test -p flags-mcp --lib error` → fails.
- [ ] Implement. Messages (agent-readable, per `error.rs`'s rule):
      `FeatureNotInPlan` → `"{feature} is not included in this organization's {plan} plan, so
      this was refused. Upgrade at {upgrade_url}"`; `EntitlementUnavailable` → `"the otto
      platform could not confirm that this organization's plan includes {feature}; nothing was
      changed. Retry shortly."`. `is_internal()` is false for both (their text is ours, never
      the platform's). In `flags-mcp/src/error.rs`, `FeatureNotInPlan` joins the
      `INVALID_REQUEST` arm; `EntitlementUnavailable` maps to `ErrorCode::INTERNAL_ERROR`.
      `Entitlements` never reads `Meter::enforce` and has no fail-open window (spec §2).
      Module doc: never called on the SDK path (Invariant 6).
- [ ] Run the tests, then the three gates.
- [ ] Commit: `feat: check plan features at the platform, failing closed` — why: the first
      plan-gated capability; an unknown answer must not grant a paid feature. `Refs #11`.

### Task 8: Checking one flag (`check_flag`)

**Files:** Modify `crates/flags-core/src/rollback.rs`, `crates/flags-core/tests/db.rs`.
**Interfaces:**
```rust
pub struct CheckReport { pub checked: usize, pub outcomes: Vec<(Uuid /*policy*/, Outcome)> }
pub async fn check_flag(db: &Db, entitlements: &impl EntitlementSource, org: OrgId, flag: FlagId) -> Result<CheckReport>;
/// Unpinned: armed policies not checked for `older_than`, oldest first, at most `limit`, as (org, flag) pairs.
pub async fn due_policies(db: &Db, older_than: Duration, limit: i64) -> Result<Vec<(OrgId, FlagId)>>;
```
Follows spec §4 "Acting on a trip" step for step (two `begin_live(db, org, None)`
transactions, the entitlement check between them with no connection held, re-lock with
`lock_by_id`, re-read the policy, re-sample, re-decide). The rollback itself is
`apply_env_patch(tx, &app, &flag.key, &env, EnvPatch { enabled: Some(false), ..Default::default() },
&ChangeMeta { actor: None, reason: Some(reason), expected_version: None }, "auto_rollback")`
with `reason = format!("auto-rollback by policy {id}: error rate on {on:.2}% vs off {off} over {n} evaluations")`
where `off` is `"{:.2}%"` or `"n/a"` when there was no off traffic — numbers only, never
reported text (Invariant 12). If the environment is already off: set `tripped_at`, insert
`rolled_back` with `flag_version = NULL`, no new version. Archived flag or environment no
longer in the app: skip, no event, still advance `last_checked_at` (decision 7). Suppressed
outcomes are deduplicated per (policy, outcome) within an hour (decision 6). `observed` is
`serde_json::to_value(&Observation)`.

- [ ] **Failing tests first** (`db.rs`; arrange telemetry with `record_*` and
      `KeyKind::Server`; pass a fixed `Entitlement`):
  - `a_tripping_policy_disables_only_its_environment`: production and staging both on,
    policy on production, telemetry that trips → one new version, `change = 'auto_rollback'`,
    `actor_user_id IS NULL`, `reason` starts `auto-rollback by policy`; production off,
    staging still on; `tripped_at` set; one `rolled_back` event with that version; a
    `LISTEN flag_changes` on a separate `PgListener` (from the test pool) receives the
    notification after commit.
  - `a_tripped_policy_does_not_act_again`: second `check_flag` → no new version, no event.
  - `observe_mode_records_once_an_hour_and_changes_nothing`: two checks → one
    `would_roll_back`, flag unchanged, `tripped_at` NULL.
  - `an_unentitled_org_records_but_does_not_act` (`Denied`) and
    `an_unknown_entitlement_fails_closed` (`Unknown`): one deduplicated event each, flag
    unchanged.
  - `a_healthy_flag_only_advances_last_checked`.
  - `an_already_disabled_environment_is_marked_tripped_without_a_version`.
  - `archived_flags_and_missing_environments_are_skipped`.
  - `due_policies_sees_every_org_unpinned`: policies in two orgs, both due; after
    `check_flag` on one, only the other is due; tripped policies are never due.
  - `a_deleted_org_is_not_checked`: tombstone the org via `platform_events::apply`
    (`org.deleted`) → `check_flag` returns `access_revoked` (the caller drops it).
- [ ] Run `… cargo test -p flags-core --test db -- policy tripp entitle due_policies` → fails.
- [ ] Implement.
- [ ] Run the db tests, then the three gates.
- [ ] Commit: `feat: turn a flag off when its rollback policy trips` — why: the automatic
      action is the per-environment kill switch, written as an ordinary version so history,
      SSE, and rollback_flag all see it; the platform is asked between transactions so no
      connection waits on it. `Refs #11`.

### Task 9: The checker loop and its hints

**Files:** Modify `crates/flags-core/src/rollback.rs`.
**Interfaces:**
```rust
#[derive(Clone)] pub struct Hints { tx: mpsc::Sender<Hint>, dropped: Arc<AtomicU64> }
impl Hints {
    pub fn send(&self, org: OrgId, flags: Vec<FlagId>);   // no-op if empty; try_send; Full → dropped += 1; Closed → ignore
    pub fn detached() -> Self;                             // receiver dropped (tests, and routers built without a checker)
}
pub struct CheckerConfig { pub debounce: Duration /*30 s*/, pub backstop_every: Duration /*5 min*/,
                           pub backstop_limit: i64 /*200*/, pub channel_capacity: usize /*1024*/ }  // Default
pub struct Checker;  impl Checker {
    /// The Hints to hand to flags-api, and the task to await on shutdown.
    pub fn spawn<E: EntitlementSource + 'static>(db: Db, entitlements: E, config: CheckerConfig,
                 shutdown: watch::Receiver<bool>) -> (Hints, JoinHandle<()>);
}
/// Pure scheduling state, unit-tested with synthetic Instants.
pub(crate) struct Schedule { … }
impl Schedule { fn hint(&mut self, key: (OrgId, FlagId), now: Instant);  fn next_due(&self) -> Option<Instant>;
                fn take_due(&mut self, now: Instant) -> Vec<(OrgId, FlagId)>;  fn checked(&mut self, key, now: Instant);
                fn prune(&mut self, now: Instant); }
```

- [ ] **Failing tests first** (unit, `rollback.rs`): `Schedule` — a first hint is due now; a
      hint for a flag checked 10 s ago is due 20 s later (trailing edge), never dropped; many
      hints for one pending flag stay one entry with the earliest due time; `prune` drops
      `checked` entries older than the debounce. `Hints` — `send` on a full channel bumps
      `dropped` and returns immediately; on a detached one it does nothing; empty `flags`
      sends nothing.
- [ ] Run `cargo test -p flags-core --lib rollback::` → fails.
- [ ] Implement the loop: `tokio::select!` over `rx.recv()` (→ `schedule.hint`), a sleep until
      `schedule.next_due()`, a `backstop_every` interval (→ `due_policies` → `hint` each, so
      the backstop goes through the same debounce), a 60 s interval that logs and resets
      `dropped` when non-zero, and `shutdown.changed()` (→ return). Due flags are checked
      **sequentially** with `check_flag`; an `Err` is logged at `warn` with org and flag (never
      the error's database text at info) and the flag is still marked `checked`. Module doc
      explains: why telemetry drives it (the machine suspends; spec premise 3), the 30 s
      debounce, the backstop, and that it holds at most one pooled connection at a time.
- [ ] Run the tests, then the three gates.
- [ ] Commit: `feat: run rollback checks when telemetry arrives` — why: a suspended machine
      runs no timers, and telemetry is what wakes it; hints are debounced per flag and backed
      by a periodic pass while awake. `Refs #11`.

### Task 10: Retention and org deletion

**Files:** Modify `crates/flags-core/src/telemetry.rs`, `crates/flags-core/src/platform_events.rs`, `crates/flags-core/tests/db.rs`.

- [ ] **Failing tests first.** Extend `org_deleted_purges_everything_and_revokes_keys` to set a
      policy and an event first, then assert both tables are empty for the org and the
      `Applied` detail names `rollback_policies` and `rollback_events`. Add
      `old_rollback_events_are_swept`: an event with `created_at = now() - 91 days` (raw SQL)
      is deleted by `telemetry::sweep`, a 1-day-old one is kept.
- [ ] Run `… cargo test -p flags-core --test db -- org_deleted old_rollback_events` → fails.
- [ ] Implement: add `"rollback_events", "rollback_policies"` at the front of the purge list
      (children before parents, before `flag_errors`); `sweep` adds
      `DELETE FROM rollback_events WHERE created_at < now() - make_interval(days => $1)` with
      `EVAL_RETENTION_DAYS`, summed into its return.
- [ ] Run the tests, then the three gates.
- [ ] Commit: `feat: purge and expire rollback data` — why: org.deleted must leave nothing
      behind; events are bounded at the evaluation retention. `Refs #11`.

### Task 11: MCP tools

**Files:** Create `crates/flags-mcp/src/tools/rollback.rs`. Modify `crates/flags-mcp/src/tools/{mod.rs,out.rs}`, `crates/flags-mcp/src/server.rs`, `crates/flags-mcp/src/lib.rs`, `crates/flags-core/src/usage.rs`.
**Interfaces:** `Flags::new(db, platform, meter, entitlements: Entitlements)`;
`flags_mcp::router` builds `Entitlements::new(config.platform.clone(), config.upgrade_url.clone())`
next to `Meter`. Tool table as spec §5. Args (camelCase, documented per field like the
existing tools):
- `SetPolicyArgs { app, key, environment, mode: Option<Mode>, min_evaluations: Option<i64>, min_errors: Option<i64>, max_error_ratio: Option<f64>, max_error_rate: Option<f64>, window_hours: Option<i32>, include_client_reports: Option<bool> }` — omitted → `Thresholds::default()` field.
- `PolicyRefArgs { app, key, environment }` (remove).
- `ListPoliciesArgs { app, key: Option<String> }`.
- `ProposeArgs { app, key, environment, …the same optional threshold fields… }` — base is the
  existing policy for (flag, environment) if any, else defaults; given fields override.
- `EventsArgs { app, key: Option<String>, limit: Option<i64> }` (default 50, max 200).

Outputs (`out.rs`): `PolicyOut { policy: PolicyView, warning: Option<String> }`,
`PoliciesOut { app, policies }`, `RemovedPolicyOut { removed: bool }`,
`ProposalOut { key, environment, policy_id: Option<Uuid>, observation: Observation, would_trip: bool, can_trip: bool, note: String }`,
`EventsOut { events: Vec<Event> }`. No reported text in any of them.

- [ ] **Failing tests first.** `tools/mod.rs`'s
      `every_named_tool_is_routed_and_metered_consistently` with the five names added to
      `NAMES` and `set_rollback_policy` to `usage::BILLABLE` → fails until routed. In
      `server.rs`, add `set_rollback_policy` and `propose_rollback` to
      `the_instructions_name_the_opening_moves`.
- [ ] Run `cargo test -p flags-mcp --lib` → fails.
- [ ] Implement each tool in the existing shape: `caller` → `require_scope` → (for
      `set_rollback_policy` only: `self.entitlements.require(caller.org_id, AUTO_ROLLBACK).await.mcp()?`
      **before** `self.tx`) → `tx` → `charge` → domain call → commit. `set_rollback_policy`
      returns `warning: Some("this policy can trip only on max_error_rate while the flag has no
      off traffic; set max_error_rate to guard a flag at 100%")` whenever `max_error_rate` is
      unset. `propose_rollback` sets `note` from `decide` / `can_trip` in one sentence.
      Descriptions say: plan-gated (set), free (others), "changes nothing" (propose), and that
      events hold numbers only. Add one paragraph to `INSTRUCTIONS` after the rollback bullet:
      auto-rollback exists on paid plans, `set_rollback_policy` arms it per environment,
      `propose_rollback` shows what it would do now, `rollback_events` shows what it did.
- [ ] Run `cargo test -p flags-mcp`, then the three gates.
- [ ] Commit: `feat: manage rollback policies over MCP` — why: agents set a policy once per
      flag and environment; creating one is plan-gated and fails closed, removing one never is.
      `Refs #11`.

### Task 12: Wire the checker into the server and the SDK API

**Files:** Modify `crates/flags-api/src/{lib.rs,telemetry.rs}`, `crates/flags-server/src/{lib.rs,main.rs}`, `crates/flags-server/tests/e2e.rs` (call sites only).
**Interfaces:**
```rust
// flags-api
impl AppState { pub fn new(db: Db, listener: Listener, hints: Hints, platform_webhook_secret: impl Into<String>) -> Self; }
// flags-server
pub fn router(db: Db, platform: Arc<PlatformClient>, listener: Listener, hints: Hints, config: &Config) -> Router;
pub fn spawn_checker(db: Db, platform: Arc<PlatformClient>, config: &Config, checker: CheckerConfig,
                     shutdown: watch::Receiver<bool>) -> (Hints, JoinHandle<()>);  // Entitlements with mcp_config's upgrade_url
```

- [ ] **Failing test first.** In `flags-api/src/telemetry.rs` keep the existing unit tests;
      the behavior test is Task 13's e2e. Update every `router(…)` / `AppState::new(…)` call
      site (`flags-server/src/lib.rs` tests and `e2e.rs`'s `server()`) to pass
      `Hints::detached()`; `cargo build --workspace --all-targets` must fail before the
      signatures change and pass after.
- [ ] Implement: in both handlers, after `record_*` and **before** `tx.commit()`,
      `let armed = rollback::armed(&mut tx, &recorded.flags).await?;`; after a successful
      commit, `state.hints.send(sdk.0.org_id, armed);`. Nothing else on the SDK path changes.
      `main.rs`: `let (hints, checker) = spawn_checker(db.clone(), platform.clone(), &config,
      CheckerConfig::default(), shutdown.clone());` before building the router; on shutdown
      `stop.send(true)` already fires — await `checker` with a 5 s timeout, else abort, and log.
      Update `main.rs`'s module doc (background tasks list) and `flags-api/src/lib.rs`'s
      (telemetry hints the checker; still no platform call).
- [ ] Run the three gates.
- [ ] Commit: `feat: start the rollback checker and feed it from telemetry` — why: the SDK
      path only sends an in-process hint after commit, so evaluation never waits on the checker
      or the platform. `Refs #11`.

### Task 13: End to end

**Files:** Modify `crates/flags-server/tests/e2e.rs`.

- [ ] Replace the mock's `usage_status` with a stateful one: `Platform` gains
      `entitled: HashSet<Uuid>`, `legacy: HashSet<Uuid>` (answer without a `features` key, as
      a pre-0.5.0 platform), and `unavailable: HashSet<Uuid>` (503). It returns
      `Json<Value>` built by hand so the `features` key can be absent.
- [ ] `server()` gains a variant (or parameter) that builds the router with a real
      `Listener::spawn(pool.clone(), shutdown)` and `spawn_checker(…, CheckerConfig { debounce:
      Duration::from_millis(50), backstop_every: Duration::from_secs(3600), ..Default::default() }, …)`.
- [ ] `auto_rollback_needs_a_plan_that_includes_it`: org not entitled →
      `set_rollback_policy` errors with `data.code == "feature_not_in_plan"` and the message
      names the upgrade URL; `legacy` org → same; `unavailable` org →
      `platform_unavailable`, `retriable: true`; in each case `list_rollback_policies` is empty.
      An entitled org then sets one and, after being moved out of `entitled` (fresh org, to
      avoid the 60 s cache), `remove_rollback_policy` still succeeds for a non-entitled org
      whose policy was inserted while entitled — use two orgs if the cache gets in the way.
- [ ] `a_policy_turns_a_failing_flag_off_and_tells_the_stream`: entitled org; create app +
      flag; enable production and staging; `set_rollback_policy` with `minEvaluations: 20,
      minErrors: 5`; subscribe to the listener; post server-key evaluations (20 on, 20 off) and
      6 errors with `flag_enabled: true` and **no** `environment`; poll `get_flag` (≤ 5 s,
      50 ms steps) until production is off; assert staging still on, the `FlagChange` arrived
      on the subscription, `flag_history`'s newest entry is `auto_rollback` with a null actor,
      and `rollback_events` shows one `rolled_back` with that version. Then post the same with
      a **client** key against a second flag with a default policy → nothing trips within 1 s.
- [ ] `propose_rollback_changes_nothing`: after telemetry that would trip, `propose_rollback`
      reports `wouldTrip: true` and the flag's version is unchanged.
- [ ] Run `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test -p flags-server --test e2e`, then the three gates.
- [ ] Commit: `test: cover auto-rollback end to end` — `Refs #11`.

### Task 14: Docs

**Files:** Modify `docs/deploy/fly.md`, `README.md`, `docs/SDK-DEVELOPER-GUIDE.md`.

- [ ] `docs/deploy/fly.md`: a short `## Auto-rollback` section — plan-gated through the
      platform's `plans.features` (cached 60 s; fails closed), checks run when telemetry
      arrives plus a 5-minute pass while awake (works with `auto_stop_machines = "suspend"`, no
      cost change), latency bounded by SDK telemetry flush intervals, the
      image-rollback-after-`0002` caveat (roll forward), and the SELECT-only diagnostics
      `SELECT outcome, count(*) FROM rollback_events GROUP BY 1;` and
      `SELECT count(*) FILTER (WHERE tripped_at IS NULL), count(*) FROM rollback_policies;`.
- [ ] `README.md`: name `set_rollback_policy` / `propose_rollback` / `rollback_events` in
      "Using it".
- [ ] `docs/SDK-DEVELOPER-GUIDE.md`, telemetry section: one paragraph — telemetry reported
      with the server key drives auto-rollback; client-key reports count only for policies that
      opt in. No wire change.
- [ ] Run the three gates (docs-only, but CI runs them).
- [ ] Commit: `docs: describe auto-rollback` — body: what operators and SDK users need to
      know. **`Fixes #11`** (last commit).

## Testing summary

| Where | What |
|---|---|
| `flags-core` unit (`rollback.rs`, `entitlements.rs`, `telemetry.rs`) | `decide` boundaries, `assess`/`decide` agreement, `validate`, `Schedule`, `Hints`, `from_status`. |
| `flags-core/tests/db.rs` | Isolation of the new tables, defaults agree with Rust, reporter recording, samples (source, NULL env, window), policy CRUD + audit, re-arming, every `check_flag` outcome, `due_policies` unpinned, purge, sweep. |
| `flags-mcp` unit | Routing/metering consistency, error codes, instructions. |
| `flags-server/tests/e2e.rs` | Plan gate (not entitled, legacy platform, unavailable), full trip through the SDK API with SSE notification, client-key reports ignored by default, dry run. |

## Out-of-band (Phase 5)

- **otto-platform pin:** both crates → `fa1ae08` (Task 1). Platform 0.5.0 is already deployed
  (2026-10-10), so the merge is deploy-safe: entitlement answers carry `features` from the
  first request.
- **Migration `0002`:** runs at boot. Confirm `applying migrations` and the isolation summary
  naming `rollback_policies` / `rollback_events` in the boot logs. Additive, so the old
  machine keeps serving during the rollout; rolling **back** the image after it applies is not
  possible (spec Risks) — roll forward.
- **No new Fly secret or env var, no new scope, no platform re-registration, no SDK package
  change, no changeset, no recurring cost.**
- **Production check:** `/readyz`; `tools/list` over `/mcp` shows the five new tools; the two
  SELECTs from Task 14 return without error.
