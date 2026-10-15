# Unauthenticated SDK key lookups hold a bounded share of the pool

**Status:** DRAFT
**Issue:** #120

## 1. Brief

From #120 (follow-up to #104):

> #104 adds an in-process cache of SDK keys that did not resolve. A client that repeats a bad key
> (a misconfigured or revoked SDK, a replayed key, a scanner that retries) now costs no database
> work for 60 s. A flood of *distinct* well-formed random keys is unaffected: every new key still
> costs one lookup.
>
> On the production pool, measured on the wire by `crates/flags-core/tests/key_round_trips.rs`,
> one lookup holds a connection for six round trips: the acquire ping, `BEGIN`, the probe, the
> lookup, `COMMIT`, and the release ping. The pool has 5 connections and a 5 s acquire timeout.
> Such a flood therefore still competes with real SDK traffic, MCP tools, `/readyz`, and
> background work for those connections. Once the cache's 4096 slots are full, new bad keys are
> not cached either.
>
> **Acceptance criteria**
> - Unauthenticated key lookups cannot hold more than a bounded share of the pool. Either cap
>   concurrent key lookups below the pool size, for example with a semaphore in front of
>   `resolve_cached`, so other work always has a connection; or add a per-client limit on the
>   SDK API. A per-client limit needs a decision on trusting `Fly-Client-IP`.
> - A request refused by the limit gets a retryable status (`429` or `503` with `Retry-After`),
>   and the SDKs keep falling back to defaults and cached values.
> - A test shows that a burst of distinct unknown keys leaves at least one pool connection free
>   for other work.
>
> **Related:** #79 (one fewer round trip per lookup). The pool's `test_before_acquire` ping is
> also worth revisiting; it is one of the six round trips.

Caller constraints: valid keys must not be starved by the bound in normal operation; revocation
semantics unchanged; no new config secret.

## 2. Premise corrections

- **A semaphore around `resolve_cached` alone does not bound connections.** sqlx returns a
  connection to the pool, and sends its release ping, in a task it spawns when the
  `PoolConnection` drops (`sqlx-core-0.8.6/src/pool/connection.rs`, `Drop`). A permit dropped when
  `resolve_hash` returns is free again while that connection is still checked out, so the next
  waiter can take another one: up to twice the permits in use. The same happens when a request
  future is dropped mid-lookup (the client disconnects): the permit is released at once, and the
  connection is still busy rolling back and pinging. Both have to be closed for the bound to hold.
- **The pool is effectively 4, not 5, for request work.** The flag-change listener
  (`flags_core::notify::Listener::spawn`, `PgListener::connect_with(&pool)`) holds one pool
  connection for the life of the process.

## 3. Assumptions

- **A global concurrency cap, not a per-client limit.** A per-client limit needs a decision on
  trusting `Fly-Client-IP`, and a flood from many addresses defeats it anyway; the cap needs no
  client identity and bounds the shared resource directly. It does not refuse anything in normal
  operation (section 6, sizing).
- **Permits derive from the pool size; no new config key.** `permits = max(1, (max_connections -
  1) / 2)`: half of what the listener leaves, rounded down. On the production pool of 5 that is 2,
  so a flood holds at most 2 connections and the listener's 1 leaves 2 free for evaluation,
  MCP, `/readyz`, and background work. `FLAGS_DB_MAX_CONNECTIONS` (already a config key, at least
  2) scales it. At the minimum pool of 2 the cap is 1 and nothing is reserved beyond the
  listener; that is an operator's choice of a tiny pool, not a default.
- **A bounded wait, then `503` with `Retry-After`.** A lookup that cannot get a permit within
  `KEY_LOOKUP_WAIT` (2 s, under the pool's 5 s acquire timeout) is refused with the SDK
  surface's existing `503 temporarily_unavailable` body and `Retry-After: 5`. `503` rather than
  `429`: the limit is global, not the caller's rate, and `503` is the status every SDK already
  treats as "serve cached or default values" (`docs/SDK-DEVELOPER-GUIDE.md`, Error Handling; the
  server already sends it for database failures). The error code is `busy` so logs and tests can
  tell it from a database failure.
- **Only database lookups take a permit.** A malformed key and a cached unknown key are answered
  before the gate, so a repeating bad client never queues. A key that resolves takes a permit
  for its lookup, as it must: a key is only known to be valid after the lookup.
- **The stream's periodic re-check (`still_valid`) stays ungated.** It re-checks a key that
  already resolved, at most once per heartbeat per open stream; gating it would let a flood end
  or stall authenticated streams.
- Run Profile values were all read from the section. Assumed: local DB tests run on a dedicated
  container (`postgres://flags:flags@localhost:15451/otto_flags`) instead of the section's 15434,
  so parallel worktrees do not drop each other's test databases.

## 4. Goal and success criteria

A flood of distinct well-formed unknown keys can hold at most `permits` pool connections, so
other work always finds one, while valid keys keep resolving in normal operation.

- With every permit taken by lookups blocked in the database, `max_connections - permits`
  connections can still be acquired from the pool (the test proves 3 of 5).
- A lookup that waits past `KEY_LOOKUP_WAIT` gets `503`, `Retry-After: 5`, code `busy`.
- A lookup's permit is released only after its connection is back in the pool, including when
  the request is dropped mid-lookup.
- The round trips per lookup stay 6 (`key_round_trips.rs` unchanged and green).
- Revocation unchanged: a rotated or deleted org's key stops resolving on the next request.

## 5. Scope

In: `crates/flags-core/src/keys.rs` (the limit, `resolve_cached` behind it, `resolve_hash`
awaiting the connection's return), `crates/flags-core/src/unpinned.rs` (begin on a borrowed
connection), `crates/flags-core/src/error.rs` (`Error::Busy`), `crates/flags-api/src/lib.rs`
(`AppState` field), `crates/flags-api/src/sdk.rs`, `crates/flags-api/src/error.rs` (`busy` →
`503`), `docs/SDK-DEVELOPER-GUIDE.md` (a `503` row), and tests.

Out: a per-client limit; removing `test_before_acquire` or a round trip (#79); a positive key
cache; gating `still_valid`; MCP and other surfaces.

## 6. Architecture

`crates/flags-core/src/keys.rs`:

```rust
/// How long a lookup waits for a permit before it is refused (#120).
pub const KEY_LOOKUP_WAIT: Duration = Duration::from_secs(2);

/// Caps how many SDK key lookups hold a pool connection at once (#120).
#[derive(Debug)]
pub struct KeyLookups { permits: Arc<Semaphore>, size: usize, wait: Duration }

impl KeyLookups {
    pub fn new(permits: usize, wait: Duration) -> Self;      // permits >= 1
    pub fn for_pool(max_connections: u32) -> Self;           // max(1, (max - 1) / 2), KEY_LOOKUP_WAIT
    pub fn permits(&self) -> usize;                          // the cap
    pub fn available(&self) -> usize;                        // free permits, for tests
}

pub async fn resolve_cached(db: &Db, unknown: &UnknownKeys, limit: &KeyLookups, key: &str)
    -> Result<Option<KeyOwner>>;
```

`resolve_cached`: malformed key → `Ok(None)`; cached unknown hash → `Ok(None)`; else acquire an
owned permit within `limit.wait` (timeout → `Err(Error::Busy)`), then run the lookup in a
`tokio::spawn`ed task that owns the permit, a clone of `db`, and the hash, and drops the permit
only after `resolve_hash` returns. Awaiting the `JoinHandle` gives the answer; if the request is
dropped, the task still finishes and only then frees the permit. A `JoinError` (the task
panicked) is re-raised as a panic, as it would have been inline. `Ok(None)` is cached as today.

`resolve_hash`: acquires a `PoolConnection` explicitly, runs the unpinned transaction on it
(`unpinned::begin_on`), and awaits `PoolConnection::return_to_pool()` before returning, so the
release ping is inside the caller's await (and inside the permit). Same six round trips.

`crates/flags-core/src/unpinned.rs`: the probe moves into a helper taking a `Transaction<'c, _>`;
`begin_as_tenant_role` calls it on `db.pool().begin()` as today, and a new
`begin_on(db, &'c mut PgConnection)` calls it on `conn.begin()`. Same SQL, same checks.

`crates/flags-core/src/error.rs`: `Error::Busy` — "too many SDK key lookups are in progress;
retry shortly", code `busy`, `retriable` and `is_transient` true, `is_internal` false.

`crates/flags-api`: `AppState` gains `pub key_lookups: Arc<KeyLookups>`, built in `AppState::new`
from `db.pool().options().get_max_connections()` (signature unchanged). The `Sdk` extractor
passes it. `ApiError::from(Error::Busy)` → `503`, `busy`, "the flag service is busy; serve
cached or default values and retry", with the `Retry-After: 5` every `503` already carries. It is
logged at most once a second at `warn` (a flood must not flood the log), never per request.

Public interface: additive. Rust API (`KeyLookups`, `KEY_LOOKUP_WAIT`, `Error::Busy`, an
`AppState` field, `resolve_cached`'s new parameter, all workspace-internal). Wire: a new `503`
cause with code `busy`, a status SDKs already handle; nothing else changes.

## 7. Invariants impact

- **The database is small and shared:** the point of the change; the lookup's six round trips are
  unchanged and now bounded in concurrency.
- **SDK REST contract stable:** additive; `503` + `Retry-After` was already a documented outcome
  of a database failure. The guide's status table gains a `503` row.
- **SDKs fall back on errors:** every SDK treats `5xx` as "serve cached or default".
- **Client keys are public / stored hashed:** only hashes are held; nothing new is stored.
- **SDK path never calls the platform; no SQL outside flags-core; runtime queries only:**
  unchanged (the SQL text is identical).
- **Tenant isolation:** the same probe runs on the borrowed connection; the RLS refusal is
  unchanged and still tested by `unpinned` tests.
- No migration, no config key, no secret, no scope.

## 8. Migration

None.

## 9. Out-of-band

None.

## 10. Error handling and edge cases

- Permit wait timeout → `Busy` → `503`. Not cached.
- Pool acquire timeout inside the permit → `Db(PoolTimedOut)` → `503 temporarily_unavailable`, as
  today.
- Request dropped while waiting for a permit: nothing held. Dropped mid-lookup: the spawned task
  finishes and releases the permit after the connection is back.
- Runtime shutdown with tasks in flight: tasks are dropped with the runtime, as every other task.
- A failing lookup's transaction is rolled back when dropped, and `return_to_pool` closes a
  connection that is no longer usable; both happen before the permit is freed.

## 11. Testing approach

Gates per the section (test DB on 15451), clippy, fmt, migrations check, podman build.

- `crates/flags-api/tests/key_lookups.rs` (new binary): a pool of 5 on the test database;
  `AppState` with `KeyLookups::new(2, 300 ms)`; a separate connection takes
  `LOCK TABLE public.app_keys IN ACCESS EXCLUSIVE MODE` in an open transaction, so every lookup
  blocks in the database holding its connection. Send 20 requests with distinct random keys.
  Wait until both permits are taken and Postgres shows two lookups waiting on the lock; then
  acquire 3 connections from the pool within 1 s (the AC: at least one free). The other 18
  requests answer `503`, `Retry-After: 5`, `"error": "busy"` while the lock is held; after the
  lock is released the 2 in flight answer `401`. A second test: a request dropped mid-lookup does
  not free its permit until the lookup finishes.
- `keys.rs` unit test: `KeyLookups::for_pool` sizing (2→1, 5→2, 10→4).
- `crates/flags-core/tests/key_round_trips.rs`: unchanged expectations (6 / 0), updated call
  signature. `db.rs` revocation tests: updated signature.
- `unpinned` tests continue to cover the probe; one added for `begin_on`.

## 12. Risks and open questions

- Under a sustained flood larger than what two lookups can clear, valid keys queue behind bogus
  ones and may get `503` too. That is the intended trade: the SDKs serve cached or default values
  and the rest of the service (evaluation for already-authenticated work, MCP, `/readyz`,
  background) keeps working. A per-client limit would be the next step if this proves too coarse.
- Waiting requests hold a socket and a small future each, up to 2 s; hyper bounds connections,
  not this code.
