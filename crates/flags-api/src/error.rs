//! The SDK surface's error body: `{"error": code, "error_description": text}`.
//! Status codes follow `docs/SDK-DEVELOPER-GUIDE.md`: 401 for a bad key, 404
//! for an unknown flag (SDKs then serve their default), 5xx for ours. Every
//! 503 carries `Retry-After: 5`: a database failure (`temporarily_unavailable`)
//! and a key lookup refused because every lookup permit was taken (`busy`,
//! #120). A refused lookup is logged at `warn` at most once a second, so a
//! flood cannot flood the log, and per request only at `debug`.

use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::response::{IntoResponse, Response};
use axum::Json;
use http::StatusCode;

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub code: &'static str,
    pub message: String,
}

impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    pub fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "invalid_key",
            "send a valid SDK key (sdk_… or srv_…) as `Authorization: Bearer <key>` or `X-SDK-Key`",
        )
    }

    /// A client key used from a browser origin the app does not allow (#143).
    /// The body names neither the origin list nor the app.
    pub fn origin_not_allowed() -> Self {
        Self::new(
            StatusCode::FORBIDDEN,
            "origin_not_allowed",
            "this client key is not allowed from this origin; ask the app's owner or admin to \
             add it to the app's allowed origins",
        )
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }

    pub fn flag_not_found(key: &str) -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "flag_not_found",
            format!("no active flag {key:?} in this app"),
        )
    }
}

impl From<flags_core::Error> for ApiError {
    fn from(e: flags_core::Error) -> Self {
        match e {
            flags_core::Error::AccessRevoked => Self::unauthorized(),
            flags_core::Error::Invalid(m) => Self::bad_request(m),
            flags_core::Error::Busy => {
                if busy_warn_due(Instant::now()) {
                    tracing::warn!(
                        code = "busy",
                        "SDK key lookups refused: every lookup permit is taken \
                         (logged at most once a second)"
                    );
                }
                tracing::debug!(code = "busy", "SDK key lookup refused");
                Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "busy",
                    "the flag service is busy; serve cached or default values and retry",
                )
            }
            e if e.is_internal() => {
                let f = e.failure_reason();
                tracing::error!(
                    code = e.code(),
                    reason = f.reason,
                    status = f.status,
                    sqlstate = f.sqlstate(),
                    transport = f.transport,
                    "SDK request failed"
                );
                tracing::debug!(code = e.code(), reason = f.reason, status = f.status, sqlstate = f.sqlstate(), transport = f.transport, error = %e, "SDK request failed");
                Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "temporarily_unavailable",
                    "the flag service could not answer; serve cached or default values and retry",
                )
            }
            e => Self::new(StatusCode::BAD_REQUEST, e.code(), e.to_string()),
        }
    }
}

/// How often a refused key lookup may be logged at `warn`.
const BUSY_WARN_EVERY: Duration = Duration::from_secs(1);

/// When a refused key lookup was last logged at `warn`, process-wide.
static BUSY_WARNED: Mutex<Option<Instant>> = Mutex::new(None);

/// Whether a refused key lookup at `now` is due a `warn`.
fn busy_warn_due(now: Instant) -> bool {
    warn_due(&BUSY_WARNED, now)
}

/// Whether a warning at `now` is due, given when one was `last` logged: none
/// yet, or none in the last [`BUSY_WARN_EVERY`]. Records `now` when it is.
fn warn_due(last: &Mutex<Option<Instant>>, now: Instant) -> bool {
    // Every state of the `Option` is valid, so a poisoned lock is recovered.
    let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
    let due = last.is_none_or(|at| now.saturating_duration_since(at) >= BUSY_WARN_EVERY);
    if due {
        *last = Some(now);
    }
    due
}

impl From<savvagent_tenant::Error> for ApiError {
    fn from(e: savvagent_tenant::Error) -> Self {
        flags_core::Error::from(e).into()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut res = (
            self.status,
            Json(serde_json::json!({ "error": self.code, "error_description": self.message })),
        )
            .into_response();
        if self.status == StatusCode::SERVICE_UNAVAILABLE {
            res.headers_mut().insert(
                http::header::RETRY_AFTER,
                http::HeaderValue::from_static("5"),
            );
        }
        res
    }
}

pub type ApiResult<T> = Result<T, ApiError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_refused_lookup_is_a_retriable_503_busy() {
        let e = ApiError::from(flags_core::Error::Busy);
        assert_eq!(
            (e.status, e.code),
            (StatusCode::SERVICE_UNAVAILABLE, "busy")
        );
        let res = e.into_response();
        assert_eq!(res.headers()[http::header::RETRY_AFTER], "5");
    }

    #[test]
    fn a_refused_lookup_warns_at_most_once_a_second() {
        // A local clock: the process-wide one is shared with the other tests.
        let last = Mutex::new(None);
        let t0 = Instant::now();
        assert!(warn_due(&last, t0));
        assert!(!warn_due(&last, t0 + Duration::from_millis(999)));
        assert!(warn_due(&last, t0 + BUSY_WARN_EVERY));
        assert!(!warn_due(&last, t0 + BUSY_WARN_EVERY));
    }
}
