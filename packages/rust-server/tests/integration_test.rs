//! The SDK against a mock otto-flags server speaking the documented contract
//! (`docs/SDK-DEVELOPER-GUIDE.md`, Flag Evaluation): `POST /api/flags/{key}/evaluate`
//! with `Authorization: Bearer <key>` and `{"context": ...}`, answered with `enabled`.

use otto_flags::{Config, Context, FlagClient, VariationResult};
use serde_json::json;
use wiremock::matchers::{body_partial_json, header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "sdk_test_key";

fn client(server: &MockServer) -> FlagClient {
    FlagClient::new(Config::new(KEY).with_base_url(server.uri())).unwrap()
}

async fn mock_evaluate(server: &MockServer, flag: &str, status: u16, body: serde_json::Value) {
    Mock::given(method("POST"))
        .and(path(format!("/api/flags/{flag}/evaluate")))
        .and(header("Authorization", format!("Bearer {KEY}").as_str()))
        .respond_with(ResponseTemplate::new(status).set_body_json(body))
        .mount(server)
        .await;
}

#[tokio::test]
async fn is_enabled_returns_true_when_the_server_enables_the_flag() {
    let server = MockServer::start().await;
    mock_evaluate(
        &server,
        "test-flag",
        200,
        json!({ "key": "test-flag", "enabled": true }),
    )
    .await;

    let ctx = Context::new().with_user_id("user-123");
    assert!(client(&server).is_enabled("test-flag", Some(ctx)).await);
}

#[tokio::test]
async fn is_enabled_returns_false_when_the_server_disables_the_flag() {
    let server = MockServer::start().await;
    mock_evaluate(
        &server,
        "test-flag",
        200,
        json!({ "key": "test-flag", "enabled": false }),
    )
    .await;

    // A configured default of true proves `false` came from the server, not a fallback.
    let client = FlagClient::new(
        Config::new(KEY)
            .with_base_url(server.uri())
            .with_default("test-flag", true),
    )
    .unwrap();
    let ctx = Context::new().with_user_id("user-123");
    assert!(!client.is_enabled("test-flag", Some(ctx)).await);
}

#[tokio::test]
async fn evaluate_sends_the_context_and_reports_the_server_result() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/flags/test-flag/evaluate"))
        .and(header("Authorization", format!("Bearer {KEY}").as_str()))
        .and(body_partial_json(json!({
            "context": {
                "user_id": "user-123",
                "environment": "staging",
                "attributes": { "email": "test@example.com", "plan": "pro" }
            }
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "key": "test-flag",
            "enabled": true,
            "scope": "application",
            "variation": null,
            "configuration": null,
            "timestamp": 1_700_000_000,
            "version": 3
        })))
        .expect(1)
        .mount(&server)
        .await;

    let ctx = Context::new()
        .with_user_id("user-123")
        .with_environment("staging")
        .with_attribute("email", json!("test@example.com"))
        .with_attribute("plan", json!("pro"));
    let result = client(&server)
        .evaluate("test-flag", Some(ctx))
        .await
        .unwrap();

    assert!(result.value);
    assert_eq!(result.key, "test-flag");
    assert_eq!(result.reason, "evaluated");
}

#[tokio::test]
async fn unknown_flag_falls_back_to_the_default() {
    let server = MockServer::start().await;
    mock_evaluate(
        &server,
        "missing-flag",
        404,
        json!({ "error": "flag_not_found" }),
    )
    .await;

    let client = FlagClient::new(
        Config::new(KEY)
            .with_base_url(server.uri())
            .with_default("missing-flag", true),
    )
    .unwrap();

    let result = client.evaluate("missing-flag", None).await.unwrap();
    assert!(result.value);
    assert_eq!(result.reason, "error");
    assert!(client.is_enabled("missing-flag", None).await);
}

#[tokio::test]
async fn server_error_falls_back_to_the_default() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/flags/test-flag/evaluate"))
        .respond_with(ResponseTemplate::new(500).set_body_string("Internal server error"))
        .mount(&server)
        .await;

    let client = client(&server);
    let result = client.evaluate("test-flag", None).await.unwrap();
    assert!(!result.value);
    assert_eq!(result.reason, "error");
    assert!(!client.is_enabled("test-flag", None).await);
}

#[tokio::test]
async fn get_variation_returns_the_server_variation() {
    let server = MockServer::start().await;
    mock_evaluate(
        &server,
        "theme",
        200,
        json!({
            "key": "theme",
            "enabled": true,
            "variation": "dark-mode",
            "configuration": { "background": "#000" }
        }),
    )
    .await;

    let VariationResult {
        variation,
        enabled,
        configuration,
    } = client(&server).get_variation("theme", None).await.unwrap();
    assert_eq!(variation, "dark-mode");
    assert!(enabled);
    assert_eq!(configuration.unwrap()["background"], "#000");
}

#[tokio::test]
async fn get_variation_falls_back_to_control_on_error() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/flags/theme/evaluate"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let result = client(&server).get_variation("theme", None).await.unwrap();
    assert_eq!(result.variation, "control");
    assert!(!result.enabled);
    assert!(result.configuration.is_none());
}

#[test]
fn config_builder_sets_its_fields() {
    let config = Config::new("sdk_key")
        .with_application_id("app-1")
        .with_base_url("https://flags.example.com")
        .with_cache_ttl(std::time::Duration::from_secs(30))
        .with_timeout(std::time::Duration::from_secs(2))
        .with_default("beta", true);

    assert_eq!(config.api_key, "sdk_key");
    assert_eq!(config.application_id.as_deref(), Some("app-1"));
    assert_eq!(config.base_url, "https://flags.example.com");
    assert_eq!(config.cache_ttl, std::time::Duration::from_secs(30));
    assert_eq!(config.timeout, std::time::Duration::from_secs(2));
    assert_eq!(config.defaults.get("beta"), Some(&true));
}

#[test]
fn client_rejects_a_key_without_the_sdk_prefix() {
    assert!(FlagClient::new(Config::new("not-an-sdk-key")).is_err());
}

#[test]
fn context_builder_sets_its_fields() {
    let context = Context::new()
        .with_user_id("user-123")
        .with_session_id("session-1")
        .with_environment("production")
        .with_attribute("email", json!("test@example.com"))
        .with_attribute("plan", json!("pro"))
        .with_attribute("age", json!(30));

    assert_eq!(context.user_id.as_deref(), Some("user-123"));
    assert_eq!(context.session_id.as_deref(), Some("session-1"));
    assert_eq!(context.environment.as_deref(), Some("production"));
    let attributes = context.attributes.unwrap();
    assert_eq!(attributes["email"], "test@example.com");
    assert_eq!(attributes["plan"], "pro");
    assert_eq!(attributes["age"], 30);
}
