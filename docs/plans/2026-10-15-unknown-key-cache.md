# Unknown SDK key cache Implementation Plan

> Execute task-by-task (fresh implementer per task, review between tasks). Steps use checkbox
> (`- [ ]`) syntax for tracking.

**Goal:** Repeated SDK requests with a key that does not resolve stop reaching the database for
60 s, with bounded memory and revocation unchanged, and a test pins the lookup's round trips
(#104).

**Architecture:** `flags_core::keys::UnknownKeys`, a mutex-guarded map of SHA-256 key hashes to
expiry, at most 4096 entries; `keys::resolve_cached` puts it in front of `resolve_hash` and
caches only `Ok(None)`. `flags_api::AppState` owns one, and the `Sdk` extractor uses it. A
counting TCP proxy test pins round trips on the wire.

**Tech Stack:** Rust, sqlx 0.8 (runtime queries), tokio, Postgres 16.

**Ticket:** savvagent/otto-flags#104

**Spec:** `docs/specs/2026-10-15-unknown-key-cache-design.md`

## As built

- Tasks 1 and 2 landed as one commit: Task 1's `pub(crate)` cache methods have no caller until
  `resolve_cached`, so Task 1 alone fails clippy's dead-code lint.
- The sweep-throttle unit test was rewritten so it fails without the throttle (entries expire
  inside the second after a sweep).
- Task 3 gained `crates/flags-api/tests/unknown_keys.rs`, red first (no `unknown_keys` field),
  which closes the pool before the second request so only the cache can answer it.
- Review added `errors_are_not_remembered`, a saturation warning, a single `digest` definition,
  and a deadline on the round-trip test's idle wait.

## Global Constraints

- Gates, from `/home/robhicks/dev/otto-flags/.claude/worktrees/fix-104-bound-key-lookups`, with
  `export CARGO_BUILD_JOBS=3; mkdir -p target/tmp; export TMPDIR=$PWD/target/tmp`:
  - test: `DATABASE_URL=postgres://flags:flags@localhost:15448/otto_flags cargo test --workspace`
    (dedicated container; never 15434 or a Fly database)
  - lint: `cargo clippy --workspace --all-targets -- -D warnings`
  - format: `cargo fmt --all -- --check`
  - extra: `git diff --name-status --diff-filter=MDR origin/main...HEAD -- 'crates/*/migrations/*'` prints nothing
  - `podman build -t otto-flags:check .` (crates changed; final run)
- Evidence: pass counts per test binary; a `DATABASE_URL must be set` panic is a missing
  variable; a sqlx-core `testing/mod.rs` setup panic is environmental, re-run first.
- Invariants: client keys are public and only their SHA-256 hashes are held; the SDK path never
  calls the platform; the SDK REST contract is unchanged (unknown key → 401); the database is
  small and shared (no unbounded memory); no SQL change, no SQL outside flags-core; no migration.
- Revocation: never cache a positive result or an error.
- Out-of-band: none.
- Worktree only; trunk changes only through the PR.
- Commit format: `<type>(<scope>): <imperative, lower-case>`, body explains why, ends
  `Fixes #104`; no attribution of any kind (no Co-Authored-By, no "Generated with", no credit
  markers).

## File Structure

| File | Responsibility |
|---|---|
| **Modify.** `crates/flags-core/src/keys.rs` | `UNKNOWN_KEY_TTL`, `UNKNOWN_KEY_CAPACITY`, `UnknownKeys`, `resolve_cached`; unit tests |
| **Create.** `crates/flags-core/tests/key_round_trips.rs` | Counting proxy; round-trip pins for `resolve` and `resolve_cached` |
| **Modify.** `crates/flags-core/tests/db.rs` | Revocation through the cache |
| **Modify.** `crates/flags-api/src/lib.rs` | `AppState.unknown_keys` |
| **Modify.** `crates/flags-api/src/sdk.rs` | Extractor uses `resolve_cached` |

## Task Order & Rationale

The cache (Task 1) is pure logic, tested alone. Task 2 adds the resolver and the DB-backed pins
that depend on it. Task 3 wires it into the API.

---

### Task 1: The bounded unknown-key cache

**Files:** Modify `crates/flags-core/src/keys.rs` (new items after `resolve_hash`; unit tests in
its `mod tests`).

**Interfaces:** produces `UnknownKeys::{new, len, is_empty}` (pub) and
`UnknownKeys::{contains, insert}` (`pub(crate)`, taking `now: Instant`), the two constants.

**Out-of-band / invariants:** holds only hashes; bounded at `UNKNOWN_KEY_CAPACITY`.

**Model:** sonnet.

- [x] **Step 1: Failing unit tests** in `keys.rs` `mod tests`:

```rust
    use std::time::{Duration, Instant};

    fn h(n: u32) -> [u8; 32] {
        let mut out = [0u8; 32];
        out[..4].copy_from_slice(&n.to_be_bytes());
        out
    }

    #[test]
    fn an_unknown_key_is_remembered_until_its_ttl() {
        let cache = UnknownKeys::new();
        let t0 = Instant::now();
        assert!(!cache.contains(&h(1), t0));
        cache.insert(h(1), t0);
        assert!(cache.contains(&h(1), t0 + UNKNOWN_KEY_TTL - Duration::from_millis(1)));
        assert!(!cache.contains(&h(1), t0 + UNKNOWN_KEY_TTL));
        assert_eq!(cache.len(), 0, "an expired entry is dropped when looked up");
    }

    #[test]
    fn the_cache_never_grows_past_its_capacity() {
        let cache = UnknownKeys::new();
        let t0 = Instant::now();
        for n in 0..UNKNOWN_KEY_CAPACITY as u32 {
            cache.insert(h(n), t0);
        }
        cache.insert(h(u32::MAX), t0 + Duration::from_secs(1));
        assert_eq!(cache.len(), UNKNOWN_KEY_CAPACITY);
        assert!(!cache.contains(&h(u32::MAX), t0 + Duration::from_secs(1)));
        // Once the old entries expire, a sweep makes room.
        let later = t0 + UNKNOWN_KEY_TTL + Duration::from_secs(2);
        cache.insert(h(u32::MAX), later);
        assert!(cache.contains(&h(u32::MAX), later));
        assert_eq!(cache.len(), 1);
    }

    #[test]
    fn a_full_cache_sweeps_at_most_once_a_second() {
        let cache = UnknownKeys::new();
        let t0 = Instant::now();
        for n in 0..UNKNOWN_KEY_CAPACITY as u32 {
            cache.insert(h(n), t0);
        }
        let expired = t0 + UNKNOWN_KEY_TTL;
        cache.insert(h(u32::MAX), expired); // sweeps: everything expired
        assert_eq!(cache.len(), 1);
        for n in 0..(UNKNOWN_KEY_CAPACITY - 1) as u32 {
            cache.insert(h(n), expired);
        }
        // Full of live entries; the sweep just ran, so none runs now, and the
        // key is not inserted.
        cache.insert(h(u32::MAX - 1), expired + Duration::from_millis(10));
        assert_eq!(cache.len(), UNKNOWN_KEY_CAPACITY);
    }
```

- [x] **Step 2: Run, see them fail to compile**
  `cargo test -p flags-core --lib keys::tests` → errors: `UnknownKeys` not found.

- [x] **Step 3: Implement** (after `resolve_hash`; add `use std::collections::HashMap;`,
  `use std::sync::Mutex;`, `use std::time::{Duration, Instant};`):

```rust
/// How long a key that did not resolve is answered from memory (#104).
pub const UNKNOWN_KEY_TTL: Duration = Duration::from_secs(60);

/// The most unknown keys remembered at once: a 32-byte hash and an
/// `Instant` each, about 256 KiB when full.
pub const UNKNOWN_KEY_CAPACITY: usize = 4096;

/// How often a full cache may scan for expired entries.
const SWEEP_EVERY: Duration = Duration::from_secs(1);

/// SDK keys presented recently that did not resolve, by SHA-256 hash, so a
/// client repeating one stops costing a database lookup per request (#104).
///
/// Only "not found" is remembered. A key that resolved is looked up again on
/// every request, so rotating it or deleting its org takes effect at once,
/// exactly as without the cache; an error is not an answer and is not
/// remembered. A key that did not resolve cannot start resolving: keys are
/// random and stored in the transaction that mints them, `app_keys` rows are
/// never restored, and `deleted_orgs` tombstones are never removed. A feature
/// that restored either would have to clear this cache.
///
/// Bounded at [`UNKNOWN_KEY_CAPACITY`]: when full, expired entries are swept
/// (at most every second) and, if it is still full, a new key is simply not
/// remembered: its lookup still answers correctly.
#[derive(Debug, Default)]
pub struct UnknownKeys {
    inner: Mutex<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    /// Hash → when it expires.
    expires: HashMap<[u8; 32], Instant>,
    /// Before this, a full cache does not sweep again.
    next_sweep: Option<Instant>,
}

impl UnknownKeys {
    pub fn new() -> Self {
        Self::default()
    }

    /// Entries held, live or not yet swept.
    pub fn len(&self) -> usize {
        self.lock().expires.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The map is only a cache and every state of it is valid, so a panic
    /// while it was held (it holds no await) loses nothing worth refusing for.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether `hash` did not resolve within the last TTL as of `now`.
    pub(crate) fn contains(&self, hash: &[u8; 32], now: Instant) -> bool {
        let mut inner = self.lock();
        match inner.expires.get(hash) {
            Some(&until) if now < until => true,
            Some(_) => {
                inner.expires.remove(hash);
                false
            }
            None => false,
        }
    }

    /// Remember that `hash` did not resolve, as of `now`.
    pub(crate) fn insert(&self, hash: [u8; 32], now: Instant) {
        let mut inner = self.lock();
        if inner.expires.len() >= UNKNOWN_KEY_CAPACITY && !inner.expires.contains_key(&hash) {
            if inner.next_sweep.is_some_and(|at| now < at) {
                return;
            }
            inner.expires.retain(|_, until| now < *until);
            inner.next_sweep = Some(now + SWEEP_EVERY);
            if inner.expires.len() >= UNKNOWN_KEY_CAPACITY {
                return;
            }
        }
        inner.expires.insert(hash, now + UNKNOWN_KEY_TTL);
    }
}
```

- [x] **Step 4: Run, see them pass** — `cargo test -p flags-core --lib keys::tests` (no DB
  needed), then fmt and clippy.

- [x] **Step 5: Commit** — `feat(core): remember sdk keys that do not resolve, bounded` with a
  why-body ending `Fixes #104`.

### Task 2: The cached resolver and its round-trip pins

**Files:** Modify `crates/flags-core/src/keys.rs`; Create
`crates/flags-core/tests/key_round_trips.rs`; Modify `crates/flags-core/tests/db.rs`.

**Interfaces:** consumes Task 1; produces
`pub async fn resolve_cached(db: &Db, unknown: &UnknownKeys, key: &str) -> Result<Option<KeyOwner>>`.

**Out-of-band / invariants:** never cache `Some` or `Err`.

**Model:** sonnet.

- [x] **Step 1: Failing tests.**
  - `key_round_trips.rs`: the counting proxy (frames the client stream by type byte and 4-byte
    length after the untyped startup packet; counts 'Q' and 'S'), a production-shaped
    one-connection pool through it (`ssl_mode(Disable)`, `acquire_timeout(5 s)`, upstream
    address preferring IPv4), `RoundTrips::take(&db)` which first waits until
    `num_idle() == size()`, and two tests:
    - `resolving_a_key_takes_six_round_trips`: for a known and an unknown key, one warm-up
      `resolve`, then a measured `resolve` = 6.
    - `a_cached_unknown_key_takes_no_round_trips`: with one `UnknownKeys`: unknown key —
      warm-up `resolve` (uncached), then `resolve_cached` = 6 and returns `None`, then
      `resolve_cached` again = 0 and `None`; known key — `resolve_cached` twice, 6 and 6, both
      `Some`; `unknown.len() == 1`.
    The file's module doc explains the counting and why sqlx's own log undercounts.
  - `db.rs`:
    - `rotation_stops_a_key_through_the_cache`: create an app (client key), `resolve_cached`
      → `Some`; `rotate_keys(&app, Rotate::Client, actor)` in a pinned transaction, commit;
      `resolve_cached(old)` → `None`; `resolve_cached(new)` → `Some`; `unknown.len() == 1`.
    - `a_deleted_orgs_key_stops_through_the_cache`: `resolve_cached` → `Some`; insert the org
      into `deleted_orgs` (superuser pool); `resolve_cached` → `None`.
    - `malformed_keys_are_not_remembered`: `resolve_cached(db, &unknown, "sdk_xyz")` → `None`
      and `unknown.is_empty()`.
- [x] **Step 2: Run, see them fail to compile** (`resolve_cached` missing):
  `DATABASE_URL=postgres://flags:flags@localhost:15448/otto_flags cargo test -p flags-core --test key_round_trips --test db`.
- [x] **Step 3: Implement** in `keys.rs`:

```rust
/// [`resolve`], answering a key that did not resolve within the last
/// [`UNKNOWN_KEY_TTL`] from `unknown` instead of the database (#104). Only
/// `Ok(None)` is remembered: a key that resolves, and an error, go to the
/// database every time, so revocation is exactly as prompt as [`resolve`]'s.
pub async fn resolve_cached(
    db: &Db,
    unknown: &UnknownKeys,
    key: &str,
) -> Result<Option<KeyOwner>> {
    if !looks_like_key(key) {
        return Ok(None);
    }
    let hash: [u8; 32] = Sha256::digest(key.as_bytes()).into();
    if unknown.contains(&hash, Instant::now()) {
        return Ok(None);
    }
    let owner = resolve_hash(db, &hash).await?;
    if owner.is_none() {
        unknown.insert(hash, Instant::now());
    }
    Ok(owner)
}
```

- [x] **Step 4: Run, see them pass** (same command; run `key_round_trips` three times to check
  it is not flaky), then fmt and clippy.
- [x] **Step 5: Commit** — `feat(core): resolve sdk keys through the unknown-key cache` with a
  why-body ending `Fixes #104`.

### Task 3: Use the cache on the SDK API

**Files:** Modify `crates/flags-api/src/lib.rs`, `crates/flags-api/src/sdk.rs`.

**Interfaces:** consumes `UnknownKeys`, `resolve_cached`.

**Out-of-band / invariants:** unknown key still `401`; the stream re-check (`stream.rs`
`still_valid`) keeps `resolve_hash`, uncached.

**Model:** haiku — mechanical wiring.

- [x] **Step 1:** `AppState` gains, with a doc comment,
  `pub unknown_keys: std::sync::Arc<flags_core::keys::UnknownKeys>` (keys that did not
  resolve, shared by every request; see `keys::UnknownKeys`), set to
  `Arc::new(UnknownKeys::new())` in `AppState::new`.
- [x] **Step 2:** in `sdk.rs` the extractor calls
  `keys::resolve_cached(&state.db, &state.unknown_keys, key)`.
- [x] **Step 3:** full gates (Global Constraints), including `podman build`. The existing
  `crates/flags-server/tests/e2e.rs` SDK tests (unknown key → 401, rotation) must still pass.
- [x] **Step 4: Commit** — `feat(api): answer repeated unknown sdk keys from memory`, body
  ending `Fixes #104`.
