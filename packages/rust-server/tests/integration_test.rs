//! The SDK against a mock otto-flags server speaking the documented contract
//! (`docs/SDK-DEVELOPER-GUIDE.md`, Flag Evaluation): `POST /api/flags/{key}/evaluate`
//! with `Authorization: Bearer <key>` and `{"context": ...}`, answered with `enabled`.

use otto_flags::{Config, Context, FlagClient, OttoFlagsError, VariationResult};
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
    // The server sends `"configuration": null` for a flag without one; that is absent.
    assert!(result.configuration.is_none());
    assert!(result.variation.is_none());
}

#[tokio::test]
async fn get_config_is_none_for_an_enabled_flag_without_configuration() {
    let server = MockServer::start().await;
    mock_evaluate(
        &server,
        "test-flag",
        200,
        json!({ "key": "test-flag", "enabled": true, "variation": null, "configuration": null }),
    )
    .await;

    let config = client(&server).get_config("test-flag", None).await.unwrap();
    assert!(config.is_none());
}

#[tokio::test]
async fn evaluate_sends_the_configured_application_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/flags/test-flag/evaluate"))
        .and(body_partial_json(
            json!({ "context": { "application_id": "app-1" } }),
        ))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "enabled": true })))
        .expect(1)
        .mount(&server)
        .await;

    let client = FlagClient::new(
        Config::new(KEY)
            .with_base_url(server.uri())
            .with_application_id("app-1"),
    )
    .unwrap();
    assert!(client.is_enabled("test-flag", None).await);
}

#[tokio::test]
async fn a_response_without_enabled_falls_back_to_the_default() {
    let server = MockServer::start().await;
    // `value` is the field the SDK used to read; the server never sends it.
    mock_evaluate(&server, "test-flag", 200, json!({ "value": false })).await;

    let client = FlagClient::new(
        Config::new(KEY)
            .with_base_url(server.uri())
            .with_default("test-flag", true),
    )
    .unwrap();
    assert!(client.is_enabled("test-flag", None).await);
}

#[tokio::test]
async fn a_second_evaluation_is_served_from_the_cache() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/flags/test-flag/evaluate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "enabled": true })))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server);
    assert_eq!(
        client.evaluate("test-flag", None).await.unwrap().reason,
        "evaluated"
    );
    let cached = client.evaluate("test-flag", None).await.unwrap();
    assert!(cached.value);
    assert_eq!(cached.reason, "cached");
}

#[tokio::test]
async fn an_unreachable_server_falls_back_to_the_default() {
    // Bind a port, then free it, so nothing is listening there. (A dropped `MockServer`
    // goes back to wiremock's pool and keeps listening.)
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let uri = format!("http://127.0.0.1:{port}");

    let client = FlagClient::new(
        Config::new(KEY)
            .with_base_url(uri)
            .with_default("test-flag", true),
    )
    .unwrap();
    assert!(matches!(
        client.evaluate("test-flag", None).await,
        Err(OttoFlagsError::RequestFailed(_))
    ));
    assert!(client.is_enabled("test-flag", None).await);
}

#[tokio::test]
async fn unknown_flag_falls_back_to_the_default() {
    let server = MockServer::start().await;
    // Errors are not cached: `evaluate` and `is_enabled` each reach the server.
    Mock::given(method("POST"))
        .and(path("/api/flags/missing-flag/evaluate"))
        .and(header("Authorization", format!("Bearer {KEY}").as_str()))
        .respond_with(
            ResponseTemplate::new(404).set_body_json(json!({ "error": "flag_not_found" })),
        )
        .expect(2)
        .mount(&server)
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

    let client = FlagClient::new(
        Config::new(KEY)
            .with_base_url(server.uri())
            .with_default("test-flag", true),
    )
    .unwrap();
    let result = client.evaluate("test-flag", None).await.unwrap();
    assert!(result.value);
    assert_eq!(result.reason, "error");
    assert!(client.is_enabled("test-flag", None).await);
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
    assert!(matches!(
        FlagClient::new(Config::new("not-an-sdk-key")),
        Err(OttoFlagsError::InvalidApiKey)
    ));
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

#[tokio::test]
async fn different_users_each_get_their_own_cached_answer() {
    let server = MockServer::start().await;
    for (user, enabled) in [("user-a", true), ("user-b", false)] {
        Mock::given(method("POST"))
            .and(path("/api/flags/targeted/evaluate"))
            .and(body_partial_json(json!({ "context": { "user_id": user } })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "enabled": enabled })))
            .expect(1)
            .mount(&server)
            .await;
    }

    let client = client(&server);
    let a = || Some(Context::new().with_user_id("user-a"));
    let b = || Some(Context::new().with_user_id("user-b"));

    let first_a = client.evaluate("targeted", a()).await.unwrap();
    let first_b = client.evaluate("targeted", b()).await.unwrap();
    assert!(first_a.value);
    assert!(!first_b.value);
    assert_eq!(first_b.reason, "evaluated");

    // Within the TTL each context is served its own cached answer.
    let again_a = client.evaluate("targeted", a()).await.unwrap();
    let again_b = client.evaluate("targeted", b()).await.unwrap();
    assert!(again_a.value);
    assert!(!again_b.value);
    assert_eq!(again_a.reason, "cached");
    assert_eq!(again_b.reason, "cached");
}

#[tokio::test]
async fn contexts_differing_in_an_attribute_each_get_their_own_answer() {
    let server = MockServer::start().await;
    for (plan, enabled) in [("pro", true), ("free", false)] {
        Mock::given(method("POST"))
            .and(path("/api/flags/targeted/evaluate"))
            .and(body_partial_json(
                json!({ "context": { "attributes": { "plan": plan } } }),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "enabled": enabled })))
            .expect(1)
            .mount(&server)
            .await;
    }

    let client = client(&server);
    let ctx = |plan: &str| {
        Some(
            Context::new()
                .with_user_id("user-1")
                .with_attribute("plan", json!(plan)),
        )
    };
    assert!(client.evaluate("targeted", ctx("pro")).await.unwrap().value);
    assert!(
        !client
            .evaluate("targeted", ctx("free"))
            .await
            .unwrap()
            .value
    );
}

#[tokio::test]
async fn an_identical_context_hits_the_cache_whatever_the_attribute_order() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/flags/test-flag/evaluate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "enabled": true })))
        .expect(1)
        .mount(&server)
        .await;

    let client = client(&server);
    let first = Context::new()
        .with_user_id("user-1")
        .with_attribute("a", json!(1))
        .with_attribute("b", json!({ "x": 1, "y": 2 }));
    let second = Context::new()
        .with_attribute("b", json!({ "y": 2, "x": 1 }))
        .with_attribute("a", json!(1))
        .with_user_id("user-1");
    client.evaluate("test-flag", Some(first)).await.unwrap();
    let cached = client.evaluate("test-flag", Some(second)).await.unwrap();
    assert_eq!(cached.reason, "cached");
}

#[tokio::test]
async fn invalidating_a_flag_drops_its_cached_answer_for_every_context() {
    let server = MockServer::start().await;
    for user in ["user-a", "user-b"] {
        Mock::given(method("POST"))
            .and(path("/api/flags/targeted/evaluate"))
            .and(body_partial_json(json!({ "context": { "user_id": user } })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "enabled": true })))
            .expect(2)
            .mount(&server)
            .await;
    }

    let client = client(&server);
    for user in ["user-a", "user-b"] {
        let ctx = Some(Context::new().with_user_id(user));
        client.evaluate("targeted", ctx).await.unwrap();
    }
    client.invalidate_cache(Some("targeted"));
    for user in ["user-a", "user-b"] {
        let ctx = Some(Context::new().with_user_id(user));
        let result = client.evaluate("targeted", ctx).await.unwrap();
        assert_eq!(result.reason, "evaluated");
    }
}

#[tokio::test]
async fn the_configured_application_id_is_part_of_the_cached_context() {
    let server = MockServer::start().await;
    for app in ["app-1", "app-2"] {
        Mock::given(method("POST"))
            .and(path("/api/flags/test-flag/evaluate"))
            .and(body_partial_json(
                json!({ "context": { "application_id": app } }),
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({ "enabled": app == "app-1" })),
            )
            .expect(1)
            .mount(&server)
            .await;
    }

    let client = FlagClient::new(
        Config::new(KEY)
            .with_base_url(server.uri())
            .with_application_id("app-1"),
    )
    .unwrap();

    // `None`, an empty context, and one naming the configured app are the same request.
    assert!(client.evaluate("test-flag", None).await.unwrap().value);
    let empty = client
        .evaluate("test-flag", Some(Context::new()))
        .await
        .unwrap();
    assert_eq!(empty.reason, "cached");
    let mut same_app = Context::new();
    same_app.application_id = Some("app-1".into());
    let same = client.evaluate("test-flag", Some(same_app)).await.unwrap();
    assert_eq!(same.reason, "cached");

    // Another app is another context.
    let mut other_app = Context::new();
    other_app.application_id = Some("app-2".into());
    let other = client.evaluate("test-flag", Some(other_app)).await.unwrap();
    assert_eq!(other.reason, "evaluated");
    assert!(!other.value);
}
