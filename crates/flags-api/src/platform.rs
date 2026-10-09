//! The otto platform's lifecycle webhooks.
//!
//! **The status code is the retry protocol.** The platform redelivers anything
//! that is not a 2xx. A bad signature is `401` and never processed (the body is
//! not parsed before the MAC checks out); a handled, repeated, or unknown event
//! is `200`; a failure to apply is `5xx`, so the platform tries again.

use axum::body::Bytes;
use axum::extract::State;
use axum::Json;
use flags_core::platform_events::{self, Outcome};
use http::{HeaderMap, StatusCode};
use otto_resource::webhook::{self, WebhookError};

use crate::error::{ApiError, ApiResult};
use crate::AppState;

fn rejected() -> ApiError {
    ApiError::new(
        StatusCode::UNAUTHORIZED,
        "invalid_signature",
        "the Otto-Signature header is missing, malformed, stale, or does not match the body",
    )
}

pub async fn receive(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<Json<Outcome>> {
    let signature = headers
        .get(webhook::SIGNATURE_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or_else(rejected)?;
    let event = match webhook::verify(&state.platform_webhook_secret, signature, &body) {
        Ok(event) => event,
        Err(WebhookError::BadBody(reason)) => {
            tracing::error!(%reason, "platform webhook was signed but unreadable");
            return Err(ApiError::bad_request(
                "the webhook body is not a valid event",
            ));
        }
        Err(e) => {
            tracing::warn!(error = %e, "platform webhook rejected");
            return Err(rejected());
        }
    };
    let outcome = platform_events::apply(&state.db, &event)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, event_id = %event.id, "could not apply a platform event");
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "temporarily_unavailable",
                "could not apply the event; redeliver it",
            )
        })?;
    Ok(Json(outcome))
}
