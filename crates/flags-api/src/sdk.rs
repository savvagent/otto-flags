//! Evaluation and flag listing for SDKs.

use axum::extract::{FromRequestParts, Path, Query, State};
use axum::Json;
use flags_core::apps::AppsExt;
use flags_core::eval::{self, Context};
use flags_core::flags::{FlagStatus, FlagsExt, SdkFlag};
use flags_core::keys::{self, KeyKind, KeyOwner};
use flags_core::platform_events::begin_live;
use http::request::Parts;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{ApiError, ApiResult};
use crate::AppState;

/// The app an SDK key opens. Extracting it is the authentication.
#[derive(Debug, Clone)]
pub struct Sdk(pub KeyOwner, pub Vec<u8>);

impl Sdk {
    /// Server keys see targeting rules; public client keys do not.
    pub fn sees_rules(&self) -> bool {
        self.0.kind == KeyKind::Server
    }
}

fn presented_key(parts: &Parts) -> Option<&str> {
    if let Some(v) = parts.headers.get("x-sdk-key").and_then(|v| v.to_str().ok()) {
        return Some(v.trim());
    }
    let raw = parts
        .headers
        .get(http::header::AUTHORIZATION)?
        .to_str()
        .ok()?;
    let (scheme, token) = raw.split_once(' ')?;
    scheme.eq_ignore_ascii_case("bearer").then(|| token.trim())
}

impl FromRequestParts<AppState> for Sdk {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let key = presented_key(parts).ok_or_else(ApiError::unauthorized)?;
        match keys::resolve(&state.db, key).await? {
            Some(owner) => Ok(Sdk(owner, keys::hash(key))),
            None => Err(ApiError::unauthorized()),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct EvaluateBody {
    #[serde(default)]
    pub context: Option<Context>,
}

#[derive(Debug, Serialize)]
pub struct EvaluateResponse {
    pub key: String,
    pub enabled: bool,
    pub scope: &'static str,
    pub variation: Option<String>,
    pub configuration: Option<Value>,
    pub timestamp: i64,
    pub version: i32,
    pub context: Context,
}

/// The environment to evaluate in when the context names none. The SDKs
/// default to production too.
const DEFAULT_ENVIRONMENT: &str = "production";

pub async fn evaluate(
    State(state): State<AppState>,
    sdk: Sdk,
    Path(key): Path<String>,
    body: axum::body::Bytes,
) -> ApiResult<Json<EvaluateResponse>> {
    // An empty body is an evaluation with no context, not a malformed request:
    // some SDKs send nothing for a flag that needs no targeting.
    let context = if body.iter().all(u8::is_ascii_whitespace) {
        Context::default()
    } else {
        serde_json::from_slice::<EvaluateBody>(&body)
            .map_err(|e| {
                ApiError::bad_request(format!("the body is not an evaluation request: {e}"))
            })?
            .context
            .unwrap_or_default()
    };
    let environment = context
        .environment
        .clone()
        .filter(|e| !e.is_empty())
        .unwrap_or_else(|| DEFAULT_ENVIRONMENT.into());

    let mut tx = begin_live(&state.db, sdk.0.org_id, None).await?;
    let flag = tx.find_flag(sdk.0.app_id, &key).await?;
    tx.commit().await?;
    let flag = flag
        .filter(|f| f.status == FlagStatus::Active)
        .ok_or_else(|| ApiError::flag_not_found(&key))?;

    let e = eval::evaluate(flag.view(), &environment, &context);
    Ok(Json(EvaluateResponse {
        key: flag.key,
        enabled: e.enabled,
        scope: "application",
        variation: e.variation,
        configuration: e.configuration,
        timestamp: chrono::Utc::now().timestamp(),
        version: flag.version,
        context,
    }))
}

#[derive(Debug, Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    environment: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct FlagList {
    pub flags: Vec<SdkFlag>,
    pub count: usize,
    pub organization_id: String,
    pub application_id: Option<String>,
}

pub async fn list_flags(
    State(state): State<AppState>,
    sdk: Sdk,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<FlagList>> {
    let environment = q.environment.unwrap_or_else(|| DEFAULT_ENVIRONMENT.into());
    let mut tx = begin_live(&state.db, sdk.0.org_id, None).await?;
    let app = tx
        .get_app(sdk.0.app_id)
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    let flags = tx.list_flags(&app, false).await?;
    tx.commit().await?;
    let flags: Vec<SdkFlag> = flags
        .iter()
        .map(|f| SdkFlag::from_flag(f, &environment, sdk.sees_rules()))
        .collect();
    Ok(Json(FlagList {
        count: flags.len(),
        flags,
        organization_id: sdk.0.org_id.to_string(),
        application_id: Some(sdk.0.app_id.to_string()),
    }))
}

/// Org-wide ("enterprise") flags are not part of v1, so this answers the
/// documented shape with nothing in it rather than a 404 SDKs would log.
pub async fn enterprise_flags(sdk: Sdk) -> Json<FlagList> {
    Json(FlagList {
        flags: vec![],
        count: 0,
        organization_id: sdk.0.org_id.to_string(),
        application_id: None,
    })
}
