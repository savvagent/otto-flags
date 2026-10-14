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

/// A database error with a chosen SQLSTATE and a marker text: sqlx's own
/// Postgres error cannot be built outside sqlx. sqlx formats `Error::Database`
/// through the boxed error's `Display`, so the marker reaches the debug line.
#[derive(Debug)]
struct FakeDbError(&'static str, &'static str);

impl std::fmt::Display for FakeDbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.1)
    }
}

impl std::error::Error for FakeDbError {}

impl sqlx::error::DatabaseError for FakeDbError {
    fn message(&self) -> &str {
        self.1
    }
    fn code(&self) -> Option<std::borrow::Cow<'_, str>> {
        Some(std::borrow::Cow::Borrowed(self.0))
    }
    fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
        self
    }
    fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
        self
    }
    fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
        self
    }
    fn kind(&self) -> sqlx::error::ErrorKind {
        sqlx::error::ErrorKind::Other
    }
}

/// The text that must stay at debug, the fields expected at error, and the call.
type Case = (&'static str, &'static str, Box<dyn FnOnce()>);

#[test]
fn server_failures_log_code_and_reason_above_debug_and_text_only_at_debug() {
    // Produced here, outside `capture`, so the connect attempt's own events
    // are not captured: only `from_platform` runs inside it.
    let connect_failure = {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("addr")
            .port();
        let cfg = otto_resource::ClientConfig::new(
            format!("http://127.0.0.1:{port}"),
            "https://flags.test/mcp",
            "secret",
        );
        let client = otto_resource::PlatformClient::new(cfg).expect("client");
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime")
            .block_on(client.usage_status(uuid::Uuid::nil()))
            .expect_err("nothing listens on the port")
    };
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
            "MARKER_SQLSTATE",
            r#"code="internal_error" reason="admin_shutdown" sqlstate="57P01""#,
            Box::new(|| {
                let wire = from_core(&Error::Db(sqlx::Error::Database(Box::new(FakeDbError(
                    "57P01",
                    "MARKER_SQLSTATE",
                )))));
                assert!(!wire.message.contains("MARKER"), "{}", wire.message);
            }),
        ),
        (
            "could not reach the platform",
            r#"code="platform_unavailable" reason="transport" transport="connect""#,
            Box::new(move || {
                from_platform(&connect_failure);
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
            "MARKER_TENANT",
            r#"code="internal_error" reason="protocol""#,
            Box::new(|| {
                let wire = from_tenant(otto_tenant::Error::Db(sqlx::Error::Protocol(
                    "MARKER_TENANT".into(),
                )));
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
