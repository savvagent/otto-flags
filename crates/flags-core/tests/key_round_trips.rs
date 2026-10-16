//! How many round trips SDK key resolution makes to the database (#104).
//!
//! Every SDK request resolves its key before anything is proven about it, so
//! the lookup's cost is what a flood of well-formed bogus keys costs the small
//! shared database. These tests pin it, counted on the wire: a proxy between
//! the pool and the test database counts the client's synchronisation points,
//! each simple `Query` ('Q') and each extended-protocol `Sync` ('S'), which is
//! one round trip each. sqlx's own query log would undercount: it does not log
//! `BEGIN` or the pool's acquire ping.
//!
//! A change to the count must be deliberate: update the expectation here with
//! the reason.
//!
//! The same proxy can stop forwarding at a chosen round trip
//! ([`RoundTrips::stall_after`]), which is what a half-open connection looks
//! like to the client: the request leaves and no answer ever comes. That
//! shows the lookup's client-side deadlines (#130).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use flags_core::apps::AppsExt;
use flags_core::keys::{self, KeyLookups, LookupBounds, UnknownKeys, KEY_LOOKUP_WAIT};
use otto_tenant::ids::{OrgId, UserId};
use otto_tenant::Db;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};
use sqlx::PgPool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

/// Round trips the client started, across every proxied connection, and the
/// switch that stops forwarding them.
#[derive(Clone)]
struct RoundTrips {
    count: Arc<AtomicUsize>,
    /// A round trip whose number since the last `take` exceeds this is not
    /// forwarded: its connection's forwarder parks until `unstall`.
    stall_after: Arc<AtomicUsize>,
    unstall: Arc<Notify>,
}

impl Default for RoundTrips {
    fn default() -> Self {
        Self {
            count: Arc::default(),
            stall_after: Arc::new(AtomicUsize::new(usize::MAX)),
            unstall: Arc::default(),
        }
    }
}

impl RoundTrips {
    /// Forward the next `n` round trips, then stop.
    fn stall_after(&self, n: usize) {
        self.stall_after.store(n, Ordering::SeqCst);
    }

    /// End every parked forwarder. Each drops its write half to the server,
    /// so the backend sees EOF and ends: left alone, a backend stalled
    /// mid-transaction would keep its lock on `app_keys`, and sqlx could not
    /// drop the test database.
    fn unstall(&self) {
        self.stall_after.store(usize::MAX, Ordering::SeqCst);
        self.unstall.notify_waiters();
    }

    /// The round trips since the last `take`, once `db`'s connection is back
    /// in the pool. sqlx returns a connection in a spawned task that pings it
    /// first, after the caller already has its answer, so the release ping
    /// would otherwise land in the next measurement.
    async fn take(&self, db: &Db) -> usize {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while db.pool().num_idle() < db.pool().size() as usize {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("the pool's connection was never returned");
        self.count.swap(0, Ordering::SeqCst)
    }
}

/// Forward the client's side of one connection to the server, counting each
/// 'Q' and 'S' message after the untyped startup packet (TLS is off, so
/// there is no `SSLRequest` before it). One past `trips.stall_after` is held
/// back, and the forwarder parks until `trips.unstall`.
async fn forward_client(
    mut client: tokio::net::tcp::OwnedReadHalf,
    mut server: tokio::net::tcp::OwnedWriteHalf,
    trips: RoundTrips,
) -> std::io::Result<()> {
    // The startup message has no type byte: a length, then the body.
    let len = client.read_i32().await?;
    let mut body = vec![0u8; usize::try_from(len - 4).expect("startup length")];
    client.read_exact(&mut body).await?;
    server.write_i32(len).await?;
    server.write_all(&body).await?;
    loop {
        let kind = match client.read_u8().await {
            Ok(kind) => kind,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
            Err(e) => return Err(e),
        };
        let len = client.read_i32().await?;
        let mut body = vec![0u8; usize::try_from(len - 4).expect("message length")];
        client.read_exact(&mut body).await?;
        if kind == b'Q' || kind == b'S' {
            // Registered before the check, so an `unstall` between the check
            // and the wait cannot be missed.
            let unstalled = trips.unstall.notified();
            tokio::pin!(unstalled);
            unstalled.as_mut().enable();
            let n = trips.count.fetch_add(1, Ordering::SeqCst) + 1;
            if n > trips.stall_after.load(Ordering::SeqCst) {
                unstalled.await;
                return Ok(());
            }
        }
        server.write_u8(kind).await?;
        server.write_i32(len).await?;
        server.write_all(&body).await?;
    }
}

/// A counting proxy to `upstream`; returns its address.
async fn proxy(upstream: SocketAddr, trips: RoundTrips) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        loop {
            let (client, _) = listener.accept().await.expect("proxy accept");
            let server = TcpStream::connect(upstream)
                .await
                .expect("proxy connect upstream");
            let (client_r, mut client_w) = client.into_split();
            let (mut server_r, server_w) = server.into_split();
            let trips = trips.clone();
            tokio::spawn(async move {
                // A broken proxy shows up as an acquire timeout in the test;
                // say why here.
                if let Err(e) = forward_client(client_r, server_w, trips).await {
                    eprintln!("counting proxy: {e}");
                }
            });
            tokio::spawn(async move { tokio::io::copy(&mut server_r, &mut client_w).await });
        }
    });
    addr
}

/// A one-connection pool, otherwise shaped like production's
/// (`flags-server/src/main.rs`: sqlx defaults, so `test_before_acquire` pings
/// an idle connection before handing it out), reaching the test database
/// through the counting proxy. TLS is off so the proxy can read the protocol.
async fn counted_db(pool: &PgPool) -> (Db, RoundTrips) {
    let options: PgConnectOptions = pool.connect_options().as_ref().clone();
    let upstream = tokio::net::lookup_host((options.get_host(), options.get_port()))
        .await
        .unwrap()
        .min_by_key(|a| !a.is_ipv4())
        .unwrap();
    let trips = RoundTrips::default();
    let addr = proxy(upstream, trips.clone()).await;
    let counted = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(std::time::Duration::from_secs(5))
        .connect_with(
            options
                .host(&addr.ip().to_string())
                .port(addr.port())
                .ssl_mode(PgSslMode::Disable),
        )
        .await
        .unwrap();
    (Db::from_pool(counted), trips)
}

/// A client key that resolves, created through the uncounted pool.
async fn known_key(pool: &PgPool) -> String {
    let db = Db::from_pool(pool.clone());
    let mut tx = db.begin(OrgId::new()).await.unwrap();
    let (_, issued) = tx
        .create_app("web", &["production".into()], UserId::new())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    issued.client_key.unwrap()
}

fn unknown_key() -> String {
    keys::NewKey::mint(keys::KeyKind::Client).key
}

/// A lookup is six round trips, known key or not: the pool's acquire ping
/// (`test_before_acquire`), `BEGIN`, the probe (role checks, role switch,
/// search path, and the lookup's lock, statement, and idle timeouts, #130),
/// the key lookup, `COMMIT`, and the ping sqlx sends before
/// returning the connection to the pool. The connection is held for all six.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn resolving_a_key_takes_six_round_trips(pool: PgPool) {
    let known = known_key(&pool).await;
    let (db, trips) = counted_db(&pool).await;
    for key in [known.as_str(), &unknown_key()] {
        // Warm: the connection's prepared-statement cache holds the
        // statements after the first call, as a production connection's does.
        // A cold call costs one more round trip per statement (Parse and
        // Describe, then Bind and Execute).
        keys::resolve(&db, key).await.unwrap();
        trips.take(&db).await;
        keys::resolve(&db, key).await.unwrap();
        assert_eq!(trips.take(&db).await, 6, "{key}");
    }
    db.pool().close().await;
}

/// A key that did not resolve is answered from memory the next time: no
/// round trip at all. A key that resolves goes to the database every time, so
/// revoking it takes effect at once.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_cached_unknown_key_takes_no_round_trips(pool: PgPool) {
    let known = known_key(&pool).await;
    let unknown_key = unknown_key();
    let (db, trips) = counted_db(&pool).await;
    let unknown = Arc::new(UnknownKeys::new());
    let limit = KeyLookups::new(1, KEY_LOOKUP_WAIT);
    // Warm the connection's statement cache without touching `unknown`.
    keys::resolve(&db, &unknown_key).await.unwrap();
    trips.take(&db).await;

    assert!(keys::resolve_cached(&db, &unknown, &limit, &unknown_key)
        .await
        .unwrap()
        .is_none());
    assert_eq!(trips.take(&db).await, 6, "first lookup of the unknown key");
    assert!(keys::resolve_cached(&db, &unknown, &limit, &unknown_key)
        .await
        .unwrap()
        .is_none());
    assert_eq!(trips.take(&db).await, 0, "the unknown key, remembered");

    for attempt in 0..2 {
        assert!(keys::resolve_cached(&db, &unknown, &limit, &known)
            .await
            .unwrap()
            .is_some());
        assert_eq!(trips.take(&db).await, 6, "known key, attempt {attempt}");
    }
    assert_eq!(unknown.len(), 1);
    db.pool().close().await;
}

/// Client-side bounds short enough for a test, with headroom for the real
/// round trips on a loaded host. The server-side ones are long: these tests
/// stall on the client's side, and a server timeout firing first would be a
/// different failure from the one under test.
fn short_bounds() -> LookupBounds {
    LookupBounds {
        lock: Duration::from_secs(30),
        statement: Duration::from_secs(30),
        transaction: Duration::from_secs(1),
        release: Duration::from_millis(300),
    }
}

/// A lookup whose connection stops answering mid-transaction (the lookup
/// statement leaves, no answer comes) gives up at the transaction deadline
/// with a transient error, and its connection is closed, not returned,
/// before its permit is freed (#130).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_lookup_stalled_mid_transaction_closes_its_connection(pool: PgPool) {
    let (db, trips) = counted_db(&pool).await;
    let unknown = Arc::new(UnknownKeys::new());
    let limit = KeyLookups::new(1, KEY_LOOKUP_WAIT).with_bounds(short_bounds());
    keys::resolve(&db, &unknown_key()).await.unwrap();
    trips.take(&db).await;
    // The acquire ping, BEGIN, and the probe go through; the lookup stalls.
    trips.stall_after(3);
    let started = Instant::now();
    let answer = tokio::time::timeout(
        Duration::from_secs(5),
        keys::resolve_cached(&db, &unknown, &limit, &unknown_key()),
    )
    .await;
    let elapsed = started.elapsed();
    let (size, available) = (db.pool().size(), limit.available());
    trips.unstall();
    let err = answer
        .expect("the stalled lookup never gave up")
        .expect_err("a stalled lookup answered");
    assert!(err.is_transient(), "{err:?}");
    assert_eq!(err.failure_reason().reason, "io", "{err:?}");
    assert!(
        elapsed < Duration::from_millis(2500),
        "gave up after {elapsed:?}"
    );
    assert_eq!(size, 0, "the stalled connection was closed");
    assert_eq!(available, 1, "the permit is free");
    assert!(unknown.is_empty(), "an error is not remembered");
    db.pool().close().await;
}

/// A lookup whose connection stops answering at the release ping, after the
/// answer was read, still answers by the release deadline, with the
/// connection closed rather than left checked out (#130).
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_lookup_stalled_at_the_release_ping_still_answers(pool: PgPool) {
    let (db, trips) = counted_db(&pool).await;
    let unknown = Arc::new(UnknownKeys::new());
    let limit = KeyLookups::new(1, KEY_LOOKUP_WAIT).with_bounds(short_bounds());
    keys::resolve(&db, &unknown_key()).await.unwrap();
    trips.take(&db).await;
    // Everything through COMMIT goes through; the release ping stalls.
    trips.stall_after(5);
    let started = Instant::now();
    let answer = tokio::time::timeout(
        Duration::from_secs(5),
        keys::resolve_cached(&db, &unknown, &limit, &unknown_key()),
    )
    .await;
    let elapsed = started.elapsed();
    let (size, available) = (db.pool().size(), limit.available());
    trips.unstall();
    let owner = answer
        .expect("the stalled release never gave up")
        .expect("the lookup's answer was read before the release");
    assert!(owner.is_none());
    assert!(
        elapsed < Duration::from_millis(2500),
        "answered after {elapsed:?}"
    );
    assert_eq!(size, 0, "the stalled connection was closed");
    assert_eq!(available, 1, "the permit is free");
    db.pool().close().await;
}
