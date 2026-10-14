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

/// Why a webhook was rejected, without the error text.
fn rejection_reason(e: &WebhookError) -> &'static str {
    match e {
        WebhookError::MalformedHeader => "malformed_header",
        WebhookError::BadSignature => "bad_signature",
        WebhookError::Stale => "stale",
        WebhookError::BadBody(_) => "bad_body",
    }
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
        Err(e @ WebhookError::BadBody(_)) => {
            // The error text is serde's and can quote the body.
            let reason = rejection_reason(&e);
            tracing::error!(reason, "platform webhook was signed but unreadable");
            tracing::debug!(reason, error = %e, "platform webhook was signed but unreadable");
            return Err(ApiError::bad_request(
                "the webhook body is not a valid event",
            ));
        }
        Err(e) => {
            let reason = rejection_reason(&e);
            tracing::warn!(reason, "platform webhook rejected");
            tracing::debug!(reason, error = %e, "platform webhook rejected");
            return Err(rejected());
        }
    };
    let outcome = platform_events::apply(&state.db, &event)
        .await
        .map_err(|e| {
            let f = e.failure_reason();
            tracing::error!(
                code = e.code(),
                reason = f.reason,
                status = f.status,
                sqlstate = f.sqlstate(),
                transport = f.transport,
                event_id = %event.id,
                "could not apply a platform event"
            );
            tracing::debug!(
                code = e.code(),
                reason = f.reason,
                status = f.status,
                sqlstate = f.sqlstate(),
                transport = f.transport,
                event_id = %event.id,
                error = %e,
                "could not apply a platform event"
            );
            ApiError::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "temporarily_unavailable",
                "could not apply the event; redeliver it",
            )
        })?;
    Ok(Json(outcome))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejection_reasons_name_each_variant() {
        assert_eq!(
            rejection_reason(&WebhookError::MalformedHeader),
            "malformed_header"
        );
        assert_eq!(
            rejection_reason(&WebhookError::BadSignature),
            "bad_signature"
        );
        assert_eq!(rejection_reason(&WebhookError::Stale), "stale");
        assert_eq!(
            rejection_reason(&WebhookError::BadBody("secret".into())),
            "bad_body"
        );
    }
}
