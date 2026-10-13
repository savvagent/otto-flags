//! `flags-server` — one binary, one port, every surface.
//!
//! ```text
//!   /healthz /readyz                  health      no database on the liveness path
//!   /.well-known/oauth-protected-resource, /mcp
//!                                     flags-mcp   agents: bearer tokens from the platform
//!   /api/…                            flags-api   SDKs: sdk_/srv_ keys
//!   /platform/webhooks                flags-api   the platform's signed lifecycle events
//!   everything else                   JSON 404
//! ```
//!
//! The rollback checker is started separately by [`spawn_checker`] so `main`
//! can stop it on shutdown; its [`Hints`] handle is passed to [`router`].
//!
//! Assembly is a library function so a test can build the whole router: axum
//! panics on a route registered twice, and that is better found by a test than
//! by a deploy.

pub mod config;
pub mod health;

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::response::{IntoResponse, Response};
use axum::Router;
use flags_core::entitlements::Entitlements;
use flags_core::notify::Listener;
use flags_core::rollback::{Checker, CheckerConfig, Hints};
use http::{StatusCode, Uri};
use otto_resource::{ClientConfig, PlatformClient};
use otto_tenant::Db;
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tower_http::trace::TraceLayer;

pub use config::{Config, LogFormat};

/// The one client every platform call goes through (it holds the caches).
pub fn platform_client(config: &Config) -> Result<Arc<PlatformClient>> {
    let client = PlatformClient::new(ClientConfig::new(
        &config.platform_url,
        &config.resource_uri,
        &config.introspection_secret,
    ))
    .context("FLAGS_PLATFORM_URL is not usable")?;
    Ok(Arc::new(client))
}

pub fn router(
    db: Db,
    platform: Arc<PlatformClient>,
    listener: Listener,
    hints: Hints,
    config: &Config,
) -> Router {
    let mcp = flags_mcp::router(db.clone(), mcp_config(platform, config));
    let api = flags_api::router(flags_api::AppState::new(
        db.clone(),
        listener,
        hints,
        &config.platform_webhook_secret,
    ));
    health::router(db)
        .merge(mcp)
        .merge(api)
        .fallback(not_found)
        // Request spans without headers: including them would log every
        // bearer token and SDK key in clear.
        .layer(TraceLayer::new_for_http())
}

/// Start the rollback checker. Returns the [`Hints`] for the SDK API and the
/// task to await on shutdown. Entitlement checks use [`Config::upgrade_url`],
/// as the MCP surface does.
pub fn spawn_checker(
    db: Db,
    platform: Arc<PlatformClient>,
    config: &Config,
    checker: CheckerConfig,
    shutdown: watch::Receiver<bool>,
) -> (Hints, JoinHandle<()>) {
    Checker::spawn(
        db,
        Entitlements::new(platform, config.upgrade_url()),
        checker,
        shutdown,
    )
}

/// How the rollback checker's task ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckerEnd {
    /// After shutdown was requested: expected.
    Shutdown,
    /// Before shutdown was requested, without panicking.
    Early,
    /// It panicked.
    Panicked,
    /// It was aborted.
    Cancelled,
}

/// Wait for the checker task to end and say how. Ending while the server is
/// still serving is logged at `error`: from then on no flag is rolled back
/// automatically, and nothing else would notice.
pub async fn watch_checker(task: JoinHandle<()>, shutdown: watch::Receiver<bool>) -> CheckerEnd {
    let ended = task.await;
    let requested = *shutdown.borrow();
    let end = match ended {
        Ok(()) if requested => CheckerEnd::Shutdown,
        Ok(()) => CheckerEnd::Early,
        Err(e) if e.is_panic() => CheckerEnd::Panicked,
        Err(_) => CheckerEnd::Cancelled,
    };
    match end {
        CheckerEnd::Shutdown => {}
        CheckerEnd::Cancelled if requested => {}
        CheckerEnd::Early | CheckerEnd::Cancelled => tracing::error!(
            ?end,
            "the auto-rollback checker stopped while the server is running; no flag will be \
             rolled back automatically until restart"
        ),
        CheckerEnd::Panicked => tracing::error!(
            "the auto-rollback checker panicked; no flag will be rolled back automatically \
             until restart"
        ),
    }
    end
}

fn mcp_config(platform: Arc<PlatformClient>, config: &Config) -> flags_mcp::Config {
    let mut mcp = flags_mcp::Config::new(
        platform,
        &config.platform_url,
        &config.resource_uri,
        &config.public_url,
    );
    mcp.allowed_hosts = config.allowed_hosts();
    mcp.allowed_origins = config.allowed_origins.clone();
    mcp.enforce_quotas = config.enforce_quotas;
    mcp.upgrade_url = config.upgrade_url();
    mcp
}

async fn not_found(uri: Uri) -> Response {
    (
        StatusCode::NOT_FOUND,
        axum::Json(serde_json::json!({
            "error": "not_found",
            "error_description": format!(
                "no route serves {}. Agents: the MCP endpoint is /mcp (see \
                 /.well-known/oauth-protected-resource). SDKs: see /api/sdk/flags.",
                uri.path()
            ),
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use http::Request;
    use tower::ServiceExt;

    fn app() -> Router {
        let config = Config::for_test();
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy(&config.database_url)
            .unwrap();
        router(
            Db::from_pool(pool),
            platform_client(&config).unwrap(),
            Listener::detached(),
            Hints::detached(),
            &config,
        )
    }

    async fn get(path: &str, host: &str) -> (StatusCode, serde_json::Value) {
        let res = app()
            .oneshot(
                Request::get(path)
                    .header("host", host)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or_default())
    }

    #[tokio::test]
    async fn the_whole_router_assembles_and_discovery_names_the_platform() {
        let (status, body) =
            get("/.well-known/oauth-protected-resource", "flags.example.com").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["resource"], "https://flags.example.com/mcp");
        assert_eq!(
            body["authorization_servers"][0],
            "https://platform.example.com"
        );
        assert_eq!(
            body["scopes_supported"],
            serde_json::json!(["flags:read", "flags:write", "apps:admin"])
        );
    }

    #[tokio::test]
    async fn an_unauthenticated_mcp_call_is_told_where_to_authenticate() {
        let res = app()
            .oneshot(
                Request::post("/mcp")
                    .header("host", "flags.example.com")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(res.status(), StatusCode::UNAUTHORIZED);
        let challenge = res.headers()["www-authenticate"].to_str().unwrap();
        assert!(challenge.contains(
            "resource_metadata=\"https://flags.example.com/.well-known/oauth-protected-resource\""
        ));
    }

    #[test]
    fn the_checker_and_the_mcp_surface_share_one_upgrade_url() {
        let config = Config::for_test();
        assert_eq!(
            config.upgrade_url(),
            "https://platform.example.com/settings/billing"
        );
        let platform = platform_client(&config).unwrap();
        assert_eq!(
            mcp_config(platform, &config).upgrade_url,
            config.upgrade_url()
        );
    }

    #[tokio::test]
    async fn a_checker_that_ends_before_shutdown_is_reported() {
        let (_stop, shutdown) = watch::channel(false);
        let task = tokio::spawn(async {});
        assert_eq!(watch_checker(task, shutdown).await, CheckerEnd::Early);
    }

    #[tokio::test]
    async fn a_checker_that_panics_is_reported() {
        let (_stop, shutdown) = watch::channel(false);
        let task = tokio::spawn(async { panic!("boom") });
        assert_eq!(watch_checker(task, shutdown).await, CheckerEnd::Panicked);
    }

    #[tokio::test]
    async fn a_checker_that_ends_on_shutdown_is_not_an_error() {
        let (stop, shutdown) = watch::channel(false);
        let mut rx = shutdown.clone();
        let task = tokio::spawn(async move {
            let _ = rx.changed().await;
        });
        stop.send(true).unwrap();
        assert_eq!(watch_checker(task, shutdown).await, CheckerEnd::Shutdown);
    }

    #[tokio::test]
    async fn sdk_routes_refuse_a_missing_key_without_touching_the_database() {
        let (status, body) = get("/api/sdk/flags", "flags.example.com").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"], "invalid_key");
        let (status, _) = get("/nope", "flags.example.com").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
