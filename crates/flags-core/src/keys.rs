//! SDK credentials: minting, hashing, and resolving them to an app.
//!
//! Two kinds, matching what the SDKs in `packages/` already send:
//!
//! | Kind | Prefix | Where it lives |
//! |---|---|---|
//! | client | `sdk_` | Browser and mobile apps. Public by design. |
//! | server | `srv_` | Backend services. A secret. |
//!
//! Only a SHA-256 of each key is stored (`app_keys`), so a database read does
//! not yield a usable server key. The key is high-entropy random, so a plain
//! hash is enough; there is nothing for a slow KDF to protect.
//!
//! Resolution runs unpinned, as the tenant role (`crate::unpinned`), because
//! the org is exactly what the key is being resolved to. `app_keys` is outside
//! row-level security for that reason, and every statement here names the key
//! hash explicitly.
//!
//! The SDK API's request extractor resolves keys through [`resolve_cached`],
//! which remembers keys that did not resolve for [`UNKNOWN_KEY_TTL`]
//! ([`UnknownKeys`]), so a client repeating one stops costing a database
//! lookup per request (#104). Behind that cache, [`KeyLookups`] caps how many
//! lookups hold a pool connection at once, so a flood of distinct well-formed
//! unknown keys cannot take the whole small pool; a lookup that cannot start
//! within [`KEY_LOOKUP_WAIT`] is refused with [`Error::Busy`] (#120). One that
//! starts runs under [`LookupBounds`], so a lock on `app_keys`, a slow
//! statement, or a dead connection cannot hold its permit for long (#130). The
//! stream's periodic re-check of a key that resolved stays on [`resolve_hash`],
//! ungated and uncached (under [`LookupBounds::DEFAULT`]), so it notices
//! revocation and a flood cannot end an authenticated stream.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use otto_tenant::ids::OrgId;
use otto_tenant::Db;
use rand::RngCore;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;

use crate::error::{Error, Result};
use crate::ids::FlagAppId;
use crate::unpinned::{Held, Timeouts};

pub const CLIENT_PREFIX: &str = "sdk_";
pub const SERVER_PREFIX: &str = "srv_";

/// How many characters of a key are kept in clear, to tell keys apart.
const SHOWN_PREFIX_LEN: usize = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum KeyKind {
    Client,
    Server,
}

impl KeyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyKind::Client => "client",
            KeyKind::Server => "server",
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            KeyKind::Client => CLIENT_PREFIX,
            KeyKind::Server => SERVER_PREFIX,
        }
    }

    fn parse(s: &str) -> Option<Self> {
        match s {
            "client" => Some(KeyKind::Client),
            "server" => Some(KeyKind::Server),
            _ => None,
        }
    }
}

/// A freshly minted key, in clear. Exists only between minting and the
/// response that shows it.
#[derive(Debug, Clone)]
pub struct NewKey {
    pub kind: KeyKind,
    pub key: String,
}

impl NewKey {
    pub fn mint(kind: KeyKind) -> Self {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        Self {
            kind,
            key: format!("{}{}", kind.prefix(), hex::encode(bytes)),
        }
    }

    pub fn hash(&self) -> Vec<u8> {
        hash(&self.key)
    }

    pub fn shown_prefix(&self) -> String {
        self.key.chars().take(SHOWN_PREFIX_LEN).collect()
    }
}

pub fn hash(key: &str) -> Vec<u8> {
    digest(key).to_vec()
}

/// The SHA-256 of `key`: what `app_keys` stores, and what [`UnknownKeys`]
/// remembers.
fn digest(key: &str) -> [u8; 32] {
    Sha256::digest(key.as_bytes()).into()
}

/// Whether a string is shaped like an SDK key at all. Lets the SDK surface
/// reject a misplaced OAuth token or a typo without a database round trip.
pub fn looks_like_key(s: &str) -> bool {
    (s.starts_with(CLIENT_PREFIX) || s.starts_with(SERVER_PREFIX))
        && s.len() == CLIENT_PREFIX.len() + 64
        && s[CLIENT_PREFIX.len()..]
            .bytes()
            .all(|b| b.is_ascii_hexdigit())
}

/// What a key opens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyOwner {
    pub org_id: OrgId,
    pub app_id: FlagAppId,
    pub kind: KeyKind,
}

/// Resolve a presented key, or `None` if it is unknown, revoked by rotation, or
/// belongs to an org the platform has deleted.
///
/// Ungated and uncached: a request path resolving a key it was just handed
/// uses [`resolve_cached`], which bounds how many lookups hold a connection
/// (#120).
pub async fn resolve(db: &Db, key: &str) -> Result<Option<KeyOwner>> {
    if !looks_like_key(key) {
        return Ok(None);
    }
    resolve_hash(db, &hash(key)).await
}

/// [`resolve`] by the key's hash, for re-checking a key that is no longer held
/// in clear (a long-lived stream).
///
/// Returns only once its connection is back in the pool, release ping
/// included, or closed, so a caller holding a [`KeyLookups`] permit across
/// this call covers the connection's whole checkout (#120). Runs under
/// [`LookupBounds::DEFAULT`]: a lookup that misses its transaction deadline
/// closes its connection and fails with a transient `sqlx::Error::Io`
/// (#130).
pub async fn resolve_hash(db: &Db, key_hash: &[u8]) -> Result<Option<KeyOwner>> {
    resolve_hash_within(db, key_hash, &LookupBounds::DEFAULT).await
}

/// [`resolve_hash`] under `bounds`.
async fn resolve_hash_within(
    db: &Db,
    key_hash: &[u8],
    bounds: &LookupBounds,
) -> Result<Option<KeyOwner>> {
    let mut conn = Held::acquire(db).await?;
    // No `?` and no early return between the acquire and `release` or
    // `discard`: dropping a pool connection hands its release (rolling back
    // an open transaction, the ping) to a task sqlx spawns, so this would
    // return, and its caller free its permit, while the connection is still
    // checked out. (A panic in `lookup` still takes that path; the permit is
    // then freed by the unwind.)
    //
    // Both steps are bounded on the client (#130): the server-side timeouts
    // the probe sets cannot reach a client whose connection went half-open.
    // A transaction that misses its deadline may have left the connection
    // mid-protocol, so it is closed, never returned; a release that misses
    // its deadline is cut short, which also closes it (`Held::release`).
    // Either way the connection is gone before this returns.
    match tokio::time::timeout(bounds.transaction, lookup(db, &mut conn, key_hash, bounds)).await {
        Ok(owner) => {
            if !conn.release(bounds.release).await {
                // The answer was already read: still give it.
                tracing::warn!(
                    reason = "release_timed_out",
                    within = ?bounds.release,
                    "sdk key lookup's connection did not return to the pool in time; closed it"
                );
            }
            owner
        }
        Err(_) => {
            conn.discard();
            // A fixed reason of its own: the API logs the error below as
            // `io`, the same as a broken socket.
            tracing::warn!(
                reason = "transaction_timed_out",
                within = ?bounds.transaction,
                "sdk key lookup's transaction did not finish in time; closed its connection"
            );
            Err(Error::Db(sqlx::Error::Io(
                std::io::ErrorKind::TimedOut.into(),
            )))
        }
    }
}

/// [`resolve_hash`]'s query, on the connection it holds.
async fn lookup(
    db: &Db,
    conn: &mut Held,
    key_hash: &[u8],
    bounds: &LookupBounds,
) -> Result<Option<KeyOwner>> {
    // No org yet: finding it is the point. Unpinned, as the tenant role
    // (`crate::unpinned`); `app_keys` and `deleted_orgs` are outside RLS. The
    // probe sets the server-side timeouts, so the lookup that follows gives
    // up on a lock or a slow statement (#130).
    let timeouts = Timeouts {
        lock: bounds.lock,
        statement: bounds.statement,
        idle: bounds.transaction,
    };
    let mut tx = conn.begin(db, Some(timeouts)).await?;
    let row: Option<(uuid::Uuid, uuid::Uuid, String)> = sqlx::query_as(
        "SELECT k.org_id, k.app_id, k.kind FROM app_keys k \
         WHERE k.key_hash = $1 \
           AND NOT EXISTS (SELECT 1 FROM deleted_orgs d WHERE d.org_id = k.org_id)",
    )
    .bind(key_hash)
    .fetch_optional(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(row.and_then(|(org, app, kind)| {
        Some(KeyOwner {
            org_id: org.into(),
            app_id: app.into(),
            kind: KeyKind::parse(&kind)?,
        })
    }))
}

/// [`resolve`], answering a key that did not resolve within the last
/// [`UNKNOWN_KEY_TTL`] from `unknown` instead of the database (#104), and
/// otherwise looking it up under one of `limit`'s permits (#120). Only
/// `Ok(None)` is remembered: a key that resolves, and an error, go to the
/// database every time, so revocation is exactly as prompt as [`resolve`]'s.
///
/// A malformed key and a remembered one take no permit. Any other waits up to
/// the limit's wait for one, then fails with [`Error::Busy`]. The lookup runs
/// in a task that owns the permit and frees it only after its connection is
/// back in the pool, so a caller dropped mid-lookup (a client that hung up)
/// neither frees the permit early nor loses the answer: the task still
/// remembers an unknown key.
pub async fn resolve_cached(
    db: &Db,
    unknown: &Arc<UnknownKeys>,
    limit: &KeyLookups,
    key: &str,
) -> Result<Option<KeyOwner>> {
    if !looks_like_key(key) {
        return Ok(None);
    }
    let hash = digest(key);
    if unknown.contains(&hash, Instant::now()) {
        return Ok(None);
    }
    let permit = match tokio::time::timeout(limit.wait, limit.permits.clone().acquire_owned()).await
    {
        Ok(Ok(permit)) => permit,
        // A timeout, or a closed semaphore, which nothing here ever closes.
        Ok(Err(_)) | Err(_) => return Err(Error::Busy),
    };
    let (db, unknown, limit_bounds) = (db.clone(), Arc::clone(unknown), limit.bounds);
    let task = tokio::spawn(async move {
        let owner = resolve_hash_within(&db, &hash, &limit_bounds).await;
        if matches!(owner, Ok(None)) {
            unknown.insert(hash, Instant::now());
        }
        drop(permit);
        owner
    });
    match task.await {
        Ok(owner) => owner,
        Err(join) => match join.try_into_panic() {
            // Panic as the lookup would have inline.
            Ok(panic) => std::panic::resume_unwind(panic),
            // Cancelled: the runtime is shutting down, and the pool with it.
            // Not `Busy`, whose log line would blame the permits.
            Err(_) => Err(Error::Db(sqlx::Error::PoolClosed)),
        },
    }
}

/// How long an SDK key lookup waits for a [`KeyLookups`] permit before it is
/// refused with [`Error::Busy`] (#120). Under the pool's 5 s acquire timeout,
/// so a refused request answers before a pool timeout would.
pub const KEY_LOOKUP_WAIT: Duration = Duration::from_secs(2);

/// Caps how many SDK key lookups ([`resolve_cached`]) hold a pool connection
/// at once (#120).
///
/// SDK keys are presented before anything is proven about the caller, and
/// [`UnknownKeys`] only helps with a key that repeats: a flood of distinct
/// well-formed random keys costs one lookup each. Without a cap those lookups
/// could take every connection in the small shared pool, starving evaluation,
/// MCP tools, `/readyz`, and background work. [`KeyLookups::for_pool`] sizes
/// the cap at half of what the flag-change listener (which holds one pool
/// connection for the life of the process) leaves, so on the production pool
/// of 5 a flood holds at most 2 and 2 stay free.
///
/// A permit covers the connection's whole checkout, which takes two things.
/// sqlx returns a dropped `PoolConnection` to the pool in a task it spawns,
/// after a release ping, so a permit freed when the query returned would be
/// free while the connection is still out: [`resolve_hash`] awaits the return
/// itself. And a request future dropped mid-lookup would drop its permit at
/// once: [`resolve_cached`] runs the lookup in a task that owns the permit.
#[derive(Debug)]
pub struct KeyLookups {
    permits: Arc<Semaphore>,
    size: usize,
    wait: Duration,
    bounds: LookupBounds,
}

/// How long an SDK key lookup may take once it has its connection (#130), so
/// that neither its [`KeyLookups`] permit nor its pool connection is held for
/// as long as a lock on `app_keys` lasts or a dead connection takes to notice.
///
/// `lock` and `statement` are set on the server for the lookup's statement:
/// a lock wait fails with `lock_not_available` (55P03) and a slow statement
/// with `query_canceled` (57014), both transient (`503` on the SDK API) and
/// never remembered as an unknown key. They cannot bound a half-open
/// connection, which never hears the server's answer: `transaction` is a
/// client-side deadline on the whole transaction, `BEGIN` to `COMMIT`, and
/// `release` one on returning the connection to the pool. A connection that
/// misses either is closed, never returned. `transaction` is also the
/// server's `idle_in_transaction_session_timeout`, so a backend whose client
/// gave up ends itself rather than keep its lock on `app_keys`.
///
/// ```
/// use flags_core::keys::{KeyLookups, LookupBounds, KEY_LOOKUP_WAIT};
/// assert_eq!(KeyLookups::new(1, KEY_LOOKUP_WAIT).bounds(), LookupBounds::DEFAULT);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LookupBounds {
    /// `lock_timeout` for the lookup's statement.
    pub lock: Duration,
    /// `statement_timeout` for the lookup's statement.
    pub statement: Duration,
    /// Client-side deadline on the transaction, and the server's
    /// `idle_in_transaction_session_timeout`.
    pub transaction: Duration,
    /// Client-side deadline on returning the connection to the pool.
    pub release: Duration,
}

impl LookupBounds {
    /// Production's bounds. A healthy lookup takes a few milliseconds; with
    /// the pool's 5 s acquire timeout a permit is held at most about 8 s.
    pub const DEFAULT: Self = Self {
        lock: Duration::from_millis(500),
        statement: Duration::from_secs(1),
        transaction: Duration::from_secs(2),
        release: Duration::from_secs(1),
    };
}

impl KeyLookups {
    /// At most `permits` lookups at once, each waiting up to `wait` for its
    /// turn, under [`LookupBounds::DEFAULT`].
    ///
    /// # Panics
    ///
    /// If `permits` is 0: no lookup could ever run.
    pub fn new(permits: usize, wait: Duration) -> Self {
        assert!(permits >= 1, "a key lookup limit needs at least one permit");
        Self {
            permits: Arc::new(Semaphore::new(permits)),
            size: permits,
            wait,
            bounds: LookupBounds::DEFAULT,
        }
    }

    /// The same limit, with each lookup under `bounds` instead of
    /// [`LookupBounds::DEFAULT`].
    pub fn with_bounds(self, bounds: LookupBounds) -> Self {
        Self { bounds, ..self }
    }

    /// The bounds each lookup runs under.
    pub fn bounds(&self) -> LookupBounds {
        self.bounds
    }

    /// The limit for a pool of `max_connections`: half of what the listener's
    /// one connection leaves, rounded down, never fewer than one, with
    /// [`KEY_LOOKUP_WAIT`].
    ///
    /// ```
    /// use flags_core::keys::KeyLookups;
    /// assert_eq!(KeyLookups::for_pool(5).permits(), 2);
    /// assert_eq!(KeyLookups::for_pool(2).permits(), 1);
    /// ```
    pub fn for_pool(max_connections: u32) -> Self {
        let permits = (max_connections.saturating_sub(1) / 2).max(1);
        Self::new(permits as usize, KEY_LOOKUP_WAIT)
    }

    /// [`KeyLookups::for_pool`] for `db`'s pool.
    pub fn for_db(db: &Db) -> Self {
        Self::for_pool(crate::unpinned::max_connections(db))
    }

    /// The cap: how many lookups may hold a connection at once.
    pub fn permits(&self) -> usize {
        self.size
    }

    /// Permits free right now.
    pub fn available(&self) -> usize {
        self.permits.available_permits()
    }
}

/// How long a key that did not resolve is answered from memory (#104).
pub const UNKNOWN_KEY_TTL: Duration = Duration::from_secs(60);

/// The most unknown keys remembered at once: a 32-byte hash and an
/// `Instant` each, roughly 400 KiB when full counting the map's buckets.
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
/// remembered: its lookup still answers correctly, and a warning (at most
/// once a second, with the sweep) says the cache is saturated.
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
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Entries held, live or not yet swept.
    pub fn len(&self) -> usize {
        self.lock().expires.len()
    }

    /// Whether no entries are held.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The guard is never held across an `.await`, and every state of the
    /// map is a valid cache, so a poisoned lock is recovered rather than
    /// propagated.
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Whether `hash` did not resolve within the last TTL as of `now`. An
    /// expired entry for `hash` is removed.
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
                // At most once a second: the sweep that found it full is
                // throttled the same way.
                tracing::warn!(
                    capacity = UNKNOWN_KEY_CAPACITY,
                    "unknown sdk key cache is full; new unknown keys reach the database"
                );
                return;
            }
        }
        inner.expires.insert(hash, now + UNKNOWN_KEY_TTL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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
        // Just before they expire: a sweep finds nothing to drop, the key is
        // refused, and the next sweep waits a second.
        let swept = t0 + UNKNOWN_KEY_TTL - Duration::from_millis(500);
        cache.insert(h(u32::MAX), swept);
        assert_eq!(cache.len(), UNKNOWN_KEY_CAPACITY);
        // Everything has expired now, but the second is not up: no sweep.
        let throttled = t0 + UNKNOWN_KEY_TTL + Duration::from_millis(200);
        cache.insert(h(u32::MAX), throttled);
        assert_eq!(cache.len(), UNKNOWN_KEY_CAPACITY);
        assert!(!cache.contains(&h(u32::MAX), throttled));
        // A second after the last sweep, the next one makes room.
        let after = swept + Duration::from_secs(1);
        cache.insert(h(u32::MAX), after);
        assert_eq!(cache.len(), 1);
        assert!(cache.contains(&h(u32::MAX), after));
    }

    /// The listener holds one connection; key lookups get half of the rest,
    /// rounded down, and never fewer than one (#120).
    #[test]
    fn key_lookups_take_half_of_what_the_listener_leaves() {
        for (pool, permits) in [(0, 1), (1, 1), (2, 1), (3, 1), (5, 2), (10, 4)] {
            assert_eq!(KeyLookups::for_pool(pool).permits(), permits, "pool {pool}");
        }
        let fresh = KeyLookups::for_pool(5);
        assert_eq!(fresh.available(), fresh.permits());
    }

    #[test]
    fn minted_keys_carry_their_kind_and_are_recognised() {
        let c = NewKey::mint(KeyKind::Client);
        let s = NewKey::mint(KeyKind::Server);
        assert!(c.key.starts_with("sdk_"));
        assert!(s.key.starts_with("srv_"));
        assert!(looks_like_key(&c.key) && looks_like_key(&s.key));
        assert_ne!(c.key, NewKey::mint(KeyKind::Client).key);
        assert_eq!(c.shown_prefix().len(), 12);
    }

    #[test]
    fn other_strings_are_not_keys() {
        for bad in [
            "",
            "sdk_",
            "sdk_xyz",
            "otto_at_abcdef",
            &format!("pat_{}", "a".repeat(64)),
            &format!("sdk_{}", "g".repeat(64)),
        ] {
            assert!(!looks_like_key(bad), "{bad:?}");
        }
    }

    #[test]
    fn the_hash_is_stable_and_does_not_contain_the_key() {
        let k = NewKey::mint(KeyKind::Server);
        assert_eq!(k.hash(), hash(&k.key));
        assert_eq!(k.hash().len(), 32);
    }
}
