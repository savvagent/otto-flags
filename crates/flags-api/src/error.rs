//! The SDK surface's error body: `{"error": code, "error_description": text}`.
//! Status codes follow `docs/SDK-DEVELOPER-GUIDE.md`: 401 for a bad key, 404
//! for an unknown flag (SDKs then serve their default), 5xx for ours.

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
            e if e.is_internal() => {
                tracing::error!(error = %e, "SDK request failed");
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

impl From<otto_tenant::Error> for ApiError {
    fn from(e: otto_tenant::Error) -> Self {
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
