//! The auto-rollback checker logs a failed check and a failed backstop scan
//! with the text-free fields of `Error::failure_reason` above `debug`, and the
//! same fields plus the error text only at `debug` (#54).
//!
//! The third site, a policy `check_flag` could not act on, logs the same
//! fields but is not reached here: it needs a policy to fail mid-check against
//! a working database. `log_text.rs` scans it with the rest.
//!
//! Its own test binary, with one test: `tracing` caches per callsite whether
//! any subscriber is interested, so another test in the same binary hitting
//! the same callsite with no subscriber can make this one miss its events.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use flags_core::entitlements::Entitlement;
use flags_core::ids::FlagId;
use flags_core::rollback::{Checker, CheckerConfig};
use savvagent_tenant::ids::OrgId;
use sqlx::postgres::PgPoolOptions;

#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("capture lock").clone()).expect("utf-8")
    }
}

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

/// The captured lines at `level` whose message is `message`.
fn lines<'a>(logs: &'a str, level: &str, message: &str) -> Vec<&'a str> {
    logs.lines()
        .filter(|l| l.contains(&format!(" {level} ")) && l.contains(message))
        .collect()
}

// A current-thread runtime, so the checker task runs on this thread and sees
// the thread-local default subscriber.
#[tokio::test]
async fn checker_failures_log_reasons_above_debug_and_text_only_at_debug() {
    // A closed pool fails every acquire with `PoolClosed`, without a server.
    let pool = PgPoolOptions::new()
        .connect_lazy("postgres://flags:flags@127.0.0.1:1/none")
        .expect("lazy pool");
    pool.close().await;
    let db = savvagent_tenant::Db::from_pool(pool);

    let capture = Capture::default();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let (stop, shutdown) = tokio::sync::watch::channel(false);
    let (hints, task) =
        Checker::spawn(db, Entitlement::Granted, CheckerConfig::default(), shutdown);
    // The backstop's first tick is immediate; the hint makes one check due.
    hints.send(OrgId::new(), vec![FlagId::new()]);

    const BACKSTOP: &str = "auto-rollback backstop could not list due policies";
    const BACKSTOP_DEBUG: &str = "auto-rollback backstop failure";
    const CHECK: &str = "auto-rollback check failed";
    const CHECK_DEBUG: &str = "auto-rollback check failure";
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let logs = capture.text();
        if [BACKSTOP, BACKSTOP_DEBUG, CHECK, CHECK_DEBUG]
            .iter()
            .all(|m| logs.contains(m))
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the checker did not log both failures:\n{logs}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    stop.send(true).expect("checker running");
    task.await.expect("checker task");

    let logs = capture.text();
    // What the error itself says: only the debug twins may carry it.
    let text = sqlx::Error::PoolClosed.to_string();
    for (message, debug_message) in [(BACKSTOP, BACKSTOP_DEBUG), (CHECK, CHECK_DEBUG)] {
        let warn = lines(&logs, "WARN", message);
        assert_eq!(warn.len(), 1, "{message}:\n{logs}");
        let warn = warn[0];
        assert!(warn.contains("code=\"internal_error\""), "{warn}");
        assert!(warn.contains("reason=\"pool_closed\""), "{warn}");
        // A closed pool has no status, SQLSTATE, or transport: absent, not empty.
        for absent in ["status=", "sqlstate=", "transport="] {
            assert!(!warn.contains(absent), "{warn}");
        }
        assert!(!warn.contains("error="), "{warn}");
        assert!(!warn.contains(&text), "{warn}");

        let debug = lines(&logs, "DEBUG", debug_message);
        assert_eq!(debug.len(), 1, "{debug_message}:\n{logs}");
        let debug = debug[0];
        assert!(debug.contains("code=\"internal_error\""), "{debug}");
        assert!(debug.contains("reason=\"pool_closed\""), "{debug}");
        assert!(debug.contains("error="), "{debug}");
        assert!(debug.contains(&text), "{debug}");
    }
    // A closed pool is transient: the failed check is retried.
    let check = lines(&logs, "WARN", CHECK)[0];
    assert!(check.contains("retry=true"), "{check}");
}
