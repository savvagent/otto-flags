//! `flags-mcp` — the Streamable HTTP MCP surface agents manage flags through.
//!
//! ```text
//!   POST /mcp
//!     └─ require_bearer ──── introspect the token at the platform, attach the principal
//!          └─ StreamableHttpService ──── rmcp, JSON-RPC framing
//!               └─ Flags ──── one tool call
//!                    └─ begin_live(org) ──── flags-core, RLS, commit
//! ```
//!
//! No SQL lives here: every statement is a `flags-core` method on a pinned
//! [`otto_tenant::Tx`]. Nothing is client-specific: plain Streamable HTTP and
//! plain JSON Schema, so every MCP-speaking agent gets the same surface.

pub mod auth;
pub mod error;
pub mod server;
pub mod tools;

use std::sync::Arc;

use axum::routing::{any_service, get};
use axum::Router;
use flags_core::usage::Meter;
use otto_resource::PlatformClient;
use otto_tenant::Db;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

pub use auth::ResourceServer;
pub use server::Flags;

/// Deployment settings the MCP surface cannot infer.
#[derive(Debug, Clone)]
pub struct Config {
    pub platform: Arc<PlatformClient>,
    /// The platform's public base URL, advertised as the authorization server.
    pub platform_url: String,
    /// This resource's canonical URI and the audience every token must carry.
    /// Exactly the `resource_uri` registered at the platform.
    pub resource_uri: String,
    /// Public base URL of this service, for the discovery pointer in a `401`.
    pub public_url: String,
    /// Accepted `Host` headers. rmcp defaults to loopback only, which rejects
    /// every request to a hosted server with an error that never mentions
    /// hostnames, so it is required here.
    pub allowed_hosts: Vec<String>,
    /// Accepted browser `Origin`s; empty disables the check (CLI agents send none).
    pub allowed_origins: Vec<String>,
    pub enforce_quotas: bool,
    pub upgrade_url: String,
}

impl Config {
    pub fn new(
        platform: Arc<PlatformClient>,
        platform_url: impl Into<String>,
        resource_uri: impl Into<String>,
        public_url: impl Into<String>,
    ) -> Self {
        let public_url = public_url.into();
        let host = url::Url::parse(&public_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .unwrap_or_default();
        let platform_url = platform_url.into();
        Self {
            platform,
            upgrade_url: format!("{}/settings/billing", platform_url.trim_end_matches('/')),
            platform_url,
            resource_uri: resource_uri.into(),
            allowed_hosts: if host.is_empty() { vec![] } else { vec![host] },
            allowed_origins: vec![],
            enforce_quotas: false,
            public_url,
        }
    }
}

/// The MCP surface: `/.well-known/oauth-protected-resource` (open, because it
/// is how an unauthenticated client learns to authenticate) and `/mcp`
/// (bearer tokens audienced for [`Config::resource_uri`]).
///
/// Stateless transport: nothing is pushed to a client, so any replica can
/// serve any request without sticky sessions.
pub fn router(db: Db, config: Config) -> Router {
    let rs = Arc::new(ResourceServer::new(
        db.clone(),
        config.platform.clone(),
        config.resource_uri.clone(),
        config.public_url.clone(),
        config.platform_url.clone(),
    ));

    let mut transport = StreamableHttpServerConfig::default();
    transport.stateful_mode = false;
    transport.json_response = true;
    transport.allowed_hosts = config.allowed_hosts;
    transport.allowed_origins = config.allowed_origins;

    let flags = Flags::new(
        db,
        config.platform.clone(),
        Meter::new(
            config.platform.clone(),
            config.enforce_quotas,
            config.upgrade_url,
        ),
    );
    let service = StreamableHttpService::new(
        move || Ok(flags.clone()),
        Arc::new(LocalSessionManager::default()),
        transport,
    );

    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(auth::protected_resource_metadata),
        )
        .with_state(rs.clone())
        .route_service(
            "/mcp",
            any_service(service).layer(axum::middleware::from_fn_with_state(
                rs,
                auth::require_bearer,
            )),
        )
}
