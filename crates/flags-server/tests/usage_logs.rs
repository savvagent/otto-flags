//! Usage shipping keeps the platform's text out of logs above `debug` and out
//! of `usage_outbox.last_error` (#44): a failed shipment is stored by name and
//! status, and a refused event is logged with a fixed reason, the platform's
//! own reason only at `debug` and in `usage_outbox_rejected`.
//!
//! Its own test binary, with one test: `tracing` caches per callsite whether
//! any subscriber is interested, so another test in the same binary hitting
//! the same callsite with no subscriber can make this one miss its debug
//! events.

use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use otto_resource::{RejectedEvent, UsageBatch, UsageReceipt};
use sqlx::PgPool;
use uuid::Uuid;

const BODY_MARKER: &str = "body-marker-7f3a";
const REASON_MARKER: &str = "reason-marker-c91e";

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("capture lock").extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Capture {
    type Writer = Capture;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The first call answers 502 with a body carrying `BODY_MARKER`; every later
/// call rejects each event with a reason carrying `REASON_MARKER`.
async fn ship(State(calls): State<Arc<Mutex<usize>>>, Json(batch): Json<UsageBatch>) -> Response {
    let call = {
        let mut n = calls.lock().expect("calls lock");
        *n += 1;
        *n
    };
    if call == 1 {
        return (
            StatusCode::BAD_GATEWAY,
            format!("upstream said {BODY_MARKER}"),
        )
            .into_response();
    }
    let rejected = batch
        .events
        .iter()
        .map(|e| RejectedEvent {
            event_id: e.event_id,
            reason: format!("unknown org {REASON_MARKER}"),
        })
        .collect();
    Json(UsageReceipt {
        accepted: 0,
        duplicates: 0,
        rejected,
    })
    .into_response()
}

async fn start_platform() -> String {
    let app = Router::new()
        .route("/internal/usage", post(ship))
        .with_state(Arc::new(Mutex::new(0usize)));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

#[sqlx::test(migrator = "flags_core::MIGRATOR")]
async fn usage_shipping_keeps_platform_text_out_of_logs_and_the_outbox(pool: PgPool) {
    let url = start_platform().await;
    let mut config = flags_server::Config::for_test();
    config.platform_url = url;
    let platform = flags_server::platform_client(&config).unwrap();
    let db = otto_tenant::Db::from_pool(pool.clone());

    let event_id = sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO usage_outbox (org_id, user_id, tool, billable) \
         VALUES ($1, NULL, 'create_flag', true) RETURNING event_id",
    )
    .bind(Uuid::new_v4())
    .fetch_one(&pool)
    .await
    .unwrap();

    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(capture.clone())
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);

    let cfg = flags_core::usage::ShipperConfig::default();
    let err = flags_core::usage::ship_once(&db, &platform, &cfg)
        .await
        .unwrap_err();
    assert_eq!(err.failure_reason(), ("http_status", Some(502)));
    let last_error: Option<String> =
        sqlx::query_scalar("SELECT last_error FROM usage_outbox WHERE event_id = $1")
            .bind(event_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(last_error.as_deref(), Some("http_status 502"));

    sqlx::query("UPDATE usage_outbox SET next_attempt_at = now()")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        flags_core::usage::ship_once(&db, &platform, &cfg)
            .await
            .unwrap(),
        1
    );
    drop(guard);

    let logs = String::from_utf8(capture.0.lock().expect("capture lock").clone()).expect("utf-8");
    let lines: Vec<&str> = logs.lines().collect();
    for line in lines
        .iter()
        .filter(|l| l.contains(" ERROR ") || l.contains(" WARN ") || l.contains(" INFO "))
    {
        assert!(!line.contains(BODY_MARKER), "{line}");
        assert!(!line.contains(REASON_MARKER), "{line}");
    }
    let refused: Vec<&&str> = lines
        .iter()
        .filter(|l| l.contains(" ERROR ") && l.contains("refused a usage event"))
        .collect();
    assert_eq!(refused.len(), 1, "{logs}");
    assert!(refused[0].contains(&event_id.to_string()), "{}", refused[0]);
    assert!(refused[0].contains("reason=\"rejected\""), "{}", refused[0]);
    assert!(
        lines.iter().any(|l| l.contains(" DEBUG ")
            && l.contains(&event_id.to_string())
            && l.contains(REASON_MARKER)),
        "{logs}"
    );

    let reason: String =
        sqlx::query_scalar("SELECT reason FROM usage_outbox_rejected WHERE event_id = $1")
            .bind(event_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(reason, format!("unknown org {REASON_MARKER}"));
    let left: i64 = sqlx::query_scalar("SELECT count(*) FROM usage_outbox")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(left, 0);
}
