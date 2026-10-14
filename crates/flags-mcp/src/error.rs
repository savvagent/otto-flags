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
//!   wire: it can carry schema details and is no use to an agent anyway. Logs
//!   follow the same rule: the failure's code and reason at error, its text
//!   only at debug.

use flags_core::Error;
use rmcp::model::{ErrorCode, ErrorData};

/// Logs a server-side failure: the wire code and a text-free reason (see
/// [`Error::failure_reason`]) at error, the text only at debug. Server-side
/// text can carry SQL, schema names, response bodies, and URLs, which must not
/// reach the default log level. At debug both events appear, on purpose.
pub(crate) fn log_server_failure(
    what: &'static str,
    code: &'static str,
    (reason, status): (&'static str, Option<u16>),
    detail: &dyn std::fmt::Display,
) {
    tracing::error!(code, reason, status, "{what}");
    tracing::debug!(code, reason, error = %detail, "{what}");
}

const SURFACED: &str = "server-side failure surfaced to an MCP caller";

pub fn from_core(e: &Error) -> ErrorData {
    if e.is_internal() {
        log_server_failure(SURFACED, e.code(), e.failure_reason(), e);
    }
    respond(e)
}

fn respond(e: &Error) -> ErrorData {
    let (rpc, message) = if e.is_internal() {
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

/// On the wire, any tenant failure but a bad argument is an opaque, non-retriable
/// internal error. The log keeps the original, so a pool timeout is still
/// logged as one.
pub fn from_tenant(e: otto_tenant::Error) -> ErrorData {
    let original = Error::Tenant(e);
    let wire = Error::Tenant(match &original {
        Error::Tenant(otto_tenant::Error::Invalid(m)) => otto_tenant::Error::Invalid(m.clone()),
        other => otto_tenant::Error::Config(other.to_string()),
    });
    if wire.is_internal() {
        log_server_failure(SURFACED, wire.code(), original.failure_reason(), &original);
    }
    respond(&wire)
}

pub fn from_platform(e: &otto_resource::Error) -> ErrorData {
    log_server_failure(
        "platform lookup failed for an MCP caller",
        "platform_unavailable",
        flags_core::error::platform_failure_reason(e),
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

    #[test]
    fn tenant_failures_keep_their_wire_answer() {
        let invalid = from_tenant(otto_tenant::Error::Invalid("bad org".into()));
        assert_eq!(invalid.message, "bad org");
        assert_eq!(invalid.data.expect("data")["code"], "invalid_argument");
        for e in [
            otto_tenant::Error::OrgNotFound(uuid::Uuid::nil().into()),
            otto_tenant::Error::Db(sqlx_error()),
        ] {
            let wire = from_tenant(e);
            assert_eq!(wire.code, ErrorCode::INTERNAL_ERROR);
            let data = wire.data.expect("data");
            assert_eq!(data["code"], "internal_error");
            assert_eq!(data["retriable"], false);
        }
    }

    fn sqlx_error() -> sqlx::Error {
        sqlx::Error::PoolTimedOut
    }
}
