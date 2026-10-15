# Otto Flags Rust Server SDK

Official Rust Server SDK for Otto Flags - AI-powered feature flags with automatic error detection.

## Features

- 🚀 **Fast Evaluation**: Thread-safe in-memory caching with configurable TTL
- 📊 **Telemetry**: Automatic tracking of flag evaluations
- 🤖 **AI Error Detection**: Correlate errors with flag changes
- 📦 **Type Safe**: Full Rust type safety with comprehensive documentation
- 🌐 **Server-Optimized**: Built specifically for Rust server environments
- ⚡ **High Performance**: Async/await with Tokio runtime
- 🔒 **Memory Safe**: Zero-cost abstractions with Rust's ownership system

## Requirements

- Rust 1.70 or higher

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
otto_flags = "0.1.0"
tokio = { version = "1.0", features = ["full"] }
```

## Quick Start

```rust
use otto_flags::{Config, Context, FlagClient};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize the client
    let config = Config::new("sdk_your_api_key_here")
        .with_application_id("your-app-id");

    let client = FlagClient::new(config)?;

    // Evaluate a flag
    let context = Context::new()
        .with_user_id("user-123")
        .with_environment("production");

    let result = client.evaluate("new-feature", Some(context)).await?;
    println!("Feature enabled: {}", result.value);

    // Or use the convenience method
    if client.is_enabled("new-feature", None).await {
        // New feature code
    }

    Ok(())
}
```

## Configuration

```rust
use otto_flags::Config;
use std::time::Duration;

let config = Config::new("sdk_your_api_key_here")
    .with_application_id("your-app-id")
    .with_base_url("https://otto-flags.savvagent.com")
    .with_cache_ttl(Duration::from_secs(60))
    .with_timeout(Duration::from_secs(5))
    .with_default("feature-a", false)
    .with_default("feature-b", true);

let client = FlagClient::new(config)?;
```

## Usage Examples

### Actix Web

```rust
use actix_web::{web, App, HttpResponse, HttpServer};
use otto_flags::{Config, Context, FlagClient};

struct AppState {
    flag_client: FlagClient,
}

async fn handler(
    data: web::Data<AppState>,
    req: actix_web::HttpRequest,
) -> HttpResponse {
    let context = Context::new()
        .with_user_id(req.headers()
            .get("X-User-ID")
            .and_then(|h| h.to_str().ok())
            .unwrap_or(""))
        .with_ip_address(req.peer_addr()
            .map(|addr| addr.ip().to_string()));

    if data.flag_client.is_enabled("new-feature", Some(context)).await {
        HttpResponse::Ok().body("New feature!")
    } else {
        HttpResponse::Ok().body("Old feature")
    }
}

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    let config = Config::new("sdk_your_key");
    let client = FlagClient::new(config).unwrap();

    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(AppState {
                flag_client: client.clone(),
            }))
            .route("/", web::get().to(handler))
    })
    .bind("127.0.0.1:8080")?
    .run()
    .await
}
```

### Axum

```rust
use axum::{
    extract::State,
    routing::get,
    Router,
};
use otto_flags::{Config, Context, FlagClient};
use std::sync::Arc;

#[derive(Clone)]
struct AppState {
    flag_client: FlagClient,
}

async fn handler(State(state): State<Arc<AppState>>) -> String {
    let context = Context::new()
        .with_user_id("user-123")
        .with_environment("production");

    if state.flag_client.is_enabled("new-feature", Some(context)).await {
        "New feature!".to_string()
    } else {
        "Old feature".to_string()
    }
}

#[tokio::main]
async fn main() {
    let config = Config::new("sdk_your_key");
    let client = FlagClient::new(config).unwrap();

    let state = Arc::new(AppState {
        flag_client: client,
    });

    let app = Router::new()
        .route("/", get(handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:8080")
        .await
        .unwrap();

    axum::serve(listener, app).await.unwrap();
}
```

### Rocket

```rust
use rocket::{State, get, launch, routes};
use otto_flags::{Config, Context, FlagClient};

#[get("/")]
async fn index(client: &State<FlagClient>) -> String {
    let context = Context::new()
        .with_user_id("user-123");

    if client.is_enabled("new-feature", Some(context)).await {
        "New feature!".to_string()
    } else {
        "Old feature".to_string()
    }
}

#[launch]
fn rocket() -> _ {
    let config = Config::new("sdk_your_key");
    let client = FlagClient::new(config).unwrap();

    rocket::build()
        .manage(client)
        .mount("/", routes![index])
}
```

### Middleware Pattern

```rust
use otto_flags::{Context, FlagClient};

async fn feature_middleware(
    client: &FlagClient,
    user_id: &str,
    next: impl Future<Output = Response>,
) -> Response {
    let context = Context::new().with_user_id(user_id);

    if !client.is_enabled("feature-enabled", Some(context)).await {
        return Response::Forbidden("Feature not enabled");
    }

    next.await
}
```

### With Custom Context

```rust
use otto_flags::Context;
use serde_json::json;

let context = Context::new()
    .with_user_id("user-123")
    .with_session_id("session-abc")
    .with_environment("production")
    .with_attribute("plan", json!("premium"))
    .with_attribute("beta_user", json!(true))
    .with_attribute("country", json!("US"));

let result = client.evaluate("premium-features", Some(context)).await?;
```

## API Reference

### Types

#### Config

Configuration builder for the FlagClient.

```rust
let config = Config::new("sdk_key")
    .with_application_id("app-id")
    .with_base_url("https://otto-flags.savvagent.com")
    .with_cache_ttl(Duration::from_secs(60))
    .with_timeout(Duration::from_secs(5))
    .with_default("flag", false);
```

Results are cached for `cache_ttl` per flag key and evaluation context: a different context
(another user, other attributes) is evaluated by the server, and `invalidate_cache(Some(key))`
drops a flag's cached results for every context.

#### Context

Context builder for flag evaluation.

```rust
let context = Context::new()
    .with_user_id("user-123")
    .with_session_id("session-abc")
    .with_environment("production")
    .with_attribute("key", json!("value"));
```

#### EvaluationResult

Result from flag evaluation.

```rust
pub struct EvaluationResult {
    pub key: String,
    pub value: bool,
    pub configuration: Option<serde_json::Value>,
    pub variation: Option<String>,
    pub reason: String,  // "cached", "evaluated", "error"
    pub metadata: Option<Metadata>,  // None when reason is "error"
}

pub struct Metadata {
    pub version: Option<i64>,     // the flag version that produced the result
    pub scope: Option<String>,    // "application" or "enterprise"
    // flag_id, description, variant: not sent by the server; always None
}
```

### Methods

#### FlagClient::new(config: Config) -> Result<Self, OttoFlagsError>

Creates a new FlagClient.

```rust
let client = FlagClient::new(config)?;
```

#### evaluate(&self, flag_key: &str, context: Option<Context>) -> Result<EvaluationResult, OttoFlagsError>

Evaluates a feature flag. Flag keys are 1-128 ASCII letters, digits, `-`, `_` and `.` (but not
`.` or `..`), as the server requires; any other key returns its default with `reason: "error"`
and makes no request.

```rust
let result = client.evaluate("my-flag", Some(context)).await?;
```

#### is_enabled(&self, flag_key: &str, context: Option<Context>) -> bool

Checks if a flag is enabled (convenience method).

```rust
let enabled = client.is_enabled("my-flag", Some(context)).await;
```

#### invalidate_cache(&self, flag_key: Option<&str>)

Invalidates the cache.

```rust
client.invalidate_cache(Some("my-flag")); // Specific flag, every context
client.invalidate_cache(None);            // All flags
```

#### clear_cache(&self)

Clears all cached values.

```rust
client.clear_cache();
```

## Error Handling

A flag lookup never fails your request. `evaluate`, `is_enabled`, `get_config`, and
`get_variation` fall back to the flag's configured default (`with_default`, else `false`) when the
key is invalid, the server is unreachable or slow (`with_timeout`), it answers with an error
status, or the response is not an evaluate response. The fallback has `reason: "error"`, is logged
at `warn` through `tracing`, and is not cached, so the next lookup asks the server again. The
lookup methods keep their `Result` for compatibility but do not return `Err`, so `?` is safe.

```rust
let result = client.evaluate("my-flag", None).await?;
if result.reason == "error" {
    // The server's answer was unavailable; `result.value` is the default.
}
```

Only `FlagClient::new` returns `Err` in practice: `OttoFlagsError::InvalidApiKey` for a key that
does not start with `sdk_`, or `RequestFailed` if the HTTP client cannot be built.

## Best Practices

1. **Clone the Client**: The `FlagClient` is designed to be cloned cheaply
2. **Share State**: Use `Arc` or framework state management
3. **Context**: Provide rich context for better targeting
4. **Defaults**: Set sensible defaults for all flags
5. **Error Handling**: Handle errors gracefully
6. **Async Runtime**: Use Tokio for the async runtime

## Performance

The SDK is optimized for high-performance server environments:

- Thread-safe concurrent access with `Arc` and `RwLock`
- Efficient in-memory caching
- Zero-copy deserialization where possible
- Connection pooling via reqwest
- Configurable timeouts

## Logging

The SDK uses the `tracing` crate for logging. Configure your subscriber:

```rust
use tracing_subscriber;

tracing_subscriber::fmt::init();
```

## License

MIT

## Support

- Documentation: https://github.com/savvagent/otto-flags#readme
- API Docs: https://docs.rs/otto-flags
- Issues: https://github.com/savvagent/otto-flags/issues
- Email: support@savvagent.com
