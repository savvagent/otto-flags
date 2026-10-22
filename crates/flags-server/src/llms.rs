//! `GET /llms.txt` — what an agent should read first. Static: no database, no state, no auth.

use axum::http::{header, HeaderValue};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;

/// The one route. Other methods answer 405.
pub fn router() -> Router {
    Router::new().route("/llms.txt", get(llms))
}

async fn llms() -> impl IntoResponse {
    (
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("text/plain; charset=utf-8"),
            ),
            (
                header::CACHE_CONTROL,
                HeaderValue::from_static("public, max-age=3600"),
            ),
        ],
        flags_mcp::LLMS_TXT,
    )
}
