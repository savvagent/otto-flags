//! Evaluation and flag listing for SDKs.

use axum::extract::{FromRequestParts, Path, Query, State};
use axum::Json;
use flags_core::apps::{origin_allowed, AppsExt, FlagApp};
use flags_core::eval::{self, Context};
use flags_core::flags::{FlagStatus, FlagsExt, SdkFlag};
use flags_core::keys::{self, KeyKind, KeyOwner};
use flags_core::platform_events::begin_live;
use http::request::Parts;
use http::HeaderMap;
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
        match keys::resolve_cached(&state.db, &state.unknown_keys, &state.key_lookups, key).await? {
            Some(owner) => Ok(Sdk(owner, keys::hash(key))),
            None => Err(ApiError::unauthorized()),
        }
    }
}

/// Refuse a client key used from a browser origin the app does not allow.
///
/// Only a request that carries an `Origin` header is checked (browsers always
/// send one on a cross-origin call; native apps and servers send none), only
/// with a client key, and only when the app lists origins. A deterrent against
/// casual use of the public key from another site, not authentication: a
/// script can send any `Origin`, or none (#143).
fn check_origin(sdk: &Sdk, headers: &HeaderMap, app: &FlagApp) -> ApiResult<()> {
    if sdk.0.kind != KeyKind::Client || app.allowed_origins.is_empty() {
        return Ok(());
    }
    let Some(origin) = headers.get(http::header::ORIGIN) else {
        return Ok(());
    };
    // A header that is not text matches nothing.
    if origin
        .to_str()
        .is_ok_and(|o| origin_allowed(&app.allowed_origins, o))
    {
        Ok(())
    } else {
        Err(ApiError::origin_not_allowed())
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
    headers: HeaderMap,
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
    // One primary-key read in the transaction already open: the app's
    // environments and allowed origins. The caller names the environment, and
    // a public client key can name any, so only an app's own environments are
    // counted (#143). An unknown one is answered exactly as before. A client
    // key from an origin the app does not allow is refused, and not counted.
    let app = tx.get_app(sdk.0.app_id).await?;
    tx.commit().await?;
    if let Some(app) = &app {
        check_origin(&sdk, &headers, app)?;
    }
    let billed_environment = app.is_some_and(|app| app.environments.contains(&environment));
    let flag = flag
        .filter(|f| f.status == FlagStatus::Active)
        .ok_or_else(|| ApiError::flag_not_found(&key))?;

    let e = eval::evaluate(flag.view(), &environment, &context);
    if billed_environment {
        // The client key is public: its share is counted apart so the hour's
        // ceiling can apply to it (`flags_core::metering`).
        match sdk.0.kind {
            KeyKind::Client => state
                .evaluations
                .served_client(sdk.0.org_id, sdk.0.app_id, 1),
            KeyKind::Server => state.evaluations.served(sdk.0.org_id, sdk.0.app_id, 1),
        }
    }
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
    headers: HeaderMap,
    Query(q): Query<ListQuery>,
) -> ApiResult<Json<FlagList>> {
    let environment = q.environment.unwrap_or_else(|| DEFAULT_ENVIRONMENT.into());
    let mut tx = begin_live(&state.db, sdk.0.org_id, None).await?;
    let app = tx
        .get_app(sdk.0.app_id)
        .await?
        .ok_or_else(ApiError::unauthorized)?;
    check_origin(&sdk, &headers, &app)?;
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
