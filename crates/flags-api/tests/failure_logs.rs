//! SDK request and webhook failures are logged by code and reason above
//! debug, their text only at debug (#35).
//!
//! Its own test binary, with one test: `tracing` caches per callsite whether
//! any subscriber is interested, so another test in the same binary hitting
//! the same callsite with no subscriber can make this one miss its debug
//! events.

use std::sync::{Arc, Mutex};

use axum::body::Body;
use flags_api::{ApiError, AppState};
use flags_core::notify::Listener;
use flags_core::rollback::Hints;
use flags_core::Error;
use http::{Request, StatusCode};
use tower::ServiceExt;

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

/// The lines `f` logged above `DEBUG` (at `ERROR` or `WARN`) and at `DEBUG`.
fn capture(f: impl FnOnce()) -> (Vec<String>, Vec<String>) {
    let out = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(out.clone())
        .finish();
    tracing::subscriber::with_default(subscriber, f);
    let logs = String::from_utf8(out.0.lock().expect("capture lock").clone()).expect("utf-8");
    let at = |level: &str| -> Vec<String> {
        logs.lines()
            .filter(|l| l.contains(level))
            .map(str::to_string)
            .collect()
    };
    let mut above = at(" ERROR ");
    above.extend(at(" WARN "));
    (above, at(" DEBUG "))
}

const SECRET: &str = "otto_whsec_test";

/// Deliver `body` to the webhook route, signed with `secret`, and return the
/// status. The pool is lazy and never touched: both cases fail before apply.
fn deliver(secret: &str, body: Vec<u8>) -> StatusCode {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    rt.block_on(async {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused@localhost/unused")
            .expect("lazy pool");
        let app = flags_api::router(AppState::new(
            otto_tenant::Db::from_pool(pool),
            Listener::detached(),
            Hints::detached(),
            SECRET,
        ));
        let sig = otto_resource::webhook::sign(secret, chrono::Utc::now().timestamp(), &body);
        let req = Request::post("/platform/webhooks")
            .header(otto_resource::webhook::SIGNATURE_HEADER, sig)
            .body(Body::from(body))
            .expect("request");
        app.oneshot(req).await.expect("response").status()
    })
}

/// A signed event whose data serde rejects, quoting the value it found.
fn unreadable_body() -> Vec<u8> {
    otto_resource::webhook::body(
        uuid::Uuid::nil(),
        "org.deleted",
        chrono::Utc::now(),
        &serde_json::json!({ "org_id": 4242424242u64 }),
    )
}

/// The text that must stay at debug, the fields expected above it, and the call.
type Case = (&'static str, &'static str, Box<dyn FnOnce()>);

#[test]
fn failures_log_code_and_reason_above_debug_and_text_only_at_debug() {
    let cases: Vec<Case> = vec![
        (
            "MARKER_DB",
            r#"code="internal_error" reason="protocol""#,
            Box::new(|| {
                let api = ApiError::from(Error::Db(sqlx::Error::Protocol("MARKER_DB".into())));
                assert_eq!(api.status, http::StatusCode::SERVICE_UNAVAILABLE);
                assert!(!api.message.contains("MARKER"), "{}", api.message);
            }),
        ),
        (
            "MARKER_PLATFORM",
            r#"code="platform_unavailable" reason="http_status" status=503"#,
            Box::new(|| {
                let api = ApiError::from(Error::Platform(otto_resource::Error::Status {
                    status: 503,
                    body: "MARKER_PLATFORM".into(),
                }));
                assert_eq!(api.status, http::StatusCode::SERVICE_UNAVAILABLE);
                assert!(!api.message.contains("MARKER"), "{}", api.message);
            }),
        ),
        (
            // serde's text quotes the value it rejected.
            "4242424242",
            r#"reason="bad_body""#,
            Box::new(|| {
                assert_eq!(deliver(SECRET, unreadable_body()), StatusCode::BAD_REQUEST);
            }),
        ),
        (
            "signature does not match",
            r#"reason="bad_signature""#,
            Box::new(|| {
                assert_eq!(
                    deliver("otto_whsec_wrong", unreadable_body()),
                    StatusCode::UNAUTHORIZED
                );
            }),
        ),
    ];
    for (marker, fields, call) in cases {
        let (errors, debugs) = capture(call);
        assert!(
            errors.iter().any(|l| l.contains(fields)),
            "{fields} above DEBUG: {errors:#?}"
        );
        assert!(
            errors.iter().all(|l| !l.contains(marker)),
            "{marker} must not be above DEBUG: {errors:#?}"
        );
        assert!(
            debugs.iter().any(|l| l.contains(marker)),
            "{marker} at DEBUG: {debugs:#?}"
        );
    }
}
