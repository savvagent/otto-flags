//! `flags-api` — what running applications and the platform call.
//!
//! ```text
//!   POST /api/flags/{key}/evaluate   SDK key   evaluate one flag for a context
//!   POST /api/evaluate/{key}         SDK key   the same (the Rust SDK's spelling)
//!   GET  /api/sdk/flags              SDK key   every active flag in the key's app
//!   GET  /api/sdk/enterprise-flags   SDK key   org-wide flags (none yet: always empty)
//!   GET  /api/flags/stream           SDK key   server-sent flag-change events
//!   POST /api/telemetry/evaluations  SDK key   evaluation counts
//!   POST /api/telemetry/errors       SDK key   errors raised behind a flag
//!   POST /platform/webhooks          signed    the platform's lifecycle events
//! ```
//!
//! The contract is the one in `docs/SDK-DEVELOPER-GUIDE.md`, kept as-is so the
//! SDKs in `packages/` work unchanged. Keys travel in `Authorization: Bearer`
//! or `X-SDK-Key`, never in a URL. This surface is the hot path: nothing here
//! calls the platform or an LLM, and no tool is metered.

mod error;
mod platform;
mod sdk;
mod stream;
mod telemetry;

use std::time::Duration;

use axum::routing::{get, post};
use axum::Router;
use flags_core::notify::Listener;
use http::{header, HeaderName, Method};
use otto_tenant::Db;
use tower_http::cors::{Any, CorsLayer};

pub use error::ApiError;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub listener: Listener,
    /// Verifies the platform's lifecycle webhooks (`otto_whsec_…`).
    pub platform_webhook_secret: String,
    /// How often an idle stream is sent a heartbeat.
    pub heartbeat: Duration,
}

impl AppState {
    pub fn new(db: Db, listener: Listener, platform_webhook_secret: impl Into<String>) -> Self {
        Self {
            db,
            listener,
            platform_webhook_secret: platform_webhook_secret.into(),
            heartbeat: Duration::from_secs(30),
        }
    }
}

pub fn router(state: AppState) -> Router {
    // Browser SDKs call from the customer's own origin with an Authorization
    // header, so the SDK routes answer CORS for any origin. Nothing here uses
    // cookies, so allowing any origin grants nothing a page did not already
    // have: the key it sends.
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([
            header::AUTHORIZATION,
            header::CONTENT_TYPE,
            HeaderName::from_static("x-sdk-key"),
        ])
        .max_age(Duration::from_secs(3600));

    let sdk = Router::new()
        .route("/api/flags/{key}/evaluate", post(sdk::evaluate))
        .route("/api/evaluate/{key}", post(sdk::evaluate))
        .route("/api/sdk/flags", get(sdk::list_flags))
        .route("/api/sdk/enterprise-flags", get(sdk::enterprise_flags))
        .route("/api/flags/stream", get(stream::stream))
        .route("/api/telemetry/evaluations", post(telemetry::evaluations))
        .route("/api/telemetry/errors", post(telemetry::errors))
        .layer(cors);

    Router::new()
        .merge(sdk)
        .route("/platform/webhooks", post(platform::receive))
        .with_state(state)
}
