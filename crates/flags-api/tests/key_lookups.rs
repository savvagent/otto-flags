//! SDK key lookups hold a bounded share of the pool (#120).
//!
//! A connection outside the pool under test holds `app_keys` in ACCESS
//! EXCLUSIVE mode, so every lookup that gets a permit blocks in the database
//! holding its pool connection: the worst case a slow database or a flood of
//! distinct unknown keys can produce.

use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use flags_api::AppState;
use flags_core::keys::{KeyKind, KeyLookups, NewKey};
use flags_core::notify::Listener;
use flags_core::rollback::Hints;
use http::{Request, StatusCode};
use sqlx::postgres::PgPoolOptions;
use sqlx::{Connection, PgConnection, PgPool};
use tokio::sync::mpsc;
use tower::ServiceExt;

/// What one request answered: status, `Retry-After`, and the body's `error`.
type Answer = (StatusCode, Option<String>, Option<String>);

/// A pool of 5, like production's, on the test database, and the SDK API's
/// state on it with `limit` in place of the size-derived one. Close the pool
/// before the test returns so sqlx can drop the database.
async fn state_on_a_pool_of_five(pool: &PgPool, limit: KeyLookups) -> (AppState, PgPool) {
    let five = PgPoolOptions::new()
        .max_connections(5)
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let mut state = AppState::new(
        otto_tenant::Db::from_pool(five.clone()),
        Listener::detached(),
        Hints::detached(),
        Default::default(),
        "otto_whsec_test",
    );
    // What production gets from a pool of 5, before the test's own limit.
    assert_eq!(state.key_lookups.permits(), 2);
    state.key_lookups = Arc::new(limit);
    (state, five)
}

/// A connection outside the pool under test that holds `app_keys` locked
/// until [`unlock`].
async fn lock_app_keys(pool: &PgPool) -> PgConnection {
    let mut lock = PgConnection::connect_with(&pool.connect_options())
        .await
        .unwrap();
    sqlx::raw_sql("BEGIN; LOCK TABLE public.app_keys IN ACCESS EXCLUSIVE MODE")
        .execute(&mut lock)
        .await
        .unwrap();
    lock
}

async fn unlock(lock: &mut PgConnection) {
    sqlx::raw_sql("ROLLBACK").execute(&mut *lock).await.unwrap();
}

/// Sessions in the test database waiting on a lock: lookups blocked behind
/// [`lock_app_keys`]. Asked inside the lock's transaction, which would keep
/// reading the activity snapshot it took first, so the snapshot is cleared
/// before each read.
async fn lock_waiters(lock: &mut PgConnection) -> i64 {
    sqlx::query("SELECT pg_stat_clear_snapshot()")
        .execute(&mut *lock)
        .await
        .unwrap();
    sqlx::query_scalar(
        "SELECT count(*) FROM pg_stat_activity \
         WHERE wait_event_type = 'Lock' AND datname = current_database()",
    )
    .fetch_one(&mut *lock)
    .await
    .unwrap()
}

/// Poll until `available() == 0` and `waiters` lookups are blocked on the
/// lock; panics after 5 s.
async fn until_blocked(state: &AppState, lock: &mut PgConnection, waiters: i64) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if state.key_lookups.available() == 0 && lock_waiters(lock).await == waiters {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{waiters} lookups never blocked on the lock"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// Poll until every permit is free again; panics after 5 s.
async fn until_released(state: &AppState) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while state.key_lookups.available() != state.key_lookups.permits() {
        assert!(Instant::now() < deadline, "a permit was never released");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// `GET /api/sdk/flags` with a fresh random client key, in its own task,
/// sending what it answered on `answers`.
fn request(
    state: &AppState,
    answers: mpsc::UnboundedSender<Answer>,
) -> tokio::task::JoinHandle<()> {
    let app = flags_api::router(state.clone());
    tokio::spawn(async move {
        let key = NewKey::mint(KeyKind::Client).key;
        let req = Request::get("/api/sdk/flags")
            .header(http::header::AUTHORIZATION, format!("Bearer {key}"))
            .body(Body::empty())
            .expect("request");
        let res = app.oneshot(req).await.expect("response");
        let status = res.status();
        let retry_after = res
            .headers()
            .get(http::header::RETRY_AFTER)
            .map(|v| v.to_str().unwrap().to_string());
        let body = axum::body::to_bytes(res.into_body(), 64 * 1024)
            .await
            .unwrap();
        let code = serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v["error"].as_str().map(str::to_string));
        let _ = answers.send((status, retry_after, code));
    })
}

async fn next(answers: &mut mpsc::UnboundedReceiver<Answer>) -> Answer {
    tokio::time::timeout(Duration::from_secs(5), answers.recv())
        .await
        .expect("a request never answered")
        .expect("every request task ended")
}

/// The acceptance test: with both permits held by lookups blocked in the
/// database, the other 3 of the pool's 5 connections can still be acquired,
/// the 18 requests past the limit are refused `503 busy` with `Retry-After`,
/// and the 2 in flight answer `401` once the database moves again.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_flood_of_unknown_keys_leaves_connections_free(pool: PgPool) {
    let (state, five) =
        state_on_a_pool_of_five(&pool, KeyLookups::new(2, Duration::from_millis(300))).await;
    let mut lock = lock_app_keys(&pool).await;
    let (tx, mut answers) = mpsc::unbounded_channel();
    for _ in 0..20 {
        request(&state, tx.clone());
    }
    drop(tx);
    until_blocked(&state, &mut lock, 2).await;

    let mut held = Vec::new();
    for n in 0..3 {
        let conn = tokio::time::timeout(Duration::from_secs(1), five.acquire())
            .await
            .unwrap_or_else(|_| panic!("connection {n} of the 3 left free was not available"))
            .unwrap();
        held.push(conn);
    }
    drop(held);

    for _ in 0..18 {
        assert_eq!(
            next(&mut answers).await,
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Some("5".to_string()),
                Some("busy".to_string())
            )
        );
    }
    unlock(&mut lock).await;
    for _ in 0..2 {
        let (status, _, _) = next(&mut answers).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    assert!(answers.recv().await.is_none(), "exactly 20 answers");
    until_released(&state).await;
    five.close().await;
    lock.close().await.unwrap();
}

/// A client that hangs up mid-lookup does not free its permit: the lookup
/// still holds a connection until it finishes.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_dropped_request_keeps_its_permit_until_the_lookup_ends(pool: PgPool) {
    let (state, five) =
        state_on_a_pool_of_five(&pool, KeyLookups::new(1, Duration::from_millis(300))).await;
    let mut lock = lock_app_keys(&pool).await;
    let (tx, _answers) = mpsc::unbounded_channel();
    let req = request(&state, tx);
    until_blocked(&state, &mut lock, 1).await;
    req.abort();
    assert!(req.await.unwrap_err().is_cancelled());
    assert_eq!(
        state.key_lookups.available(),
        0,
        "the dropped request's lookup still holds its permit"
    );
    unlock(&mut lock).await;
    until_released(&state).await;
    five.close().await;
    lock.close().await.unwrap();
}
