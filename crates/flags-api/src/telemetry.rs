//! SDK telemetry: evaluation counts and errors behind flags.

use axum::extract::State;
use axum::Json;
use chrono::{DateTime, TimeZone, Utc};
use flags_core::platform_events::begin_live;
use flags_core::telemetry::{self, ErrorEvent, EvalEvent, Receipt, MAX_BATCH};
use serde::Deserialize;
use serde_json::Value;

use crate::error::{ApiError, ApiResult};
use crate::sdk::Sdk;
use crate::AppState;

#[derive(Debug, Deserialize)]
pub struct EvaluationsBody {
    evaluations: Vec<EvaluationIn>,
}

#[derive(Debug, Deserialize)]
struct EvaluationIn {
    flag_key: String,
    result: bool,
    #[serde(default)]
    variation: Option<String>,
    #[serde(default)]
    environment: Option<String>,
    #[serde(default)]
    context: Option<Value>,
    #[serde(default)]
    timestamp: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct ErrorsBody {
    errors: Vec<ErrorIn>,
}

#[derive(Debug, Deserialize)]
struct ErrorIn {
    flag_key: String,
    flag_enabled: bool,
    error_type: String,
    error_message: String,
    #[serde(default)]
    stack_trace: Option<String>,
    #[serde(default)]
    environment: Option<String>,
    #[serde(default)]
    context: Option<Value>,
    #[serde(default)]
    timestamp: Option<Value>,
}

/// Seconds, milliseconds, or RFC 3339: the SDKs have sent all three.
fn when(v: Option<&Value>) -> DateTime<Utc> {
    let now = Utc::now();
    match v {
        Some(Value::Number(n)) => n
            .as_i64()
            .and_then(|n| {
                if n > 100_000_000_000 {
                    Utc.timestamp_millis_opt(n).single()
                } else {
                    Utc.timestamp_opt(n, 0).single()
                }
            })
            .unwrap_or(now),
        Some(Value::String(s)) => DateTime::parse_from_rfc3339(s)
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or(now),
        _ => now,
    }
}

fn environment(explicit: Option<String>, context: Option<&Value>) -> Option<String> {
    explicit.filter(|e| !e.is_empty()).or_else(|| {
        context
            .and_then(|c| c.get("environment"))
            .and_then(Value::as_str)
            .filter(|e| !e.is_empty())
            .map(str::to_string)
    })
}

fn check_batch(n: usize) -> ApiResult<()> {
    if n > MAX_BATCH {
        return Err(ApiError::bad_request(format!(
            "send at most {MAX_BATCH} events per request (got {n})"
        )));
    }
    Ok(())
}

pub async fn evaluations(
    State(state): State<AppState>,
    sdk: Sdk,
    Json(body): Json<EvaluationsBody>,
) -> ApiResult<Json<Receipt>> {
    check_batch(body.evaluations.len())?;
    let events = body
        .evaluations
        .into_iter()
        .map(|e| EvalEvent {
            environment: environment(e.environment, e.context.as_ref())
                .unwrap_or_else(|| "production".into()),
            flag_key: e.flag_key,
            enabled: e.result,
            variation: e.variation,
            at: when(e.timestamp.as_ref()),
        })
        .collect();
    let mut tx = begin_live(&state.db, sdk.0.org_id, None).await?;
    let receipt = telemetry::record_evaluations(&mut tx, sdk.0.app_id, events).await?;
    tx.commit().await?;
    Ok(Json(receipt))
}

pub async fn errors(
    State(state): State<AppState>,
    sdk: Sdk,
    Json(body): Json<ErrorsBody>,
) -> ApiResult<Json<Receipt>> {
    check_batch(body.errors.len())?;
    let events = body
        .errors
        .into_iter()
        .map(|e| ErrorEvent {
            environment: environment(e.environment, e.context.as_ref()),
            flag_key: e.flag_key,
            flag_enabled: e.flag_enabled,
            error_type: e.error_type,
            error_message: e.error_message,
            stack_trace: e.stack_trace,
            at: when(e.timestamp.as_ref()),
        })
        .collect();
    let mut tx = begin_live(&state.db, sdk.0.org_id, None).await?;
    let receipt = telemetry::record_errors(&mut tx, sdk.0.app_id, events).await?;
    tx.commit().await?;
    Ok(Json(receipt))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn timestamps_in_every_spelling_the_sdks_send() {
        let secs = when(Some(&json!(1_700_000_000)));
        let millis = when(Some(&json!(1_700_000_000_000i64)));
        let iso = when(Some(&json!("2023-11-14T22:13:20Z")));
        assert_eq!(secs, millis);
        assert_eq!(secs, iso);
        assert!(Utc::now() - when(Some(&json!("garbage"))) < chrono::TimeDelta::seconds(5));
    }

    #[test]
    fn environment_falls_back_to_the_context() {
        let ctx = json!({"environment": "staging"});
        assert_eq!(environment(None, Some(&ctx)).as_deref(), Some("staging"));
        assert_eq!(
            environment(Some("prod".into()), Some(&ctx)).as_deref(),
            Some("prod")
        );
        assert_eq!(environment(Some(String::new()), None), None);
    }
}
