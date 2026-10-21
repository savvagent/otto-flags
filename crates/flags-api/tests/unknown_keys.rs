//! The SDK API answers a repeated unknown key from memory (#104).

use axum::body::Body;
use flags_api::AppState;
use flags_core::keys::{KeyKind, NewKey};
use flags_core::notify::Listener;
use flags_core::rollback::Hints;
use http::{Request, StatusCode};
use sqlx::PgPool;
use tower::ServiceExt;

/// The extractor resolves through the state's unknown-key cache: the first
/// request looks the key up and remembers it, and the second is refused with
/// the database closed, which would otherwise be a 503.
#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn a_repeated_unknown_key_is_refused_from_memory(pool: PgPool) {
    let state = AppState::new(
        otto_tenant::Db::from_pool(pool),
        Listener::detached(),
        Hints::detached(),
        Default::default(),
        "otto_whsec_test",
    );
    let app = flags_api::router(state.clone());
    let key = NewKey::mint(KeyKind::Client).key;
    for attempt in 0..2 {
        let req = Request::get("/api/sdk/flags")
            .header(http::header::AUTHORIZATION, format!("Bearer {key}"))
            .body(Body::empty())
            .expect("request");
        let status = app.clone().oneshot(req).await.expect("response").status();
        assert_eq!(status, StatusCode::UNAUTHORIZED, "attempt {attempt}");
        assert_eq!(state.unknown_keys.len(), 1);
        state.db.pool().close().await;
    }
}
