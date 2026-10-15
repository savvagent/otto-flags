//! Example Axum server that evaluates Otto Flags with the Rust server SDK.
//!
//! Configuration comes from the environment (see `Settings::from_env`):
//! `OTTO_FLAGS_SDK_KEY` (required), `OTTO_FLAGS_API_URL`, `OTTO_FLAGS_ENVIRONMENT`, and `PORT`.

use axum::{
    extract::{Query, State},
    routing::{get, post},
    Json, Router,
};
use otto_flags::{Config, Context, FlagClient};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::net::SocketAddr;
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

/// The production Otto Flags API, used when `OTTO_FLAGS_API_URL` is not set.
const DEFAULT_API_URL: &str = "https://otto-flags.savvagent.com";
const DEFAULT_ENVIRONMENT: &str = "development";
const DEFAULT_PORT: u16 = 8083;

/// Flags this example reads, with the value each falls back to when the API cannot be reached.
const FLAG_DEFAULTS: &[(&str, bool)] = &[
    ("new-ui", false),
    ("beta-features", false),
    ("advanced-analytics", false),
    ("advanced-processing", false),
];

/// Settings read from the environment.
#[derive(Debug, PartialEq, Eq)]
struct Settings {
    sdk_key: String,
    api_url: String,
    environment: String,
    port: u16,
}

impl Settings {
    /// Reads settings through `lookup` (normally `std::env::var(..).ok()`).
    fn from_env(lookup: impl Fn(&str) -> Option<String>) -> Result<Self, String> {
        let sdk_key = lookup("OTTO_FLAGS_SDK_KEY")
            .ok_or("OTTO_FLAGS_SDK_KEY is not set; use the app's SDK key (sdk_...)")?;
        if !sdk_key.starts_with("sdk_") {
            return Err("OTTO_FLAGS_SDK_KEY must be an SDK key starting with sdk_".into());
        }
        let port = match lookup("PORT") {
            Some(p) => p
                .parse()
                .map_err(|_| format!("PORT is not a port number: {p}"))?,
            None => DEFAULT_PORT,
        };
        Ok(Self {
            sdk_key,
            api_url: lookup("OTTO_FLAGS_API_URL").unwrap_or_else(|| DEFAULT_API_URL.into()),
            environment: lookup("OTTO_FLAGS_ENVIRONMENT")
                .unwrap_or_else(|| DEFAULT_ENVIRONMENT.into()),
            port,
        })
    }
}

#[derive(Clone)]
struct AppState {
    // `FlagClient` is cheap to clone: its cache and HTTP client are shared.
    flags: FlagClient,
    environment: String,
}

impl AppState {
    /// An evaluation context for one user in the configured environment.
    fn context(&self, user_id: &str) -> Context {
        Context::new()
            .with_user_id(user_id)
            .with_environment(&self.environment)
    }
}

#[derive(Deserialize)]
struct FeaturesQuery {
    #[serde(rename = "userId")]
    user_id: String,
}

#[derive(Serialize)]
struct FeaturesResponse {
    #[serde(rename = "userId")]
    user_id: String,
    features: HashMap<String, bool>,
}

#[derive(Deserialize)]
struct DataRequest {
    #[serde(rename = "userId")]
    user_id: Option<String>,
    data: serde_json::Value,
}

#[derive(Serialize)]
struct DataResponse {
    processed: bool,
    method: String,
    data: serde_json::Value,
}

#[derive(Serialize)]
struct HealthResponse {
    status: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                "rust_server_example=info,otto_flags=info,tower_http=info".into()
            }),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    let settings = Settings::from_env(|name| std::env::var(name).ok())?;

    let config = FLAG_DEFAULTS.iter().fold(
        Config::new(&settings.sdk_key).with_base_url(&settings.api_url),
        |config, (key, value)| config.with_default(*key, *value),
    );
    let flags = FlagClient::new(config)?;

    let state = AppState {
        flags,
        environment: settings.environment.clone(),
    };

    let app = Router::new()
        .route("/health", get(health))
        .route("/api/features", get(get_features))
        .route("/api/data", post(process_data))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    let addr = SocketAddr::from(([0, 0, 0, 0], settings.port));
    let listener = tokio::net::TcpListener::bind(addr).await?;

    tracing::info!("Server listening on {}", listener.local_addr()?);
    tracing::info!(
        "Otto Flags API URL: {} (environment: {})",
        settings.api_url,
        settings.environment
    );

    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
    })
}

/// Reports which features are on for a user. `is_enabled` never fails: if the API cannot be
/// reached it returns the flag's configured default, so this handler always answers.
async fn get_features(
    State(state): State<AppState>,
    Query(params): Query<FeaturesQuery>,
) -> Json<FeaturesResponse> {
    let ctx = state.context(&params.user_id);

    let mut features = HashMap::new();
    for (flag, name) in [
        ("new-ui", "newUI"),
        ("beta-features", "betaFeatures"),
        ("advanced-analytics", "advancedAnalytics"),
    ] {
        let enabled = state.flags.is_enabled(flag, Some(ctx.clone())).await;
        features.insert(name.to_string(), enabled);
    }

    Json(FeaturesResponse {
        user_id: params.user_id,
        features,
    })
}

/// Processes data with the advanced or basic method, gated on `advanced-processing`.
async fn process_data(
    State(state): State<AppState>,
    Json(request): Json<DataRequest>,
) -> Json<DataResponse> {
    let user_id = request.user_id.as_deref().unwrap_or("anonymous");
    let ctx = state
        .context(user_id)
        .with_attribute("endpoint", serde_json::json!("/api/data"));

    let method = if state
        .flags
        .is_enabled("advanced-processing", Some(ctx))
        .await
    {
        "advanced"
    } else {
        "basic"
    };

    Json(DataResponse {
        processed: true,
        method: method.to_string(),
        data: request.data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let vars: HashMap<String, String> = vars
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |name| vars.get(name).cloned()
    }

    #[test]
    fn sdk_key_is_required() {
        assert!(Settings::from_env(lookup(&[])).is_err());
    }

    #[test]
    fn sdk_key_must_be_a_client_sdk_key() {
        let err = Settings::from_env(lookup(&[("OTTO_FLAGS_SDK_KEY", "srv_abc")])).unwrap_err();
        assert!(err.contains("sdk_"), "{err}");
    }

    #[test]
    fn defaults_to_production_development_and_port_8083() {
        let settings = Settings::from_env(lookup(&[("OTTO_FLAGS_SDK_KEY", "sdk_abc")])).unwrap();
        assert_eq!(
            settings,
            Settings {
                sdk_key: "sdk_abc".into(),
                api_url: "https://otto-flags.savvagent.com".into(),
                environment: "development".into(),
                port: 8083,
            }
        );
    }

    #[test]
    fn every_variable_is_honoured() {
        let settings = Settings::from_env(lookup(&[
            ("OTTO_FLAGS_SDK_KEY", "sdk_abc"),
            ("OTTO_FLAGS_API_URL", "http://localhost:8080"),
            ("OTTO_FLAGS_ENVIRONMENT", "staging"),
            ("PORT", "9000"),
        ]))
        .unwrap();
        assert_eq!(settings.api_url, "http://localhost:8080");
        assert_eq!(settings.environment, "staging");
        assert_eq!(settings.port, 9000);
    }

    #[test]
    fn a_bad_port_is_an_error() {
        let err = Settings::from_env(lookup(&[("OTTO_FLAGS_SDK_KEY", "sdk_abc"), ("PORT", "x")]))
            .unwrap_err();
        assert!(err.contains("PORT"), "{err}");
    }
}
