//! Server-side failures are logged by code and reason at error, their text
//! only at debug (#29).
//!
//! Its own test binary, with one test: `tracing` caches per callsite whether
//! any subscriber is interested, so a unit test elsewhere in the crate hitting
//! the same callsite with no subscriber can make this one miss its debug
//! events.

use std::sync::{Arc, Mutex};

use flags_core::Error;
use flags_mcp::error::{from_core, from_platform, from_tenant};

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

/// The lines `f` logged at `ERROR` and at `DEBUG`.
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
    (at(" ERROR "), at(" DEBUG "))
}

/// The text that must stay at debug, the fields expected at error, and the call.
type Case = (&'static str, &'static str, Box<dyn FnOnce()>);

#[test]
fn server_failures_log_code_and_reason_above_debug_and_text_only_at_debug() {
    let cases: Vec<Case> = vec![
        (
            "MARKER_DB",
            r#"code="internal_error" reason="protocol""#,
            Box::new(|| {
                let wire = from_core(&Error::Db(sqlx::Error::Protocol("MARKER_DB".into())));
                assert!(!wire.message.contains("MARKER"), "{}", wire.message);
            }),
        ),
        (
            "MARKER_CORE_PLATFORM",
            r#"code="platform_unavailable" reason="http_status" status=503"#,
            Box::new(|| {
                from_core(&Error::Platform(otto_resource::Error::Status {
                    status: 503,
                    body: "MARKER_CORE_PLATFORM".into(),
                }));
            }),
        ),
        (
            "MARKER_PLATFORM",
            r#"code="platform_unavailable" reason="decode""#,
            Box::new(|| {
                let wire = from_platform(&otto_resource::Error::Decode("MARKER_PLATFORM".into()));
                assert!(!wire.message.contains("MARKER"), "{}", wire.message);
                assert_eq!(
                    wire.data.expect("data")["code"],
                    "platform_unavailable",
                    "the wire code is unchanged"
                );
            }),
        ),
        (
            // The wire flattens this to an opaque internal error; the log
            // keeps the reason.
            "pool timed out",
            r#"code="internal_error" reason="pool_timed_out""#,
            Box::new(|| {
                let wire = from_tenant(otto_tenant::Error::Db(sqlx::Error::PoolTimedOut));
                let data = wire.data.expect("data");
                assert_eq!(data["code"], "internal_error");
                assert_eq!(data["retriable"], false);
            }),
        ),
    ];
    for (marker, fields, call) in cases {
        let (errors, debugs) = capture(call);
        assert!(
            errors.iter().any(|l| l.contains(fields)),
            "{fields} at ERROR: {errors:#?}"
        );
        assert!(
            errors.iter().all(|l| !l.contains(marker)),
            "{marker} must not be at ERROR: {errors:#?}"
        );
        assert!(
            debugs.iter().any(|l| l.contains(marker)),
            "{marker} at DEBUG: {debugs:#?}"
        );
    }
}
