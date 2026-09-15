use axum::{
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post},
    Json, Router,
};
use otto_flags::{EvaluationContext, OttoFlagsClient, OttoFlagsConfig};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::net::SocketAddr;
use std::sync::Arc;
use tower_http::trace::TraceLayer;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

#[derive(Clone)]
struct AppState {
    otto_flags: Arc<OttoFlagsClient>,
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

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "rust_server_example=info,tower_http=info".into()),
        )
        .with(tracing_subscriber::fmt::layer())
        .init();

    // Get configuration from environment
    let api_url = env::var("OTTO_FLAGS_API_URL").unwrap_or_else(|_| "http://localhost:8080".to_string());
    let sdk_key = env::var("OTTO_FLAGS_SDK_KEY").unwrap_or_else(|_| "your-sdk-key".to_string());
    let port = env::var("PORT").unwrap_or_else(|_| "8083".to_string());

    // Initialize Otto Flags client
    let config = OttoFlagsConfig::builder()
        .api_url(&api_url)
        .sdk_key(&sdk_key)
        .environment("development")
        .cache_enabled(true)
        .cache_ttl(60) // 1 minute
        .build();

    let otto_flags = OttoFlagsClient::new(config)
        .await
        .expect("Failed to initialize Otto Flags client");

    let state = AppState {
        otto_flags: Arc::new(otto_flags),
    };

    // Build application routes
    let app = Router::new()
        .route("/health", get(health))
        .route("/api/features", get(get_features))
        .route("/api/data", post(process_data))
        .layer(TraceLayer::new_for_http())
        .with_state(state);

    // Start server
    let addr: SocketAddr = format!("0.0.0.0:{}", port)
        .parse()
        .expect("Invalid address");

    tracing::info!("Server starting on {}", addr);
    tracing::info!("Otto Flags API URL: {}", api_url);

    axum::Server::bind(&addr)
        .serve(app.into_make_service())
        .await
        .expect("Failed to start server");
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
    })
}

async fn get_features(
    State(state): State<AppState>,
    Query(params): Query<FeaturesQuery>,
) -> Result<Json<FeaturesResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = params.user_id;

    let ctx = EvaluationContext::builder()
        .user_id(&user_id)
        .build();

    // Check multiple feature flags
    let new_ui = state
        .otto_flags
        .is_enabled("new-ui", &ctx)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("Failed to check feature flags: {}", e),
                }),
            )
        })?;

    let beta_features = state
        .otto_flags
        .is_enabled("beta-features", &ctx)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("Failed to check feature flags: {}", e),
                }),
            )
        })?;

    let advanced_analytics = state
        .otto_flags
        .is_enabled("advanced-analytics", &ctx)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("Failed to check feature flags: {}", e),
                }),
            )
        })?;

    let mut features = HashMap::new();
    features.insert("newUI".to_string(), new_ui);
    features.insert("betaFeatures".to_string(), beta_features);
    features.insert("advancedAnalytics".to_string(), advanced_analytics);

    Ok(Json(FeaturesResponse { user_id, features }))
}

async fn process_data(
    State(state): State<AppState>,
    Json(request): Json<DataRequest>,
) -> Result<Json<DataResponse>, (StatusCode, Json<ErrorResponse>)> {
    let user_id = request.user_id.unwrap_or_else(|| "anonymous".to_string());

    let mut attributes = HashMap::new();
    attributes.insert("endpoint".to_string(), "/api/data".to_string());

    let ctx = EvaluationContext::builder()
        .user_id(&user_id)
        .attributes(attributes)
        .build();

    let advanced_processing = state
        .otto_flags
        .is_enabled("advanced-processing", &ctx)
        .await
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(ErrorResponse {
                    error: format!("Failed to check feature flag: {}", e),
                }),
            )
        })?;

    let method = if advanced_processing {
        "advanced"
    } else {
        "basic"
    };

    Ok(Json(DataResponse {
        processed: true,
        method: method.to_string(),
        data: request.data,
    }))
}
