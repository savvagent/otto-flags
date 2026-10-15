# Unknown SDK keys stop reaching the database

**Status:** IMPLEMENTED (#121)

The code is authoritative where this sketch differs in detail (field names, visibility). As
built after review: the memory bound is roughly 400 KiB counting the map's buckets, not 256 KiB;
a saturated cache logs a warning at most once a second; `crates/flags-api/tests/unknown_keys.rs`
checks that the extractor uses the cache (the second request is refused with the database
closed); and `errors_are_not_remembered` pins that an error is never cached.
**Issue:** #104

## 1. Brief

From #104 (found by the security review of #100):

> Every `/api/...` SDK request resolves its key (`flags_core::keys::resolve`) before anything has
> been proven about it, and `looks_like_key` is the only gate: anyone can send `sdk_` plus 64
> random hex characters. Since #100 that lookup runs as the tenant role in its own transaction:
> BEGIN, the role probe and switch, the lookup, and COMMIT, which is four round trips instead of
> one. The server has no rate limit, and the pool has 5 connections with a 5 s acquire timeout. So
> a flood of well-formed bogus keys holds pool connections about four times longer than before,
> and real SDK traffic, MCP tools, `/readyz`, and the background work all wait longer for a
> connection.
>
> The public `sdk_` client key already lets anyone drive the full `begin_live` evaluate path,
> which costs more than this lookup. That is why #100 accepted the extra round trips. This issue
> is about bounding unauthenticated database work on the SDK path in general.

Acceptance criteria (#104):

- Repeated lookups of a key that does not resolve stop reaching the database for a short window,
  through a small bounded in-process negative cache keyed by the key hash, or through a
  per-client limit on the SDK API.
- Revocation semantics are unchanged: a rotated key or a deleted org's key stops resolving no
  later than it does today.
- A test pins how many statements the key-resolution path runs, so the count cannot grow
  unnoticed.

Related: #79 (otto-tenant's own role probe could remove a round trip).

Caller constraints: a small bounded in-process negative cache keyed by key hash (or per-client
limit); revocation unchanged; a test pins the statement count; the database is small and shared;
no unbounded memory.

## 2. Premise corrections

- Since #116 the probe statement also pins `search_path`, folded into the same statement, so the
  lookup is still four statements: `BEGIN`, the probe (role checks, role switch, search path),
  the `app_keys`/`deleted_orgs` lookup, and `COMMIT`. On the production pool (sqlx defaults plus
  a 5 s acquire timeout, `crates/flags-server/src/main.rs:32`) the connection costs two more:
  `test_before_acquire` pings an idle connection before handing it out, and sqlx pings it again
  before returning it to the pool (`sqlx-core-0.8.6/src/pool/connection.rs`, `return_to_pool`).
  The connection is held for all six. The test counts round trips on the wire, so it pins all
  six; it does not change the pool settings (a separate trade-off: the pings are what notice a
  connection Postgres closed).
- A negative cache bounds *repeated* bogus keys (a misconfigured or revoked SDK still polling, a
  replayed key, a scanner retrying). It does not bound a flood of *distinct* random keys: each one
  is new and still costs one lookup. The AC accepts the cache alone; the distinct-key flood is the
  per-client-limit half of the AC's "or", recorded in section 12 as a follow-up rather than built
  here.

## 3. Assumptions

- **Cache, not a per-client limit.** The cache needs no client identity. Behind Fly's proxy the
  client address comes from a header (`Fly-Client-IP`), and trusting that is its own design
  question. The cache also needs no tuning per customer, and it cannot refuse legitimate traffic,
  because it only ever short-circuits keys the database already said do not exist.
- **Only "not found" is cached; positive results and errors never are.** That is what keeps
  revocation unchanged: a rotated key or a deleted org's key still hits the database on every
  request, exactly as today. A database error is not an answer about the key, so it is never
  cached.
- **A key that did not resolve cannot start resolving while it is cached.** Keys are 256 random
  bits minted in `NewKey::mint` and are stored, hashed, in the same transaction that returns them
  (`create_app`, `rotate_keys`), so no client can present a key before it exists. Once a key
  stops resolving (rotated away, org deleted) nothing makes it resolve again: `app_keys` rows are
  only inserted at minting (`apps.rs`) or deleted (rotation, purge), and `deleted_orgs` is only
  inserted into (`platform_events.rs`); nothing restores a key or un-tombstones an org. A future
  feature that did (an org restore, a key import) would have to clear the cache. A TTL still bounds
  the window, so a cache entry can never outlive its usefulness by much.
- **TTL 60 s, capacity 4096 entries.** One entry is a 32-byte SHA-256 hash and an `Instant`,
  about 48 bytes of payload, roughly 400 KiB for a full cache counting the map's buckets. When it is full,
  expired entries are swept (at most once a second), and if it is still full the new key is not
  cached: the lookup still answers correctly, it just reaches the database.
- **Keyed by SHA-256 of the key** (`keys::hash`, the same hash `app_keys` stores), so the process
  never holds a presented key in clear beyond the request.
- **Where it lives:** the cache type and the cached resolver are in `flags_core::keys` (no SQL,
  just the cache in front of `resolve_hash`). `flags_api::AppState` owns one, shared by every
  request. `AppState::new` creates it, so its signature is unchanged.
- Run Profile values were all read from the section. Assumed: local DB tests run on a dedicated
  container (`DATABASE_URL=postgres://flags:flags@localhost:15448/otto_flags`) instead of the
  section's 15434, so parallel worktrees do not drop each other's test databases.

## 4. Goal and success criteria

Repeated requests with a key that does not resolve cost no database work for a minute, with
memory bounded and revocation unchanged.

- A second request with the same unknown key within 60 s runs zero statements.
- A known key runs the four statements on every request, cached or not.
- A rotated key and a deleted org's key stop resolving on the next request after the change, as
  today.
- The cache holds at most 4096 entries.
- A test pins the round trips on the wire: 6 for a lookup on a production-shaped pool (the
  acquire and release pings plus the four statements), 0 for a cached unknown key.

## 5. Scope

In: `crates/flags-core/src/keys.rs` (cache and cached resolver), `crates/flags-api/src/lib.rs`
(`AppState` field), `crates/flags-api/src/sdk.rs` (the extractor uses the cached resolver), and
tests.

Out:
- A per-client or global rate limit on the SDK API (follow-up issue, section 12).
- Positive caching of resolved keys (would change revocation).
- The stream's periodic re-check (`stream.rs` `still_valid`), which re-checks a key that resolved
  and must keep hitting the database to notice revocation.
- Reducing the four statements (#79).

## 6. Architecture

`crates/flags-core/src/keys.rs`:

```rust
/// How long a key that did not resolve is answered from memory (#104).
pub const UNKNOWN_KEY_TTL: Duration = Duration::from_secs(60);
/// The most unknown keys remembered at once (about 256 KiB).
pub const UNKNOWN_KEY_CAPACITY: usize = 4096;

/// Recently presented keys that did not resolve, by hash. …
#[derive(Debug, Default)]
pub struct UnknownKeys { inner: std::sync::Mutex<Inner> }
struct Inner { seen: HashMap<[u8; 32], Instant>, next_sweep: Option<Instant> }

impl UnknownKeys {
    pub fn new() -> Self;
    fn contains(&self, hash: &[u8; 32], now: Instant) -> bool;   // expired → removed, false
    fn insert(&self, hash: [u8; 32], now: Instant);              // bounded, see section 3
    pub fn len(&self) -> usize;                                  // for tests and metrics
}

/// [`resolve`], answering a key that did not resolve in the last
/// [`UNKNOWN_KEY_TTL`] from `unknown` without touching the database.
pub async fn resolve_cached(db: &Db, unknown: &UnknownKeys, key: &str) -> Result<Option<KeyOwner>>;
```

`resolve_cached`: if `!looks_like_key(key)` → `Ok(None)` (no hash, no cache entry, as today).
Otherwise hash once; a cached hash → `Ok(None)`; else `resolve_hash(db, &hash)`; on `Ok(None)`
insert the hash; return the result (an `Err` is returned unchanged and not cached). The mutex is
`std::sync::Mutex`, never held across an `.await`. A poisoned mutex is recovered
(`into_inner`), since the map is only a cache and every state of it is valid.

`crates/flags-api/src/lib.rs`: `AppState` gains
`pub unknown_keys: std::sync::Arc<flags_core::keys::UnknownKeys>`, created in `AppState::new`.
Additive: the struct is built only through `new` (flags-server, the failure_logs test).

`crates/flags-api/src/sdk.rs`: the `Sdk` extractor calls
`keys::resolve_cached(&state.db, &state.unknown_keys, key)`.

Public interface: additive Rust API only (`UnknownKeys`, `resolve_cached`, two constants, one
`AppState` field). No wire, REST, MCP, or SDK change: an unknown key is still `401` with the
same body.

## 7. Invariants impact

- **Client keys are public; stored as SHA-256 hashes:** the cache holds only hashes.
- **The SDK path never calls the platform:** unchanged.
- **The database is small and shared:** fewer statements under repeated bad keys; no change for
  known keys; memory bounded at 4096 entries.
- **SDK REST contract stable:** an unknown key is still `401`.
- **Tenant isolation:** unchanged; the lookup itself is the same `unpinned` transaction.
- No migration, no SQL change, no SQL outside flags-core.

## 8. Migration

None.

## 9. Out-of-band

None: no config key (TTL and capacity are constants), no secret, no scope.

## 10. Error handling and edge cases

- Database error during lookup: returned as today (503), not cached.
- Malformed key: rejected before hashing, never cached (it cannot reach the database anyway).
- Cache full of live entries: the new unknown key is looked up and not cached; correct answer,
  bounded memory.
- Clock: `std::time::Instant` (monotonic). Unit tests pass `now` explicitly.
- Multiple machines: each has its own cache; a key unknown on one is unknown on all, so the
  caches never disagree with the database about anything but freshness of a "no".

## 11. Testing approach

Gates: `DATABASE_URL=postgres://flags:flags@localhost:15434/otto_flags cargo test --workspace`
(run here on the dedicated 15448 container), clippy, fmt, the migrations check, and the podman
build.

- Unit tests in `keys.rs`: contains after insert; expiry at `now + TTL`; capacity bound (fill to
  capacity, one more is not inserted while all are live, is inserted after they expire and a
  sweep runs); sweep throttled to once a second.
- `crates/flags-core/tests/key_round_trips.rs`, its own binary: a small counting TCP proxy in
  front of the test database counts the client's round trips on the wire, every simple `Query`
  ('Q') and every extended-protocol `Sync` ('S') message after startup. It frames the client's
  stream itself (type byte, 4-byte length, body, read with `read_exact`, so a message split
  across TCP reads is counted once) and passes the untyped startup packet through uncounted;
  TLS is off, so there is no SSLRequest. sqlx's own query log
  cannot be used: it does not log `BEGIN` (sent with `queue_simple_query`) or the acquire ping.
  The pool is production-shaped (sqlx defaults, one connection, TLS off for the proxy). Each
  case is measured warm, after one uncounted call, so the per-connection prepared-statement
  cache holds the statements, as it does in production (a cold call costs one more round trip
  per prepared statement: Parse/Describe/Sync, then Bind/Execute/Sync). sqlx returns a
  connection, and sends its release ping, in a spawned task after the caller has its answer, so
  each measurement first waits until the pool's connection is idle again
  (`num_idle() == size()`), never by acquiring (which would add a ping). Exact expectations
  (verified on the prototype: 6):
  - `resolve` of an unknown key: 6; of a known key: 6.
  - `resolve_cached` of an unknown key: 6, then 0 for the same key; of a known key: 6 and 6.
  A change that adds a statement, or drops the acquire ping, fails the test and must update the
  pinned count deliberately. No new dependency (tokio is already a dev-dependency).
- `crates/flags-core/tests/db.rs`: revocation through the cache: a key resolves via
  `resolve_cached`, is rotated away, and the next `resolve_cached` returns `None`; a key whose org
  is tombstoned stops resolving on the next call; after `None` the key is in the cache.
- `crates/flags-server/tests/e2e.rs` is unchanged; existing SDK tests cover the extractor.
  `crates/flags-api/tests/unknown_keys.rs` (added in review) shows the extractor goes through
  the cache.

## 12. Risks and open questions

- **Distinct random keys still cost one lookup each.** Follow-up #120 proposes a per-client or
  global concurrency limit on SDK key lookups (for example a semaphore capping lookups below the
  pool size, so a flood cannot take every connection), with the `Fly-Client-IP` trust question.
- **A distinct-key flood fills the cache.** 4096 random keys within one TTL fill it; until
  entries expire, nothing new is cached, including a revoked SDK that keeps polling, which then
  costs a lookup per request, as it does today. Never worse than today.
- `keys::hash` returns `Vec<u8>`; the cache key is that hash converted to `[u8; 32]`
  (`try_into`, always 32 bytes from SHA-256), and the lookup passes the same bytes to
  `resolve_hash`.
- Each machine's cache is independent; with `min_machines_running = 0` it empties when the
  machine stops. Harmless.
