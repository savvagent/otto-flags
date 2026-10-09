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
//! Assembly is a library function so a test can build the whole router: axum
//! panics on a route registered twice, and that is better found by a test than
//! by a deploy.

pub mod config;
pub mod health;

use std::sync::Arc;

use anyhow::{Context, Result};
use axum::response::{IntoResponse, Response};
use axum::Router;
use flags_core::notify::Listener;
use http::{StatusCode, Uri};
use otto_resource::{ClientConfig, PlatformClient};
use otto_tenant::Db;
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
    config: &Config,
) -> Router {
    let mcp = flags_mcp::router(db.clone(), mcp_config(platform, config));
    let api = flags_api::router(flags_api::AppState::new(
        db.clone(),
        listener,
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

    #[tokio::test]
    async fn sdk_routes_refuse_a_missing_key_without_touching_the_database() {
        let (status, body) = get("/api/sdk/flags", "flags.example.com").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        assert_eq!(body["error"], "invalid_key");
        let (status, _) = get("/nope", "flags.example.com").await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
