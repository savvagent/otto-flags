# Otto Flags Rust Server Example

Example Axum server that evaluates Otto Flags with the Otto Flags Rust Server SDK
(`packages/rust-server`, used here as a path dependency).

## What it shows

- Creating one `FlagClient` at startup and sharing it across handlers (it is cheap to clone)
- Evaluating flags per request with a `Context` (user ID, environment, attributes)
- Configured fallbacks: `is_enabled` never fails; if the API cannot be reached it returns the
  default set with `Config::with_default`, so a flag lookup never breaks a request
- axum 0.7: `tokio::net::TcpListener` + `axum::serve`

## Prerequisites

- Stable Rust and Cargo
- An Otto Flags app and its SDK key (`sdk_...`). An agent creates the app and gets its keys over
  MCP (`create_app`); the flags this example reads are `new-ui`, `beta-features`,
  `advanced-analytics`, and `advanced-processing`. Flags that do not exist evaluate to off.

## Configuration

| Variable | Required | Default | Meaning |
|---|---|---|---|
| `OTTO_FLAGS_SDK_KEY` | yes | | The app's SDK key; must start with `sdk_` |
| `OTTO_FLAGS_API_URL` | no | `https://otto-flags.savvagent.com` | Otto Flags API base URL |
| `OTTO_FLAGS_ENVIRONMENT` | no | `development` | Environment flags are evaluated in |
| `PORT` | no | `8083` | Port the example listens on |
| `RUST_LOG` | no | `rust_server_example=info,otto_flags=info,tower_http=info` | Log filter |

The server exits with an error if `OTTO_FLAGS_SDK_KEY` is missing or does not start with `sdk_`.

## Run

```bash
export OTTO_FLAGS_SDK_KEY=sdk_your_key_here
cargo run --release
```

The server listens on [http://localhost:8083](http://localhost:8083).

## API Endpoints

### Health check

```bash
curl http://localhost:8083/health
```

```json
{ "status": "ok" }
```

### Get user features

```bash
curl 'http://localhost:8083/api/features?userId=user-123'
```

Evaluates `new-ui`, `beta-features`, and `advanced-analytics` for the user:

```json
{
  "userId": "user-123",
  "features": {
    "newUI": true,
    "betaFeatures": false,
    "advancedAnalytics": true
  }
}
```

### Process data (feature-gated)

```bash
curl -X POST http://localhost:8083/api/data \
  -H 'Content-Type: application/json' \
  -d '{"userId": "user-123", "data": "example data"}'
```

Evaluates `advanced-processing` for the user (`anonymous` when `userId` is omitted), with an
`endpoint` attribute of `/api/data`, and reports which method it used:

```json
{ "processed": true, "method": "advanced", "data": "example data" }
```

## Usage in brief

```rust
use axum::{extract::State, routing::get, Router};
use otto_flags::{Config, Context, FlagClient};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::new(std::env::var("OTTO_FLAGS_SDK_KEY")?)
        .with_base_url("https://otto-flags.savvagent.com")
        .with_default("new-ui", false);
    let flags = FlagClient::new(config)?;

    let app = Router::new()
        .route("/", get(index))
        .with_state(flags);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:8083").await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn index(State(flags): State<FlagClient>) -> &'static str {
    let ctx = Context::new()
        .with_user_id("user-123")
        .with_environment("development");
    if flags.is_enabled("new-ui", Some(ctx)).await {
        "new UI"
    } else {
        "old UI"
    }
}
```

`src/main.rs` is the full example.

## Learn More

- [Otto Flags Rust SDK](../../packages/rust-server/README.md)
- [SDK Developer Guide](../../docs/SDK-DEVELOPER-GUIDE.md)
- [Axum](https://github.com/tokio-rs/axum)
- [Tokio](https://tokio.rs/)
