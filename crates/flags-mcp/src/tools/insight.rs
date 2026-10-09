//! Reading a flag's behaviour: dry-run evaluation, history, and health. All free.

use flags_core::apps::AppsExt;
use flags_core::eval::{self, Context};
use flags_core::flags::FlagsExt;
use flags_core::scopes::FLAGS_READ;
use flags_core::telemetry;
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use rmcp::{tool, tool_router};
use serde::Deserialize;

use super::out;
use crate::server::{Flags, McpResult};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HistoryArgs {
    pub app: String,
    pub key: String,
    /// How many versions, newest first. Defaults to 20, at most 200.
    #[serde(default)]
    pub limit: Option<i64>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EvaluateArgs {
    pub app: String,
    pub key: String,
    /// The environment to evaluate in, for example "production".
    pub environment: String,
    /// The context an SDK would send: user_id / anonymous_id / session_id,
    /// language, organization_id, and custom `attributes`.
    #[serde(default)]
    pub context: Context,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HealthArgs {
    pub app: String,
    pub key: String,
    /// Limit to one environment. Defaults to all.
    #[serde(default)]
    pub environment: Option<String>,
    /// How far back to look, in hours. Defaults to 24.
    #[serde(default)]
    pub window_hours: Option<i64>,
}

#[tool_router(router = insight_router, vis = "pub(crate)")]
impl Flags {
    #[tool(
        name = "flag_history",
        description = "Every version of a flag, newest first: what changed, who changed it, why, \
                       and the full state at that version. rollback_flag can restore any of them. \
                       Free."
    )]
    pub async fn flag_history(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<HistoryArgs>,
    ) -> Result<Json<out::HistoryOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(FLAGS_READ).mcp()?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "flag_history").await?;
        let app = tx.resolve_app(&args.app).await.mcp()?;
        let flag = tx.require_flag(&app, &args.key).await.mcp()?;
        let versions = tx
            .flag_history(&flag, args.limit.unwrap_or(20))
            .await
            .mcp()?;
        tx.commit().await.mcp()?;
        Ok(Json(out::HistoryOut {
            key: flag.key,
            current_version: flag.version,
            versions,
        }))
    }

    #[tool(
        name = "evaluate_flag",
        description = "What a given user would get from a flag right now, and why (which rule \
                       matched, or their rollout bucket). Changes nothing and records nothing. \
                       Use it to check targeting before turning a flag on. Free."
    )]
    pub async fn evaluate_flag(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<EvaluateArgs>,
    ) -> Result<Json<out::EvaluationOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(FLAGS_READ).mcp()?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "evaluate_flag").await?;
        let app = tx.resolve_app(&args.app).await.mcp()?;
        let flag = tx.require_flag(&app, &args.key).await.mcp()?;
        tx.commit().await.mcp()?;
        let evaluation = eval::evaluate(flag.view(), args.environment.trim(), &args.context);
        Ok(Json(out::EvaluationOut {
            key: flag.key,
            environment: args.environment,
            version: flag.version,
            evaluation,
        }))
    }

    #[tool(
        name = "flag_health",
        description = "How a flag is behaving in production, from what the SDKs report: \
                       evaluations with it on and off, errors raised in code behind it, the error \
                       rate on versus off, the most common errors, and a one-line assessment. Check \
                       it between rollout steps and before going to 100%. Free."
    )]
    pub async fn flag_health(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<HealthArgs>,
    ) -> Result<Json<out::HealthOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(FLAGS_READ).mcp()?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "flag_health").await?;
        let app = tx.resolve_app(&args.app).await.mcp()?;
        let flag = tx.require_flag(&app, &args.key).await.mcp()?;
        let health = telemetry::health(
            &mut tx,
            flag.id,
            args.environment.as_deref().map(str::trim),
            args.window_hours.unwrap_or(24),
        )
        .await
        .mcp()?;
        tx.commit().await.mcp()?;
        Ok(Json(out::HealthOut {
            key: flag.key,
            version: flag.version,
            health,
        }))
    }
}
