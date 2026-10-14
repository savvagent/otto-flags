//! Turning domain failures into something an agent can act on.
//!
//! The reader of every message is an LLM deciding, within one turn, whether to
//! retry, change its arguments, or stop and ask a person. So:
//!
//! - `flags-core` writes messages that say what was wrong and what to call
//!   next; this module carries them through intact.
//! - The branch point is machine-readable: `data.code` is stable and
//!   `data.retriable` answers the only question that has to be resolved before
//!   the next action.
//! - Server-side failures (database, platform) never put their own text on the
//!   wire: it can carry schema details and is no use to an agent anyway. Their
//!   text is logged only at debug; the code is logged at error.

use flags_core::Error;
use rmcp::model::{ErrorCode, ErrorData};

/// The log code for a platform failure: finer than the wire's
/// `platform_unavailable`, so a rejected credential is told apart from an outage.
pub(crate) fn platform_code(e: &otto_resource::Error) -> &'static str {
    use otto_resource::Error as P;
    match e {
        P::Unauthorized => "platform_unauthorized",
        P::NotFound => "platform_not_found",
        P::Status { .. } => "platform_http_status",
        P::Transport(_) => "platform_transport",
        P::Decode(_) => "platform_decode",
        P::InvalidUrl(_) => "platform_invalid_url",
    }
}

/// The log code for a domain failure: [`platform_code`] for a platform
/// failure, the wire code otherwise.
pub(crate) fn log_code(e: &Error) -> &'static str {
    match e {
        Error::Platform(p) => platform_code(p),
        _ => e.code(),
    }
}

/// Logs a server-side failure: the code at error, the text only at debug.
/// Database and platform text can carry SQL, schema names, response bodies, and
/// URLs, which must not reach the default log level; the code is ours.
pub(crate) fn log_server_failure(
    what: &'static str,
    code: &'static str,
    detail: &dyn std::fmt::Display,
) {
    tracing::error!(code, "{what}");
    tracing::debug!(code, error = %detail, "{what}");
}

pub fn from_core(e: &Error) -> ErrorData {
    let (rpc, message) = if e.is_internal() {
        log_server_failure(
            "server-side failure surfaced to an MCP caller",
            log_code(e),
            e,
        );
        let message = match e {
            Error::Platform(_) => {
                "the otto platform could not be reached to check this; nothing was changed. \
                 Retry shortly."
            }
            _ => "the server could not complete this call; nothing was changed. Retry shortly.",
        };
        (ErrorCode::INTERNAL_ERROR, message.to_string())
    } else {
        let rpc = match e {
            // Well-formed calls the server declined: not argument errors.
            Error::QuotaExceeded { .. }
            | Error::FeatureNotInPlan { .. }
            | Error::AccessRevoked
            | Error::VersionConflict { .. } => ErrorCode::INVALID_REQUEST,
            // Ours to say, but the platform could not be asked: not the caller's fault.
            Error::EntitlementUnavailable { .. } => ErrorCode::INTERNAL_ERROR,
            _ => ErrorCode::INVALID_PARAMS,
        };
        (rpc, e.to_string())
    };
    ErrorData::new(
        rpc,
        message,
        Some(serde_json::json!({ "code": e.code(), "retriable": e.retriable() })),
    )
}

pub fn from_tenant(e: &otto_tenant::Error) -> ErrorData {
    let e = Error::Tenant(match e {
        otto_tenant::Error::Invalid(m) => otto_tenant::Error::Invalid(m.clone()),
        other => otto_tenant::Error::Config(other.to_string()),
    });
    from_core(&e)
}

pub fn from_platform(e: &otto_resource::Error) -> ErrorData {
    log_server_failure(
        "platform lookup failed for an MCP caller",
        platform_code(e),
        e,
    );
    ErrorData::new(
        ErrorCode::INTERNAL_ERROR,
        "the otto platform could not be reached to check this; nothing was changed. Retry shortly.",
        Some(serde_json::json!({ "code": "platform_unavailable", "retriable": e.is_retriable() })),
    )
}

/// A token without a scope the tool needs. Actionable: the agent must
/// re-authorize asking for it, so the message names it.
pub fn from_scope(e: &crate::auth::MissingScope) -> ErrorData {
    ErrorData::new(
        ErrorCode::INVALID_REQUEST,
        format!(
            "{e}. Re-authorize this server requesting it (the scopes this server knows are listed \
             at /.well-known/oauth-protected-resource)"
        ),
        Some(serde_json::json!({ "code": "insufficient_scope", "retriable": false })),
    )
}

pub fn forbidden(message: impl Into<String>) -> ErrorData {
    ErrorData::new(
        ErrorCode::INVALID_REQUEST,
        message.into(),
        Some(serde_json::json!({ "code": "forbidden", "retriable": false })),
    )
}

/// Reached only if the MCP surface was mounted without its middleware.
pub fn unauthenticated() -> ErrorData {
    ErrorData::internal_error(
        "this request carried no authenticated principal, which means the MCP surface was \
         mounted without its resource-server middleware; this is a server misconfiguration, \
         not a problem with your call",
        Some(serde_json::json!({ "code": "unauthenticated", "retriable": false })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_error_carries_a_code_and_a_retriable_flag() {
        for e in [
            Error::Invalid("nope".into()),
            Error::FlagNotFound {
                app: "web".into(),
                key: "f".into(),
            },
            Error::VersionConflict {
                key: "f".into(),
                expected: 1,
                actual: 2,
            },
            Error::Db(sqlx_error()),
            Error::FeatureNotInPlan {
                feature: "auto_rollback".into(),
                plan: "Free".into(),
                upgrade_url: "https://example.test/upgrade".into(),
            },
            Error::EntitlementUnavailable {
                feature: "auto_rollback".into(),
            },
        ] {
            let data = from_core(&e).data.expect("data");
            assert_eq!(data["code"], e.code());
            assert_eq!(data["retriable"], e.retriable());
        }
    }

    #[test]
    fn actionable_detail_survives_and_database_text_does_not() {
        let e = Error::AppNotFound {
            name: "wbe".into(),
            hint: "its apps are: web, api".into(),
        };
        assert!(from_core(&e).message.contains("web, api"));
        let db = from_core(&Error::Db(sqlx_error()));
        assert!(!db.message.contains("pool"), "{}", db.message);
    }

    #[test]
    fn plan_gate_errors_tell_the_agent_what_to_do() {
        let denied = from_core(&Error::FeatureNotInPlan {
            feature: "auto_rollback".into(),
            plan: "Free".into(),
            upgrade_url: "https://example.test/upgrade".into(),
        });
        assert!(denied.message.contains("https://example.test/upgrade"));
        assert!(denied.message.contains("Free"));
        assert_eq!(denied.code, ErrorCode::INVALID_REQUEST);
        let unknown = Error::EntitlementUnavailable {
            feature: "auto_rollback".into(),
        };
        let data = from_core(&unknown);
        assert_eq!(data.code, ErrorCode::INTERNAL_ERROR);
        assert_eq!(data.data.expect("data")["retriable"], true);
    }

    #[derive(Clone, Default)]
    struct Capture(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

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

    /// Runs `f` under a thread-local subscriber (the code under test is
    /// synchronous, and a global one would leak between parallel tests) and
    /// returns what it logged.
    fn capture(f: impl FnOnce()) -> String {
        let out = Capture::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .with_writer(out.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, f);
        let bytes = out.0.lock().expect("capture lock").clone();
        String::from_utf8(bytes).expect("utf-8 log output")
    }

    #[test]
    fn server_failures_log_the_code_above_debug_and_the_text_only_at_debug() {
        let mut wire = Vec::new();
        let logs = capture(|| {
            wire.push(from_core(&Error::Db(sqlx::Error::Protocol(
                "MARKER_DB relation secret".into(),
            ))));
            wire.push(from_platform(&otto_resource::Error::Status {
                status: 503,
                body: "MARKER_PLATFORM".into(),
            }));
        });
        let at =
            |level: &str| -> Vec<&str> { logs.lines().filter(|l| l.contains(level)).collect() };
        let errors = at(" ERROR ");
        let debugs = at(" DEBUG ");
        assert!(
            errors.iter().any(|l| l.contains("code=\"internal_error\"")),
            "{logs}"
        );
        assert!(
            errors
                .iter()
                .any(|l| l.contains("code=\"platform_http_status\"")),
            "{logs}"
        );
        assert!(errors.iter().all(|l| !l.contains("MARKER_")), "{logs}");
        assert!(debugs.iter().any(|l| l.contains("MARKER_DB")), "{logs}");
        assert!(
            debugs.iter().any(|l| l.contains("MARKER_PLATFORM")),
            "{logs}"
        );

        // The wire is unchanged: no text, same codes.
        assert!(!wire[0].message.contains("MARKER_"), "{}", wire[0].message);
        assert_eq!(
            wire[1].data.as_ref().expect("data")["code"],
            "platform_unavailable"
        );
    }

    #[test]
    fn platform_codes_name_the_failure_kind() {
        use otto_resource::Error as P;
        assert_eq!(platform_code(&P::Unauthorized), "platform_unauthorized");
        assert_eq!(platform_code(&P::NotFound), "platform_not_found");
        assert_eq!(
            platform_code(&P::Status {
                status: 500,
                body: String::new()
            }),
            "platform_http_status"
        );
        assert_eq!(platform_code(&P::Decode("x".into())), "platform_decode");
        assert_eq!(
            platform_code(&P::InvalidUrl("x".into())),
            "platform_invalid_url"
        );
    }

    fn sqlx_error() -> sqlx::Error {
        sqlx::Error::PoolTimedOut
    }
}
