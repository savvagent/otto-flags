# An SDK key lookup holds its permit for a bounded time

**Status:** IMPLEMENTED (#134)
**Issue:** #130

The code is authoritative where this sketch differs in detail. As built after review: the
server's `idle_in_transaction_session_timeout` is the transaction deadline plus 500 ms
(`keys::IDLE_PAST_DEADLINE`), so the client always gives up first with its own reason and the
server's timeout is only the backstop for a half-open link. Setting values round up to whole
milliseconds, at least 1 ms and at most `i32::MAX` ms (`unpinned::millis`). `Held::release`
returns whether sqlx's return finished in time, not whether the connection was pooled. Tests pin
the `statement_timeout` path (`a_slow_lookup_gives_up_at_the_statement_timeout`) and the
connection's return after a lock timeout. The stream re-check's remaining gaps (dropped mid-check,
uncapped concurrency, errors not logged, no tests) are #133.

## 1. Brief

From #130 (follow-up to #120, PR #129):

> The SDK key lookup (`flags_core::keys::resolve_hash`) runs its unpinned transaction with no
> `lock_timeout` or `statement_timeout`. Since #120 a lookup holds one of the `KeyLookups` permits
> (2 on the production pool) for as long as it takes. A long lock on `app_keys`, such as a
> migration's `ALTER TABLE` or a slow rotation, therefore holds both permits for its whole
> duration. Meanwhile every SDK request that is not a remembered unknown key gets `503 busy`.
> Before #120 the same stall would have held the whole pool, so #120 is strictly better, but a
> lookup's worst case is still unbounded. The same is true on the release side: sqlx's release
> ping in `return_to_pool` has no timeout of its own, so a half-open TCP connection keeps its
> permit until the kernel gives up on the socket.
>
> **Acceptance criteria**
> - [ ] A key lookup that waits on a lock or a slow statement fails within a bounded time, with a
>   transient error that maps to `503`. It must not be cached as unknown.
> - [ ] The bound does not add a round trip unnoticed: `crates/flags-core/tests/key_round_trips.rs`
>   is updated deliberately if the count changes. Setting the timeout in the existing probe
>   statement would avoid one, but the probe is shared with every unpinned and maintenance
>   transaction.
> - [ ] The release ping is bounded too, or the PR says why not. A timeout around `Held::release`
>   must not free the permit while the connection is still checked out.
> - [ ] A test shows a lookup blocked on an `app_keys` lock gives up within the bound and frees its
>   permit.

Caller constraints: fold the timeouts into an existing statement so the six-round-trip pin
(`key_round_trips.rs`) holds, or justify changing it; a timed-out lookup maps to a transient 503,
not "invalid key", and is never cached as unknown.

## 2. Premise corrections

- **A rotation does not hold a lock a lookup waits on.** `rotate_keys` deletes and inserts
  `app_keys` rows; row locks never block a plain `SELECT`. What blocks a lookup is a table lock in
  ACCESS EXCLUSIVE mode: `ALTER TABLE`, `LOCK TABLE`, `TRUNCATE`, `VACUUM FULL`, `DROP`. The bound
  matters for those, and for a slow or unresponsive database.
- **A server-side timeout cannot bound a half-open connection.** `statement_timeout` cancels the
  statement on the server, but if the TCP connection is half-open the client never hears about it.
  The release ping is not the only exposed step: `BEGIN`, the probe, the lookup and `COMMIT` wait
  on the same socket. A client-side deadline is needed for the query side as well as the release.
- **Setting the timeouts inside the lookup statement would not work.** The lookup's
  `AccessShareLock` on `app_keys` is taken while the statement is bound (plan revalidation),
  before its select list runs, and the statement timer is armed when the statement starts. A
  `set_config` in the lookup itself is too late for both. They have to be set by a statement that
  runs before it: the probe.

## 3. Assumptions

- **The probe sets the timeouts only when asked.** `unpinned::enter` gains optional
  timeouts (`lock_timeout`, `statement_timeout`, `idle_in_transaction_session_timeout`), bound as
  text parameters; each is applied by a `set_config(…, true)` in its own
  CASE column, exactly like the maintenance marker. Every other unpinned and maintenance
  transaction binds NULL and runs unchanged, so the shared probe is not affected. One statement
  text for every caller, so no extra entry in a connection's statement cache.
- **Values.** `lock_timeout` 500 ms, `statement_timeout` 1 s, a client-side deadline of 2 s on the
  whole transaction (BEGIN to COMMIT), and 1 s on the release. A healthy lookup takes a few
  milliseconds; a lock wait shows as `lock_not_available` (55P03) in the logs, distinct from a
  slow statement's `query_canceled` (57014). `idle_in_transaction_session_timeout` is set to the
  transaction deadline (2 s): a lookup is never idle inside its transaction for long, and when the
  client gives up on a half-open link the backend, still holding its `AccessShareLock` on
  `app_keys` (which would block the very `ALTER TABLE` this is about), ends itself instead of
  lingering until the kernel notices. Worst case a permit is held: the pool's 5 s acquire
  timeout (unchanged, already bounded), plus 2 s, plus 1 s.
- **Server-side timeouts stay as they are mapped today.** 55P03 and 57014 are already transient
  (`error::TRANSIENT_SQLSTATES`), internal, and answered `503 temporarily_unavailable` with
  `Retry-After: 5` (`flags-api/src/error.rs`). `resolve_cached` remembers only `Ok(None)`, so an
  error is never cached. No new error variant.
- **The client-side deadline reports `sqlx::Error::Io(TimedOut)`.** Transient, internal, `503
  temporarily_unavailable`, logged with reason `io`. A new `flags_core::Error` variant would fall
  into the API's `400` arm unless mapped, for no gain.
- **A connection that misses a client-side deadline is closed, never returned.** It may be
  mid-protocol. `Held::discard` detaches it from the pool (the pool's size drops, so a
  replacement may open) and drops it, which closes the socket synchronously, before the lookup
  returns. The release timeout drops sqlx's `return_to_pool` future, which owns the floating
  connection: dropping it closes the socket and decrements the pool's size, also before `release`
  returns (`sqlx-core-0.8.6/src/pool/connection.rs`, `return_to_pool`, `Floating`,
  `DecrementSizeGuard::drop`). Either way the permit is freed only after the connection is gone.
- **The bounds are a value on `KeyLookups`, with production defaults.** `KeyLookups::new` uses
  `LookupBounds::DEFAULT`; `KeyLookups::with_bounds` replaces it, so the #120 tests that keep a
  lookup blocked on a lock can still do so (with long bounds). `resolve_hash` (the stream's
  ungated re-check) uses the defaults; a timed-out re-check is a database error, which keeps the
  stream open, as today.
- **Run Profile:** every value comes from the section; none assumed.

## 4. Goal & success criteria

No SDK key lookup holds a `KeyLookups` permit, or a pool connection, longer than a fixed bound,
whatever the database or the network does.

- A lookup blocked on an ACCESS EXCLUSIVE lock on `app_keys` fails within `lock_timeout` plus
  scheduling slack with a transient error, frees its permit, and caches nothing.
- Through the SDK API the same request answers `503 temporarily_unavailable` with `Retry-After`.
- A lookup whose connection stops answering mid-transaction fails by the client-side deadline,
  and the connection is closed (pool size drops) before the permit is freed.
- A lookup whose connection stops answering at the release ping returns its answer by the release
  timeout, with the connection closed.
- `key_round_trips.rs` still counts six round trips per lookup.

## 5. Scope

**In:** the probe's optional timeouts (`crates/flags-core/src/unpinned.rs`); `LookupBounds`,
the client-side deadline and release timeout (`crates/flags-core/src/keys.rs`); tests in
`crates/flags-core/tests/db.rs`, `crates/flags-core/tests/key_round_trips.rs`,
`crates/flags-api/tests/key_lookups.rs`, and unit tests in `unpinned.rs`.

**Out:** timeouts on any other transaction (pinned, maintenance, MCP); the pool's acquire timeout
and `test_before_acquire` ping (already bounded by the 5 s acquire timeout); TCP keepalive
settings; changing how database errors are logged.

## 6. Architecture

- `unpinned::enter(tx, db, maintenance, timeouts: Option<Timeouts>)`: three extra columns,
  `CASE WHEN $4::text IS NOT NULL THEN set_config('lock_timeout', $4::text, true) END` and the
  same for `statement_timeout` (`$5`) and `idle_in_transaction_session_timeout` (`$6`). `begin`/`begin_as_tenant_role` pass `None`;
  `Held::begin(db, timeouts)` passes what it is given. Values are whole milliseconds, rounded up and never below 1 (`0` would mean "off"), rendered as
  `"<n>ms"`.
- `unpinned::Held::release(self, within: Duration) -> bool` (true if sqlx's return finished
  within the bound, whether it pooled or closed the connection; false if it was cut short):
  `tokio::time::timeout(within, self.0.return_to_pool())`. `Held::discard(self)`: detach and drop.
- `keys::LookupBounds { lock, statement, transaction, release }`, `LookupBounds::DEFAULT`
  (500 ms, 1 s, 2 s, 1 s). `KeyLookups` stores one; `KeyLookups::with_bounds(self, b)`;
  `KeyLookups::bounds()`.
- `keys::resolve_hash(db, hash)` = `resolve_hash_within(db, hash, &LookupBounds::DEFAULT)`
  (private helper). It acquires, runs `lookup` under `tokio::time::timeout(bounds.transaction)`,
  then either releases within `bounds.release` and returns the answer, or discards the
  connection, logs one `warn` with the fixed reason `transaction_timed_out` (so on-call can tell
  it from a broken socket, which the API also logs as `io`), and returns
  `Error::Db(Io(TimedOut))`. A release that times out logs one `warn` with
  a fixed reason and still returns the answer. `resolve_cached` calls it with `limit`'s bounds.

No public interface removed; `KeyLookups::with_bounds`, `KeyLookups::bounds`, `LookupBounds` are
additive. No SDK wire change: the 503 body and header already exist.

## 7. Invariants impact

- **No SQL outside flags-core; runtime queries only:** the probe stays a runtime `query_as`.
- **Tenant isolation:** the probe's role logic is unchanged; the new columns only set three
  transaction-local GUCs. The probe still runs first in every unpinned transaction.
- **Small shared database:** strictly better; a lookup's hold time is now bounded.
- **SDK REST contract:** unchanged (503 `temporarily_unavailable` already documented).
- **SDK path never calls the platform:** unchanged.
- No migration, no config key, no secret, no scope.

## 8. Migration

None.

## 9. Out-of-band

None.

## 10. Error handling & edge cases

- `lock_timeout` and `statement_timeout` are transaction-local: they end with the transaction and
  never reach the next user of the connection, and the probe itself is not under them (they take
  effect from the next statement, the lookup).
- The tests' `KeyLookups::with_bounds` with long bounds preserve #120's blocked-lookup tests.
- A permit is freed only after `resolve_hash_within` returns, which is after the connection is
  back, discarded, or dropped by the release timeout.
- A release timeout does not turn the answer into an error: the answer was already read.

## 11. Testing approach

- `unpinned.rs` unit: a held transaction begun with timeouts reports them via `current_setting`;
  one begun with `None`, and a plain `begin`, keep the defaults.
- `tests/db.rs`: with default bounds, a lookup blocked on the `app_keys` lock fails within 1.5 s
  with a transient error (`is_transient`), the permit is back, nothing is remembered; the #120
  tests use long bounds.
- `tests/key_round_trips.rs`: the six-round-trip tests unchanged (they now cover the timeout
  columns); a stalling switch (the client sends one synchronisation point, 'Q' or 'S', per round
  trip and waits for its answer, so counting them picks the boundary deterministically) in the counting proxy shows (a) a lookup stalled mid-transaction
  fails by `bounds.transaction` with the connection closed and the permit free, (b) a lookup
  stalled at the release ping answers by `bounds.release` with the connection closed.
- `flags-api/tests/key_lookups.rs`: a request whose lookup is blocked on the lock answers `503
  temporarily_unavailable` with `Retry-After: 5` within the bound; the existing tests use long
  bounds.

## 12. Risks & open questions

- Under a long ACCESS EXCLUSIVE lock each lookup now logs one `error` line ("SDK request failed",
  reason `lock_not_available`) every ~500 ms per permit, at most ~4/s on production. Acceptable;
  a rate limit like `busy`'s is a follow-up if it proves noisy.
- A lookup during a slow but healthy database (≥ 1 s) now fails instead of succeeding late. At
  the expected few-ms lookups this is a sign of trouble; the SDKs serve cached or default values.
