//! Telemetry against a mock server speaking the documented contract
//! (`docs/SDK-DEVELOPER-GUIDE.md`, Telemetry): `POST /api/telemetry/evaluations` and
//! `/api/telemetry/errors` with the client's key, batched (#93).

use otto_flags::{Config, Context, ErrorReport, FlagClient};
use serde_json::{json, Value};
use std::time::Duration;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

const KEY: &str = "srv_test_key";

async fn server_with(enabled: bool, telemetry_status: u16) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/flags/test-flag/evaluate"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "key": "test-flag", "enabled": enabled, "variation": "blue", "version": 1
        })))
        .mount(&server)
        .await;
    for kind in ["evaluations", "errors"] {
        Mock::given(method("POST"))
            .and(path(format!("/api/telemetry/{kind}")))
            .and(header("Authorization", format!("Bearer {KEY}").as_str()))
            .respond_with(ResponseTemplate::new(telemetry_status))
            .mount(&server)
            .await;
    }
    server
}

fn client(server: &MockServer) -> FlagClient {
    FlagClient::new(Config::new(KEY).with_base_url(server.uri())).unwrap()
}

async fn bodies(server: &MockServer, kind: &str) -> Vec<Value> {
    let suffix = format!("/api/telemetry/{kind}");
    server
        .received_requests()
        .await
        .unwrap_or_default()
        .iter()
        .filter(|r: &&Request| r.url.path() == suffix)
        .map(|r| serde_json::from_slice(&r.body).unwrap())
        .collect()
}

#[tokio::test]
async fn answered_lookups_are_reported_without_user_data() {
    let server = server_with(true, 202).await;
    let client = client(&server);
    let ctx = || {
        Some(
            Context::new()
                .with_user_id("user-1")
                .with_environment("staging")
                .with_attribute("email", json!("a@example.com")),
        )
    };

    assert!(client.evaluate("test-flag", ctx()).await.unwrap().value);
    let cached = client.evaluate("test-flag", ctx()).await.unwrap();
    assert_eq!(cached.reason, "cached");
    client.flush().await;

    let sent = bodies(&server, "evaluations").await;
    assert_eq!(sent.len(), 1);
    let events = sent[0]["evaluations"].as_array().unwrap();
    assert_eq!(events.len(), 2);
    for event in events {
        assert_eq!(event["flag_key"], "test-flag");
        assert_eq!(event["result"], true);
        assert_eq!(event["variation"], "blue");
        assert_eq!(event["environment"], "staging");
        assert!(event["timestamp"].is_u64());
        assert!(event.get("user_id").is_none());
        assert!(event.get("context").is_none());
    }
}

#[tokio::test]
async fn fallbacks_and_refused_keys_are_not_reported() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/flags/test-flag/evaluate"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/api/telemetry/evaluations"))
        .respond_with(ResponseTemplate::new(202))
        .expect(0)
        .mount(&server)
        .await;
    let client = client(&server);

    assert_eq!(
        client.evaluate("test-flag", None).await.unwrap().reason,
        "error"
    );
    assert_eq!(client.evaluate("a/b", None).await.unwrap().reason, "error");
    client.flush().await;
}

#[tokio::test]
async fn errors_are_reported_with_their_text_truncated() {
    let server = server_with(true, 202).await;
    let client = client(&server);

    client.track_error(
        ErrorReport::new("test-flag", true, "TypeError", "\u{e9}".repeat(5_000))
            .with_stack_trace("at main")
            .with_environment("production"),
    );
    client.flush().await;

    let sent = bodies(&server, "errors").await;
    assert_eq!(sent.len(), 1);
    let event = &sent[0]["errors"][0];
    assert_eq!(event["flag_key"], "test-flag");
    assert_eq!(event["flag_enabled"], true);
    assert_eq!(event["error_type"], "TypeError");
    assert_eq!(
        event["error_message"].as_str().unwrap(),
        "\u{e9}".repeat(2_000)
    );
    assert_eq!(event["stack_trace"], "at main");
    assert_eq!(event["environment"], "production");
    assert!(event["timestamp"].is_u64());
}

#[tokio::test]
async fn an_error_built_from_a_rust_error_names_its_type() {
    let server = server_with(true, 202).await;
    let client = client(&server);
    let error = "x".parse::<i32>().unwrap_err();

    client.track_error(ErrorReport::from_error("test-flag", false, &error));
    client.flush().await;

    let sent = bodies(&server, "errors").await;
    let event = &sent[0]["errors"][0];
    assert_eq!(event["flag_enabled"], false);
    assert!(event["error_type"]
        .as_str()
        .unwrap()
        .ends_with("ParseIntError"));
    assert_eq!(event["error_message"], error.to_string());
}

#[tokio::test]
async fn a_hundred_lookups_are_sent_without_waiting_for_flush() {
    let server = server_with(true, 202).await;
    let client = client(&server);
    for _ in 0..100 {
        client.evaluate("test-flag", None).await.unwrap();
    }

    let arrived = tokio::time::timeout(Duration::from_secs(2), async {
        while bodies(&server, "evaluations").await.is_empty() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(arrived.is_ok(), "no evaluations were sent");
    let sent = bodies(&server, "evaluations").await;
    assert_eq!(sent[0]["evaluations"].as_array().unwrap().len(), 100);
}

#[tokio::test]
async fn disabled_telemetry_sends_nothing() {
    let server = server_with(true, 202).await;
    let client = FlagClient::new(
        Config::new(KEY)
            .with_base_url(server.uri())
            .with_telemetry(false),
    )
    .unwrap();
    client.evaluate("test-flag", None).await.unwrap();
    client.track_error(ErrorReport::new("test-flag", true, "E", "m"));
    client.flush().await;

    assert!(bodies(&server, "evaluations").await.is_empty());
    assert!(bodies(&server, "errors").await.is_empty());
}

#[tokio::test]
async fn a_failing_telemetry_endpoint_changes_no_lookup() {
    let server = server_with(true, 500).await;
    let client = FlagClient::new(
        Config::new(KEY)
            .with_base_url(server.uri())
            .with_timeout(Duration::from_millis(500)),
    )
    .unwrap();

    assert!(client.evaluate("test-flag", None).await.unwrap().value);
    client.track_error(ErrorReport::new("test-flag", true, "E", "m"));
    tokio::time::timeout(Duration::from_secs(5), client.flush())
        .await
        .expect("flush finished");
    assert!(client.is_enabled("test-flag", None).await);
}
