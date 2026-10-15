//! Lookups back off after a failure (#111): during the cooldown a lookup returns its default
//! without a request, the first one after it reaches the server, `Retry-After` lengthens the
//! window, a 404 cools down only its own flag, and an oversized body is a failed lookup.

use otto_flags::{Config, FlagClient};
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "sdk_test_key";
/// The SDK's response cap (`MAX_RESPONSE_BYTES` in `src/client.rs`).
const MAX: usize = 1024 * 1024;

/// A client whose flags all default to `true`, so a fallback differs from an answer of `false`.
fn client(base_url: String, cooldown: Duration, flags: &[&str]) -> FlagClient {
    let mut config = Config::new(KEY)
        .with_base_url(base_url)
        .with_failure_cooldown(cooldown);
    for flag in flags {
        config = config.with_default(*flag, true);
    }
    FlagClient::new(config).unwrap()
}

async fn requests(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

async fn requests_to(server: &MockServer, flag: &str) -> usize {
    let wanted = format!("/api/flags/{flag}/evaluate");
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path() == wanted)
        .count()
}

async fn mock_status(server: &MockServer, template: ResponseTemplate) {
    Mock::given(method("POST"))
        .respond_with(template)
        .mount(server)
        .await;
}

async fn mock_flag(server: &MockServer, flag: &str, template: ResponseTemplate) {
    Mock::given(method("POST"))
        .and(path(format!("/api/flags/{flag}/evaluate")))
        .respond_with(template)
        .mount(server)
        .await;
}

#[tokio::test]
async fn requests_during_the_cooldown_are_skipped_and_the_next_one_after_reaches_the_server() {
    let server = MockServer::start().await;
    mock_status(&server, ResponseTemplate::new(500)).await;
    let client = client(server.uri(), Duration::from_millis(300), &["a", "b"]);

    let first = client.evaluate("a", None).await.unwrap();
    assert_eq!(first.reason, "error");
    assert_eq!(requests(&server).await, 1);

    for flag in ["a", "b", "a"] {
        let r = client.evaluate(flag, None).await.unwrap();
        assert_eq!(r.reason, "error");
        assert!(r.value, "the default");
    }
    assert_eq!(requests(&server).await, 1, "skipped during the cooldown");

    tokio::time::sleep(Duration::from_millis(350)).await;
    client.evaluate("a", None).await.unwrap();
    assert_eq!(requests(&server).await, 2, "the first lookup after it");
}

async fn retry_after_holds_the_window(status: u16) {
    let server = MockServer::start().await;
    mock_status(
        &server,
        ResponseTemplate::new(status).insert_header("Retry-After", "1"),
    )
    .await;
    let client = client(server.uri(), Duration::from_millis(100), &["a"]);

    client.evaluate("a", None).await.unwrap();
    assert_eq!(requests(&server).await, 1);

    tokio::time::sleep(Duration::from_millis(300)).await;
    client.evaluate("a", None).await.unwrap();
    assert_eq!(requests(&server).await, 1, "still inside Retry-After");

    tokio::time::sleep(Duration::from_millis(800)).await;
    client.evaluate("a", None).await.unwrap();
    assert_eq!(requests(&server).await, 2, "Retry-After has passed");
}

#[tokio::test]
async fn retry_after_on_503_holds_the_window_past_the_cooldown() {
    retry_after_holds_the_window(503).await;
}

#[tokio::test]
async fn retry_after_on_429_holds_the_window_past_the_cooldown() {
    retry_after_holds_the_window(429).await;
}

#[tokio::test]
async fn a_missing_flag_does_not_stop_lookups_of_other_flags() {
    let server = MockServer::start().await;
    mock_flag(&server, "missing", ResponseTemplate::new(404)).await;
    mock_flag(
        &server,
        "present",
        ResponseTemplate::new(200).set_body_json(json!({ "enabled": false })),
    )
    .await;
    let client = client(
        server.uri(),
        Duration::from_secs(30),
        &["missing", "present"],
    );

    assert_eq!(
        client.evaluate("missing", None).await.unwrap().reason,
        "error"
    );
    let present = client.evaluate("present", None).await.unwrap();
    assert_eq!(present.reason, "evaluated");
    assert!(!present.value);

    assert_eq!(
        client.evaluate("missing", None).await.unwrap().reason,
        "error"
    );
    assert_eq!(
        requests_to(&server, "missing").await,
        1,
        "skipped the second time"
    );
}

fn body_of_len(len: usize) -> Vec<u8> {
    let head = br#"{"enabled":true,"pad":""#;
    let tail = br#""}"#;
    let mut body = head.to_vec();
    body.resize(len - tail.len(), b'x');
    body.extend_from_slice(tail);
    assert_eq!(body.len(), len);
    body
}

#[tokio::test]
async fn an_oversized_body_falls_back_for_that_flag_only() {
    let server = MockServer::start().await;
    let big = String::from_utf8(body_of_len(MAX + 1))
        .unwrap()
        .replace("true", "false");
    mock_flag(
        &server,
        "big",
        ResponseTemplate::new(200)
            .insert_header("Content-Type", "application/json")
            .set_body_string(big),
    )
    .await;
    mock_flag(
        &server,
        "other",
        ResponseTemplate::new(200).set_body_json(json!({ "enabled": true })),
    )
    .await;
    let client = client(server.uri(), Duration::from_secs(30), &["big", "other"]);

    let r = client.evaluate("big", None).await.unwrap();
    assert_eq!(r.reason, "error");
    assert!(r.value, "the default, not the body's false");

    // Per-flag scope: another flag still reaches the server.
    let other = client.evaluate("other", None).await.unwrap();
    assert_eq!(other.reason, "evaluated");
}

#[tokio::test]
async fn an_oversized_body_without_content_length_falls_back() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let serve = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        // Read the request head (and body) as far as it is there; its content is not used.
        let mut buf = vec![0u8; 8192];
        let _ = socket.read(&mut buf).await;
        let head = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                    Transfer-Encoding: chunked\r\nConnection: close\r\n\r\n";
        let _ = socket.write_all(head.as_bytes()).await;
        let body = body_of_len(MAX + 1);
        for chunk in body.chunks(64 * 1024) {
            let frame = format!("{:x}\r\n", chunk.len());
            // The client may hang up once it has read past the cap.
            if socket.write_all(frame.as_bytes()).await.is_err()
                || socket.write_all(chunk).await.is_err()
                || socket.write_all(b"\r\n").await.is_err()
            {
                return;
            }
        }
        let _ = socket.write_all(b"0\r\n\r\n").await;
        let _ = socket.shutdown().await;
    });

    let client = client(format!("http://{addr}"), Duration::from_secs(30), &["big"]);
    let r = client.evaluate("big", None).await.unwrap();
    assert_eq!(r.reason, "error");
    assert!(r.value);
    serve.abort();
}

#[tokio::test]
async fn a_body_exactly_at_the_cap_is_read() {
    let server = MockServer::start().await;
    mock_flag(
        &server,
        "edge",
        ResponseTemplate::new(200)
            .insert_header("Content-Type", "application/json")
            .set_body_bytes(body_of_len(MAX)),
    )
    .await;
    let mut config = Config::new(KEY)
        .with_base_url(server.uri())
        .with_default("edge", false);
    config = config.with_failure_cooldown(Duration::from_secs(30));
    let client = FlagClient::new(config).unwrap();

    let r = client.evaluate("edge", None).await.unwrap();
    assert_eq!(r.reason, "evaluated");
    assert!(r.value);
}

#[tokio::test]
async fn a_zero_cooldown_asks_the_server_every_time() {
    let server = MockServer::start().await;
    mock_status(&server, ResponseTemplate::new(500)).await;
    let client = client(server.uri(), Duration::ZERO, &["a"]);

    client.evaluate("a", None).await.unwrap();
    client.evaluate("a", None).await.unwrap();
    assert_eq!(requests(&server).await, 2);
}
