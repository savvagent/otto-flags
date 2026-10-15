# SDK key lookup limit Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** Unauthenticated SDK key lookups hold at most `max(1, (pool - 1) / 2)` pool connections
at once (2 of 5 in production); a lookup that cannot start within 2 s gets `503` + `Retry-After`
with code `busy` (#120).

**Architecture:** `flags_core::keys::KeyLookups` (an `Arc<Semaphore>` plus a wait) gates the
database half of `resolve_cached`. The lookup runs in a spawned task that owns its permit, and
`resolve_hash` awaits the connection's return to the pool, so a permit is never free while its
connection is checked out. `flags_api::AppState` sizes one from the pool; `Error::Busy` maps to
`503`.

**Tech Stack:** Rust, sqlx 0.8 (runtime queries), tokio, axum, Postgres 16.

**Ticket:** savvagent/otto-flags#120

**Spec:** `docs/specs/2026-10-15-key-lookup-limit-design.md`

## Global Constraints

- Worktree: `/home/robhicks/dev/otto-flags/.claude/worktrees/feat-120-cap-key-lookups`. Worktree
  only; trunk changes only through the PR.
- Environment for every cargo command:
  `export CARGO_BUILD_JOBS=3; mkdir -p target/tmp; export TMPDIR=$PWD/target/tmp`.
- Gates:
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15451/otto_flags cargo test --workspace`
    (dedicated container; never 15434 or a Fly database)
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'`
    prints nothing
  - `podman build -t otto-flags:check .` (crates changed; final run only)
- Evidence: pass counts per test binary; a `DATABASE_URL must be set` panic is a missing
  variable; a sqlx-core `testing/mod.rs` setup panic is environmental, re-run first.
- Invariants: the database is small and shared (the point: bounded connections); SDK REST
  contract additive (new `503` cause, existing status); SDKs fall back on `5xx`; client keys held
  only as SHA-256 hashes; the SDK path never calls the platform; no SQL outside flags-core,
  runtime queries only, SQL text unchanged; tenant isolation unchanged (same probe on the
  borrowed connection); no migration.
- Revocation: never cache a positive result or an error; `still_valid` stays ungated.
- Out-of-band: none (no config key, secret, or scope).
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why, ends
  `Fixes #120`. No attribution of any kind (no Co-Authored-By, no "Generated with", no credit
  markers).

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `crates/flags-core/src/unpinned.rs` | Probe factored into a helper on a `Transaction<'c, Postgres>`; new `begin_on(db, &mut PgConnection)` |
| **Modify.** `crates/flags-core/src/error.rs` | `Error::Busy` (`busy`, retriable, transient, not internal) |
| **Modify.** `crates/flags-core/src/keys.rs` | `KEY_LOOKUP_WAIT`, `KeyLookups`, gated `resolve_cached`, `resolve_hash` awaiting `return_to_pool`; unit tests |
| **Modify.** `crates/flags-core/tests/db.rs`, `crates/flags-core/tests/key_round_trips.rs` | New `resolve_cached` argument |
| **Modify.** `crates/flags-api/src/lib.rs` | `AppState::key_lookups` |
| **Modify.** `crates/flags-api/src/sdk.rs` | Extractor passes the limit |
| **Modify.** `crates/flags-api/src/error.rs` | `Busy` → `503 busy`, throttled warn |
| **Create.** `crates/flags-api/tests/key_lookups.rs` | Pool-share and cancellation tests |
| **Modify.** `docs/SDK-DEVELOPER-GUIDE.md` | `503` row in the status table |

## Task 1: Gate key lookups in flags-core

**Model:** opus (connection-lifetime and cancellation reasoning)

**Files:** `crates/flags-core/src/{unpinned.rs,error.rs,keys.rs}`, `crates/flags-core/tests/{db.rs,key_round_trips.rs}`

- [ ] **Step 1: Failing tests.** In `keys.rs` `mod tests`, add
  `key_lookups_take_half_of_what_the_listener_leaves`: `KeyLookups::for_pool(2).permits() == 1`,
  `for_pool(3) == 1`, `for_pool(5) == 2`, `for_pool(10) == 4`, `for_pool(0) == 1`,
  `for_pool(1) == 1`, and `available() == permits()`
  on a fresh one. In `crates/flags-core/tests/db.rs`, add
  `a_lookup_waits_for_a_permit_and_is_refused_after_the_wait` (`#[sqlx::test(migrator =
  "flags_core::MIGRATOR")]`): `KeyLookups::new(1, Duration::from_millis(200))`; a separate
  `PgConnection` (connect with `pool.connect_options()`) runs `BEGIN; LOCK TABLE public.app_keys
  IN ACCESS EXCLUSIVE MODE`; spawn `resolve_cached` of one random minted key; poll until
  `available() == 0`; a second `resolve_cached` of another key returns `Err(Error::Busy)` (match
  on the variant) after the wait; a cached unknown key (insert one first via a lookup before the
  lock) still answers `Ok(None)` with no permit free; then `ROLLBACK` the lock and the spawned
  (spawned calls need owned values: a cloned `Db`, `Arc<UnknownKeys>`, `Arc<KeyLookups>`, an
  owned `String` key)
  lookup returns `Ok(None)`, and `available()` returns to 1 (poll with a 5 s deadline). Update
  every existing `resolve_cached` call in `db.rs` and `key_round_trips.rs` to the new signature:
  `unknown` becomes `Arc<UnknownKeys>` (`Arc::new(UnknownKeys::new())`) and a
  `&KeyLookups::new(1, KEY_LOOKUP_WAIT)` is passed (round-trip expectations stay 6 and 0). Also
  add `a_dropped_lookup_still_remembers_its_unknown_key` to `db.rs`: with the app_keys lock held,
  spawn a `resolve_cached` of an unknown key, wait until `available() == 0`, `abort()` it,
  `ROLLBACK` the lock, poll (5 s) until `available() == 1`, then `unknown.len() == 1`.
- [ ] **Step 2: Run, see them fail** (do not compile):
  `DATABASE_URL=postgres://flags:flags@localhost:15451/otto_flags cargo test -p flags-core`.
- [ ] **Step 3: `unpinned.rs`.** Move the probe-and-check body of `begin_as_tenant_role` into
  `async fn enter<'c>(tx: Transaction<'c, Postgres>, db: &Db, maintenance: bool) ->
  sqlx::Result<Transaction<'c, Postgres>>` (same SQL, same refusal). `begin_as_tenant_role`
  becomes `enter(db.pool().begin().await?, db, maintenance)`. Add
  `pub(crate) async fn begin_on<'c>(db: &Db, conn: &'c mut PgConnection) ->
  sqlx::Result<Transaction<'c, Postgres>>` = `enter(conn.begin().await?, db, false)` (needs
  `use sqlx::Connection;`). Add `pub(crate) async fn acquire(db: &Db) ->
  sqlx::Result<PoolConnection<Postgres>>` (`db.pool().acquire()`) and `pub(crate) fn
  max_connections(db: &Db) -> u32` (`db.pool().options().get_max_connections()`), so every raw
  pool access stays in this file: update `no_raw_pool_queries_outside_the_allowed_sites`
  (`crates/flags-core/tests/db.rs`) from `("flags-core/src/unpinned.rs", 1)` to `3`, with the
  reasons in its comment (a connection the key lookup holds so it can await its return before
  freeing its permit, #120; reading the pool's size, which touches no table). Add a unit
  test beside the existing ones: `begin_on` runs as the tenant role without the marker (mirror
  `begin_runs_as_the_tenant_role_without_the_marker`, adapting `role_and_marker` to a
  non-`'static` transaction: change its parameter to `&mut Transaction<'_, Postgres>`).
- [ ] **Step 4: `error.rs`.** `Error::Busy` with message "too many SDK key lookups are in
  progress; retry shortly", `code() == "busy"`, `retriable()` and `is_transient()` true,
  `is_internal()` false (falls through `failure_reason` to `Failure::reason("busy")`). Fix any
  exhaustive match the compiler flags elsewhere (MCP error mapping) treating it like other
  retriable, non-internal errors.
- [ ] **Step 5: `keys.rs`.**
  - `pub const KEY_LOOKUP_WAIT: Duration = Duration::from_secs(2);`
  - `pub struct KeyLookups { permits: Arc<tokio::sync::Semaphore>, size: usize, wait: Duration }`
    with `new(permits, wait)` (asserts `permits >= 1`), `for_pool(max_connections: u32)`
    (`max(1, max.saturating_sub(1) / 2)`, `KEY_LOOKUP_WAIT`), `for_db(db: &Db)` (=
    `for_pool(unpinned::max_connections(db))`), `permits()`, `available()`; `Debug`. Doc
    comment carries the reasoning: the listener holds one connection; why the permit must cover
    the release ping and survive cancellation (sqlx returns connections in a spawned task on
    drop).
  - `resolve_hash`: `let mut conn = unpinned::acquire(db).await?;` run the existing query in an
    inner `async fn lookup(db, &mut PgConnection, hash) -> Result<Option<KeyOwner>>` that uses
    `unpinned::begin_on(db, conn)` and commits as today; hold its result, then
    `conn.return_to_pool().await;` on every outcome, then return the result. No `?` between the
    acquire and `return_to_pool` (an early return would let sqlx's spawned release free the
    permit while the connection is busy; say so in a comment).
  - `resolve_cached(db, unknown: &Arc<UnknownKeys>, limit: &KeyLookups, key)`: unchanged up to
    the cache check;
    then `tokio::time::timeout(limit.wait, limit.permits.clone().acquire_owned())` → timeout is
    `Err(Error::Busy)` (a closed semaphore cannot happen; treat as `Busy` too); then
    `tokio::spawn` a task owning the permit, `db.clone()`, `unknown.clone()`, and the hash that
    runs `resolve_hash`, inserts the hash into `unknown` on `Ok(None)`, and drops the permit
    after both; `await` the handle (`Err(join)` → `std::panic::resume_unwind(join.into_panic())`
    if it panicked, else `Busy` for a cancelled runtime). Update the module doc paragraph to
    mention the gate.
- [ ] **Step 6: Run, see them pass** (same command), then lint and format.
- [ ] **Step 7: Commit:** `feat(core): cap concurrent sdk key lookups below the pool size`,
  body explaining why (distinct-key flood, release ping and cancellation), `Fixes #120`.

## Task 2: Wire the limit into the SDK API

**Model:** sonnet

**Files:** `crates/flags-api/src/{lib.rs,sdk.rs,error.rs}`, `crates/flags-api/tests/key_lookups.rs`, `docs/SDK-DEVELOPER-GUIDE.md`

- [ ] **Step 1: Failing test** `crates/flags-api/tests/key_lookups.rs` (own binary,
  `#[sqlx::test(migrator = "flags_core::MIGRATOR")]`):
  - `a_flood_of_unknown_keys_leaves_connections_free`: build a pool of 5 from
    `pool.connect_options()` (`PgPoolOptions::new().max_connections(5)`); `AppState::new` on it,
    then replace `state.key_lookups` with `Arc::new(KeyLookups::new(2, 300 ms))`; open a separate
    `PgConnection` (outside the 5-pool, `PgConnection::connect_with`) and `BEGIN; LOCK TABLE public.app_keys IN ACCESS EXCLUSIVE MODE`; send 20
    `GET /api/sdk/flags` requests with distinct minted client keys, each in its own task that
    sends `(status, retry_after, error code)` on an mpsc channel; poll (5 s deadline) until
    `available() == 0` and the lock connection sees 2 sessions waiting on a lock
    (`SELECT count(*) FROM pg_stat_activity WHERE wait_event_type = 'Lock' AND datname =
    current_database()`); then acquire 3 connections from the 5-pool, each with a 1 s
    `tokio::time::timeout`, and hold them together (the AC); drop them; receive 18 results, each
    `503`, `Retry-After: 5`, `"error": "busy"`; `ROLLBACK` the lock; receive 2 results, each
    `401`.
  - `a_dropped_request_keeps_its_permit_until_the_lookup_ends`: same setup with
    `KeyLookups::new(1, 300 ms)`; spawn one request; poll until `available() == 0` and one lock
    waiter; `abort()` the task; sleep 100 ms; `available()` is still 0; `ROLLBACK`; poll (5 s)
    until `available() == 1`.
  Both tests close the 5-pool and the lock connection before returning (as `key_round_trips.rs`
  does), so sqlx can drop the test database.
- [ ] **Step 2: Run, see it fail** (no `key_lookups` field):
  `DATABASE_URL=postgres://flags:flags@localhost:15451/otto_flags cargo test -p flags-api --test key_lookups`.
- [ ] **Step 3: Implement.** `AppState` gains `pub key_lookups: Arc<KeyLookups>` (doc: #120),
  set in `new` from `KeyLookups::for_db(&db)` (no `.pool()` in flags-api), and
  `AppState::new`'s signature is unchanged (`unknown_keys` is already an `Arc`). The `Sdk`
  extractor passes `&state.key_lookups`. In `error.rs`, `From<flags_core::Error>` maps
  `Error::Busy` to `503`, code `busy` (an arm before the generic `e =>` arm, which would make it
  `400`), "the flag service is busy; serve cached or default values
  and retry" (the existing `IntoResponse` adds `Retry-After: 5` to every 503), logging a `warn`
  at most once a second (a process-wide `static` `Mutex<Option<Instant>>` or an `AtomicU64` of
  the last second, no new dependency) and nothing per request above `debug`. Update the module
  doc in `error.rs`. In `docs/SDK-DEVELOPER-GUIDE.md`'s HTTP Status Codes table, add
  `| \`503\` | Temporarily unavailable or busy (with \`Retry-After\`) | Return cached/default value; retry after the delay |`.
- [ ] **Step 4: Run, see it pass**; then the full gates (test, lint, format, extra).
- [ ] **Step 5: Commit:** `feat(api): refuse sdk key lookups past the limit with 503`, body why,
  `Fixes #120`.

## Close-out (before merge, in the PR)

- [ ] The spec's `**Status:**` becomes `IMPLEMENTED (#<PR>)` and this plan's boxes are ticked, in
  one final docs commit.
