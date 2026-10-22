# otto-flags Rust SDK (app: {{APP_NAME}})

`otto-flags` (imported as `otto_flags`) is the server-side Rust SDK: an async client built on Tokio
and `reqwest`. It authenticates with the app's server key. That key is shown once, by `create_app`
or `rotate_app_keys`; keep it in the app's secret store and read it from the `OTTO_FLAGS_API_KEY`
environment variable. Never write it into code, a config file under version control, or a client
bundle. The client also accepts the public client key, but only telemetry sent with the server key
drives auto-rollback by default. Rust 1.70 or later.

## Install

The crate is not installable by name yet. Depend on it from git, pinned to a full commit SHA of
`savvagent/otto-flags`. The crate lives in `packages/rust-server`; Cargo finds it by its name.
Find the current tip of `main` with:

```bash
git ls-remote https://github.com/savvagent/otto-flags main
```

and use that SHA (or another one you have checked) in place of `<commit>`. In `Cargo.toml`:

```toml
[dependencies]
otto-flags = { git = "https://github.com/savvagent/otto-flags", rev = "<commit>" }
tokio = { version = "1", features = ["full"] }
```

or from the command line:

```bash
cargo add otto-flags --git https://github.com/savvagent/otto-flags --rev <commit>
```

Pin a `rev`, not a branch, so a build is reproducible. `Cargo.lock` then records the same commit.

## Initialize

Create one client per process; `FlagClient` is cheap to clone, so share it through your framework's
state.

```rust
use otto_flags::{Config, FlagClient};

fn build_client() -> Result<FlagClient, Box<dyn std::error::Error>> {
    let key = std::env::var("OTTO_FLAGS_API_KEY")?;
    let config = Config::new(key)
        .with_application_id("{{APP_ID}}")
        .with_default("new-checkout", false);
    Ok(FlagClient::new(config)?)
}
```

`FlagClient::new` is the only call that returns an error in practice: a key that starts with
neither of the two key prefixes fails there, so a misconfigured deploy stops at start-up.

Other `Config` builders, all optional: `with_base_url` (default `https://otto-flags.savvagent.com`,
use `https`), `with_cache_ttl` (default 60 seconds), `with_timeout` (default 5 seconds),
`with_failure_cooldown` (default 5 seconds), `with_telemetry` (default on) and `with_default` (flag
key to the value served when evaluation fails).

## Evaluate a flag

Pass an optional `Context` describing who the evaluation is for. Percentage rollouts bucket on the
user id (or session id), so pass a stable identifier.

```rust
use otto_flags::Context;

let context = Context::new()
    .with_user_id("user-123")
    .with_environment("production")
    .with_attribute("plan", serde_json::json!("pro"));

if client.is_enabled("new-checkout", Some(context.clone())).await {
    render_new_checkout();
} else {
    render_old_checkout();
}

let result = client.evaluate("new-checkout", Some(context)).await?;
// result.value (bool), result.variation, result.configuration, result.reason
```

Also available: `get_config` for configuration attached to a flag, `get_variation` for
multi-variant flags, and `track_error` to report an error raised in flagged code, which is what
`flag_health` and auto-rollback compare:

```rust
use otto_flags::ErrorReport;

if let Err(e) = charge(&order).await {
    client.track_error(ErrorReport::from_error("new-checkout", enabled, &e));
}
```

Call `client.flush().await` before the process exits, or the last few seconds of telemetry are
lost. `invalidate_cache(Some("flag-key"))` drops a flag's cached results; `invalidate_cache(None)`
drops them all.

## Frameworks

### Axum

Put the client in the router state:

```rust
use axum::{extract::State, routing::get, Router};
use otto_flags::{Context, FlagClient};

async fn checkout(State(client): State<FlagClient>) -> String {
    let ctx = Context::new().with_user_id("user-123");
    if client.is_enabled("new-checkout", Some(ctx)).await {
        "new checkout".into()
    } else {
        "old checkout".into()
    }
}

#[tokio::main]
async fn main() {
    let client = build_client().expect("otto-flags");
    let app = Router::new().route("/checkout", get(checkout)).with_state(client);
    let listener = tokio::net::TcpListener::bind("0.0.0.0:8080").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```

### Actix Web

Hand the client out as `web::Data` and read it in handlers (`data.is_enabled(...)` is awaited as
above):

```rust
HttpServer::new(move || {
    App::new()
        .app_data(web::Data::new(client.clone()))
        .route("/checkout", web::get().to(checkout))
})
```

## Notes

- A flag lookup never fails the request: `evaluate`, `is_enabled`, `get_config` and
  `get_variation` fall back to the configured default (else `false`) when the key is invalid, the
  server is unreachable or slow, or it answers with an error. The fallback has
  `reason == "error"`, is logged through `tracing`, and is not cached. `evaluate` keeps its
  `Result` but does not return `Err`, so `?` is safe. After a failed lookup the client backs off
  for the failure cooldown and serves defaults without a request.
- Telemetry (evaluation counts and tracked errors) is batched and sent in the background; it feeds
  `flag_health` and auto-rollback. Keep end-user data out of error messages: they are stored and
  shown to the agents that manage your flags. `with_telemetry(false)` turns it off.
- Create flags before code references them: an unknown flag key evaluates to its default.
- The wire contract the SDK speaks is `docs/SDK-DEVELOPER-GUIDE.md` in the repository.
