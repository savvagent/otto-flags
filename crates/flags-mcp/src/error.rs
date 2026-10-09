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
//!   wire: it can carry schema details and is no use to an agent anyway.

use flags_core::Error;
use rmcp::model::{ErrorCode, ErrorData};

pub fn from_core(e: &Error) -> ErrorData {
    let (rpc, message) = if e.is_internal() {
        tracing::error!(error = %e, "server-side failure surfaced to an MCP caller");
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
            Error::QuotaExceeded { .. } | Error::AccessRevoked | Error::VersionConflict { .. } => {
                ErrorCode::INVALID_REQUEST
            }
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
    tracing::error!(error = %e, "platform lookup failed for an MCP caller");
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

    fn sqlx_error() -> sqlx::Error {
        sqlx::Error::PoolTimedOut
    }
}
