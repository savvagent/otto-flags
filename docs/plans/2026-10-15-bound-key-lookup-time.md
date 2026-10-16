# Bound SDK Key Lookup Time Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** No SDK key lookup holds a `KeyLookups` permit or a pool connection longer than a fixed
bound: server-side `lock_timeout`/`statement_timeout` set by the existing probe, a client-side
deadline on the transaction, and a timeout on the release ping.

**Architecture:** `unpinned::enter`'s probe gains two optional `set_config` columns (NULL for
every caller but the key lookup), so the timeouts cost no round trip. `keys::LookupBounds` holds
the four bounds; `resolve_hash` runs the transaction under a client-side deadline and the release
under a timeout, closing the connection (never returning it) when either expires, before the
permit is freed.

**Tech Stack:** Rust, sqlx 0.8.6, tokio, Postgres 16.

**Ticket:** savvagent/otto-flags#130

**Spec:** `docs/specs/2026-10-15-bound-key-lookup-time-design.md`

## Global Constraints

- Gates, from the worktree `/home/robhicks/dev/otto-flags/.claude/worktrees/fix-130-bound-key-lookup-time`,
  with `export CARGO_BUILD_JOBS=3 TMPDIR=$PWD/target/tmp` (mkdir it first):
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15453/otto_flags cargo test --workspace`
    (port 15453 is this worktree's own Postgres; never 15434, never a Fly database)
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'` prints nothing
  - `podman build -t otto-flags:check .` (crates/** changed) before the PR
- Evidence: report pass counts per test binary. A panic "DATABASE_URL must be set" is a missing
  variable, not a test failure. sqlx test-db setup panics inside sqlx-core `testing/mod.rs` are
  environmental under load: re-run. Skipped tests are not a pass.
- Invariants: no SQL outside flags-core, runtime `sqlx::query*` only (no macros); the probe still
  runs first in every unpinned and maintenance transaction with its role logic unchanged; the SDK
  REST contract unchanged; the six-round-trip pin in `key_round_trips.rs` holds.
- Worktree only; trunk changes only through the PR.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why and ends
  `Fixes #130`; no attribution of any kind (no
  Co-Authored-By, no "Generated with", no credit markers).

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `crates/flags-core/src/unpinned.rs` | `Timeouts`, the probe's optional timeout columns, `Held::begin(db, timeouts)`, `Held::release(within) -> bool`, `Held::discard` |
| **Modify.** `crates/flags-core/src/keys.rs` | `LookupBounds` + `DEFAULT`, `KeyLookups::with_bounds`/`bounds`, `resolve_hash` under the deadline and release timeout |
| **Modify.** `crates/flags-core/tests/db.rs` | #120 tests use long bounds; new test: a lookup blocked on the lock gives up within the bound |
| **Modify.** `crates/flags-core/tests/key_round_trips.rs` | doc update (probe sets the timeouts); stalling proxy tests for the deadline and the release timeout |
| **Modify.** `crates/flags-api/tests/key_lookups.rs` | existing tests use long bounds; new test: a blocked lookup answers 503 temporarily_unavailable |

## Task Order & Rationale

Task 1 adds the probe's timeouts (testable alone in `unpinned.rs`). Task 2 adds `LookupBounds`
and the server-side bound end to end, with the #120 tests moved to long bounds in the same commit
so the suite stays green. Task 3 adds the client-side deadline and the release timeout, which
need the stalling proxy. Task 4 is the API-level acceptance test.

---

### Task 1: The probe sets optional lookup timeouts

**Files:**
- Modify: `crates/flags-core/src/unpinned.rs`

**Interfaces:** produces `pub(crate) struct Timeouts { pub lock: Duration, pub statement: Duration, pub idle: Duration }`
and `Held::begin(&mut self, db: &Db, timeouts: Timeouts)`, used by Task 2.

**Out-of-band / invariants:** probe still first, role logic unchanged; runtime query only.

**Model:** sonnet

- [ ] **Step 1: Write the failing tests** in `unpinned.rs`'s `tests` module:
  `a_held_connection_sets_the_lookup_timeouts` (begin a `Held` transaction with
  `Timeouts { lock: 250ms, statement: 750ms, idle: 1500ms }`; `current_setting` reports `250ms`,
  `750ms`, `1500ms` for `lock_timeout`, `statement_timeout`, `idle_in_transaction_session_timeout`;
  after commit and a new plain `begin` on the same pool all three are back to `0`), and
  `begin_leaves_the_timeouts_alone` (plain `begin` and `begin_as_tenant_role(_, true)` report `0`
  for all three). Update
  `a_held_connection_begins_as_the_tenant_role_without_the_marker` to pass timeouts.
- [ ] **Step 2: Run, see them fail to compile:**
  `DATABASE_URL=… cargo test -p flags-core --lib unpinned`
- [ ] **Step 3: Implement.** `enter(tx, db, maintenance, timeouts: Option<Timeouts>)`; bind
  `$4`/`$5`/`$6` as `Option<String>` (`format!("{}ms", d.as_millis())`); two more select-list columns,
  `CASE WHEN $4::text IS NOT NULL THEN set_config('lock_timeout', $4::text, true) END` and the
  same for `statement_timeout` (`$5`) and `idle_in_transaction_session_timeout` (`$6`), each in its own column (the comment explains why: they
  take effect from the next statement, the lookup; the lookup's lock is taken when it is bound,
  so setting them in the lookup itself would be too late). Extend the destructured tuple.
  `begin_as_tenant_role` passes `None`; `Held::begin` passes `Some(timeouts)`.
- [ ] **Step 4: Run, see them pass;** also `cargo test -p flags-core --test key_round_trips`
  (still 6 round trips).
- [ ] **Step 5: Commit** `feat(core): let the unpinned probe set a lookup's lock and statement timeouts`.

### Task 2: Key lookups run under the server-side timeouts

**Files:**
- Modify: `crates/flags-core/src/keys.rs`, `crates/flags-core/tests/db.rs`,
  `crates/flags-api/tests/key_lookups.rs`

**Interfaces:** produces `pub struct LookupBounds { pub lock, pub statement, pub transaction,
pub release: Duration }`, `LookupBounds::DEFAULT` (500 ms, 1 s, 2 s, 1 s),
`KeyLookups::with_bounds(self, LookupBounds) -> Self`, `KeyLookups::bounds() -> LookupBounds`.
Task 3 consumes `transaction`/`release`.

**Model:** sonnet

- [ ] **Step 1: Failing test** in `tests/db.rs`:
  `a_lookup_blocked_on_a_lock_gives_up_within_the_bound`: with `KeyLookups::new(1, KEY_LOOKUP_WAIT)`
  (default bounds), lock `app_keys` (`lock_app_keys`), call `resolve_cached` with a fresh key;
  it returns `Err(e)` with `e.is_transient()` and `e.failure_reason().reason ==
  "lock_not_available"`, within 1.5 s; `limit.available() == 1`; `unknown.is_empty()`; then
  unlock. Change the #120 blocked-lookup tests (`a_lookup_waits_for_a_permit_and_is_refused_after_the_wait`,
  `a_dropped_lookup_still_remembers_its_unknown_key`) and the two tests in
  `crates/flags-api/tests/key_lookups.rs` to build their `KeyLookups` with
  `.with_bounds(long)` where `long` is 30 s for every bound, so they stay blocked until unlocked.
- [ ] **Step 2: Run, see it fail** (compile error, then the lookup waits for the unlock).
- [ ] **Step 3: Implement** `LookupBounds`, store it in `KeyLookups` (`new` uses `DEFAULT`),
  `with_bounds`, `bounds`. Private `resolve_hash_within(db, hash, &LookupBounds)`; `resolve_hash`
  calls it with `DEFAULT`; `resolve_cached` with `limit.bounds()`; `lookup` passes
  `Timeouts { lock, statement, idle: transaction }` to `Held::begin`. Update the module and `KeyLookups` docs.
- [ ] **Step 4: Run** `cargo test -p flags-core --test db` and `-p flags-api --test key_lookups`: pass.
- [ ] **Step 5: Commit** `feat(core): give up on an sdk key lookup blocked on a lock`.

### Task 3: Client-side deadline and release timeout

**Files:**
- Modify: `crates/flags-core/src/unpinned.rs`, `crates/flags-core/src/keys.rs`,
  `crates/flags-core/tests/key_round_trips.rs`

**Model:** opus — cancellation and connection-ownership reasoning.

- [ ] **Step 1: Failing tests** in `key_round_trips.rs`. Give `RoundTrips` a `stall_after:
  Arc<AtomicUsize>` (default `usize::MAX`); in `forward_client`, a 'Q' or 'S' whose count (after
  increment) exceeds it is not forwarded and the forwarder parks forever
  (`std::future::pending`). Tests, each warming the connection first, then `take`, then setting
  `stall_after`, with `KeyLookups::new(1, KEY_LOOKUP_WAIT).with_bounds(short)` where short is
  lock 100 ms, statement 200 ms, transaction 300 ms, release 300 ms:
  - `a_lookup_stalled_mid_transaction_closes_its_connection`: `stall_after = 3` (acquire ping,
    BEGIN, probe forwarded; the lookup stalls); `resolve_cached` returns an `Err` that
    `is_transient()` with reason `io` within 1 s; right after it, `db.pool().size() == 0`,
    `limit.available() == 1`, `unknown.is_empty()`.
  - `a_lookup_stalled_at_the_release_ping_still_answers`: `stall_after = 5` (the 6th, the release
    ping, stalls); `resolve_cached` returns `Ok(None)` within 1 s; right after,
    `db.pool().size() == 0`, `limit.available() == 1`.
- [ ] **Step 2: Run, see them fail** (they hang until the test's own outer timeout: wrap each call
  in `tokio::time::timeout(5s)` so the failure is an assertion, not a hang).
- [ ] **Step 3: Implement.** `Held::release(self, within: Duration) -> bool`:
  `tokio::time::timeout(within, self.0.return_to_pool()).await.is_ok()` (true: sqlx's return finished in time, pooled or closed; false: cut short); the doc says why a
  timeout here never frees the permit with the connection out (the future owns the floating
  connection; dropping it closes the socket and decrements the pool's size; cite sqlx-core 0.8.6
  `pool/connection.rs`). `Held::discard(self)`: `drop(self.0.detach())`, same reasoning.
  `resolve_hash_within`: `match tokio::time::timeout(bounds.transaction, lookup(..)).await`:
  `Ok(owner)` → `if !conn.release(bounds.release).await { warn!(reason = "release_timed_out", …) }`,
  return owner; `Err(_)` → `conn.discard()`, `warn!(reason = "transaction_timed_out", …)`,
  return `Err(Error::Db(sqlx::Error::Io(io::ErrorKind::TimedOut.into())))`. Keep the "no `?`
  between acquire and release" comment accurate.
- [ ] **Step 4: Run** `cargo test -p flags-core --test key_round_trips --test db` and `--lib`: pass;
  the six-round-trip tests unchanged.
- [ ] **Step 5: Commit** `feat(core): bound an sdk key lookup's transaction and release ping`.

### Task 4: The SDK API answers a blocked lookup with a transient 503

**Files:**
- Modify: `crates/flags-api/tests/key_lookups.rs`

**Model:** sonnet

- [ ] **Step 1: Test** `a_lookup_blocked_on_a_lock_answers_503_within_the_bound`: default bounds
  (`KeyLookups::new(2, KEY_LOOKUP_WAIT)`), lock `app_keys`, one request with a fresh unknown key;
  it answers `(503, Some("5"), Some("temporarily_unavailable"))` within 2 s; permits back to 2.
  (Passes already after Task 2: it pins the mapping; record that it was written after the
  implementation.)
- [ ] **Step 2: Run** `cargo test -p flags-api --test key_lookups`: pass.
- [ ] **Step 3: Commit** `test(api): pin the 503 for an sdk key lookup that gives up`.

### Task 5: Full gates and record as shipped (before merge)

- [ ] Run every gate in Global Constraints; record counts.
- [ ] Final commit before merge: spec `**Status:** IMPLEMENTED (#<PR>)`, plan boxes ticked.
