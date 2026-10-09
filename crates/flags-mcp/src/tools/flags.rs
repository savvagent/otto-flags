//! Creating and changing flags. Every change becomes a new version with the
//! caller and their reason, so it can be read back (`flag_history`) and undone
//! (`rollback_flag`).

use flags_core::apps::AppsExt;
use flags_core::eval::Rule;
use flags_core::flags::{ChangeMeta, EnvPatch, FlagPatch, FlagsExt, NewFlag};
use flags_core::scopes::{FLAGS_READ, FLAGS_WRITE};
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use rmcp::{tool, tool_router};
use serde::Deserialize;
use serde_json::Value;

use super::out;
use crate::auth::Principal;
use crate::server::{Flags, McpResult};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListFlagsArgs {
    /// The app's name (or id).
    pub app: String,
    /// Include archived flags. Defaults to false.
    #[serde(default)]
    pub include_archived: bool,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlagRefArgs {
    /// The app's name (or id).
    pub app: String,
    /// The flag's key, as the code checks it (for example "new-checkout").
    pub key: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateFlagArgs {
    pub app: String,
    /// The key application code will check. Letters, digits, '-', '_', '.';
    /// at most 128 characters. Cannot be changed later.
    pub key: String,
    /// Human-readable name. Defaults to the key.
    #[serde(default)]
    pub name: Option<String>,
    /// What the flag gates and when it can be removed.
    #[serde(default)]
    pub description: Option<String>,
    /// Named variations for experiments or multi-valued flags, for example
    /// {"control": {}, "treatment": {"weight": 1, "configuration": {"color": "blue"}}}.
    /// Leave out for a plain on/off flag.
    #[serde(default)]
    pub variations: Option<Value>,
    /// Configuration the SDK returns with the flag (any JSON).
    #[serde(default)]
    pub configuration: Option<Value>,
    /// Why this flag exists; kept in its history.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdateFlagArgs {
    pub app: String,
    pub key: String,
    #[serde(default)]
    pub name: Option<String>,
    /// An empty string clears it.
    #[serde(default)]
    pub description: Option<String>,
    /// Replaces the whole set of variations. A variation still named by an
    /// environment's rules or default cannot be removed until that
    /// environment stops using it.
    #[serde(default)]
    pub variations: Option<Value>,
    /// Replaces the configuration; JSON null clears it.
    #[serde(default)]
    pub configuration: Option<Value>,
    /// The version you last read. If the flag has changed since, the call is
    /// refused rather than overwriting someone else's change.
    #[serde(default)]
    pub expected_version: Option<i32>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetEnvironmentArgs {
    pub app: String,
    pub key: String,
    /// One of the app's environments, for example "production".
    pub environment: String,
    /// Master switch for this environment. Off means off for everyone.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// Share of identified users (by user_id, else anonymous_id, else
    /// session_id) the flag is on for when no rule matches, 0-100. The same
    /// user stays in as the percentage rises.
    #[serde(default)]
    pub rollout_percentage: Option<f64>,
    /// Replaces this environment's targeting rules (an empty list clears them).
    /// Evaluated in order; the first match decides. Each rule is
    /// {"attribute", "operator", "values", "enabled"?, "variation"?, "description"?}.
    /// Operators: in, not_in, equals, not_equals, contains, starts_with,
    /// ends_with, gt, gte, lt, lte, exists, not_exists.
    #[serde(default)]
    pub rules: Option<Vec<Rule>>,
    /// Variation to serve when the flag is on and no rule names one. An empty
    /// string clears it (variations are then split by weight).
    #[serde(default)]
    pub default_variation: Option<String>,
    #[serde(default)]
    pub expected_version: Option<i32>,
    /// Why; kept in the flag's history. Say what you are rolling out and why now.
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ChangeArgs {
    pub app: String,
    pub key: String,
    #[serde(default)]
    pub expected_version: Option<i32>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RollbackArgs {
    pub app: String,
    pub key: String,
    /// The version to restore. Defaults to the one before the current version.
    /// flag_history lists them.
    #[serde(default)]
    pub to_version: Option<i32>,
    #[serde(default)]
    pub expected_version: Option<i32>,
    /// What went wrong; kept in the flag's history.
    #[serde(default)]
    pub reason: Option<String>,
}

fn meta(caller: &Principal, expected_version: Option<i32>, reason: Option<String>) -> ChangeMeta {
    ChangeMeta {
        actor: caller.user_id,
        reason,
        expected_version,
    }
}

#[tool_router(router = flags_router, vis = "pub(crate)")]
impl Flags {
    #[tool(
        name = "list_flags",
        description = "The flags in an app, with each one's state in every environment. Free."
    )]
    pub async fn list_flags(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ListFlagsArgs>,
    ) -> Result<Json<out::FlagsOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(FLAGS_READ).mcp()?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "list_flags").await?;
        let app = tx.resolve_app(&args.app).await.mcp()?;
        let flags = tx.list_flags(&app, args.include_archived).await.mcp()?;
        tx.commit().await.mcp()?;
        Ok(Json(out::FlagsOut {
            app: app.name,
            flags,
        }))
    }

    #[tool(
        name = "get_flag",
        description = "One flag's full state: environments (enabled, rollout, rules, default \
                       variation), variations, configuration, and its current version. Read this \
                       before changing a flag and pass its version as expectedVersion. Free."
    )]
    pub async fn get_flag(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<FlagRefArgs>,
    ) -> Result<Json<out::FlagOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(FLAGS_READ).mcp()?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "get_flag").await?;
        let app = tx.resolve_app(&args.app).await.mcp()?;
        let flag = tx.require_flag(&app, &args.key).await.mcp()?;
        tx.commit().await.mcp()?;
        Ok(Json(out::FlagOut { flag }))
    }

    #[tool(
        name = "create_flag",
        description = "Create a flag in an app. It starts switched off in every environment; turn \
                       it on with set_flag_environment. Put the key in the code that checks the \
                       flag."
    )]
    pub async fn create_flag(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<CreateFlagArgs>,
    ) -> Result<Json<out::FlagOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(FLAGS_WRITE).mcp()?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "create_flag").await?;
        let app = tx.resolve_app(&args.app).await.mcp()?;
        let flag = tx
            .create_flag(
                &app,
                NewFlag {
                    key: args.key,
                    name: args.name,
                    description: args.description,
                    variations: args.variations,
                    configuration: args.configuration,
                },
                caller.user_id,
                args.reason,
            )
            .await
            .mcp()?;
        tx.commit().await.mcp()?;
        Ok(Json(out::FlagOut { flag }))
    }

    #[tool(
        name = "update_flag",
        description = "Change a flag's name, description, variations, or configuration. To turn \
                       it on or off, roll it out, or target it, use set_flag_environment."
    )]
    pub async fn update_flag(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<UpdateFlagArgs>,
    ) -> Result<Json<out::FlagOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(FLAGS_WRITE).mcp()?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "update_flag").await?;
        let app = tx.resolve_app(&args.app).await.mcp()?;
        let flag = tx
            .update_flag(
                &app,
                &args.key,
                FlagPatch {
                    name: args.name,
                    description: args.description,
                    variations: args.variations,
                    configuration: args.configuration,
                },
                meta(&caller, args.expected_version, args.reason),
            )
            .await
            .mcp()?;
        tx.commit().await.mcp()?;
        Ok(Json(out::FlagOut { flag }))
    }

    #[tool(
        name = "set_flag_environment",
        description = "Turn a flag on or off in one environment, set its rollout percentage, \
                       replace its targeting rules, or choose its default variation. Only the \
                       fields you pass change. Applies to running apps within seconds. Pass \
                       expectedVersion and a reason."
    )]
    pub async fn set_flag_environment(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<SetEnvironmentArgs>,
    ) -> Result<Json<out::FlagOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(FLAGS_WRITE).mcp()?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "set_flag_environment")
            .await?;
        let app = tx.resolve_app(&args.app).await.mcp()?;
        let patch = EnvPatch {
            enabled: args.enabled,
            rollout_percentage: args.rollout_percentage,
            rules: args.rules,
            default_variation: args
                .default_variation
                .map(|v| Some(v.trim().to_string()).filter(|v| !v.is_empty())),
        };
        let flag = tx
            .set_environment(
                &app,
                &args.key,
                &args.environment,
                patch,
                meta(&caller, args.expected_version, args.reason),
            )
            .await
            .mcp()?;
        tx.commit().await.mcp()?;
        Ok(Json(out::FlagOut { flag }))
    }

    #[tool(
        name = "archive_flag",
        description = "Archive a flag once the code no longer checks it. Archived flags evaluate \
                       as off everywhere and drop out of list_flags; restore_flag brings one back."
    )]
    pub async fn archive_flag(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ChangeArgs>,
    ) -> Result<Json<out::FlagOut>, ErrorData> {
        self.set_archived(parts, args, true, "archive_flag").await
    }

    #[tool(
        name = "restore_flag",
        description = "Bring an archived flag back, with the state it had when archived."
    )]
    pub async fn restore_flag(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ChangeArgs>,
    ) -> Result<Json<out::FlagOut>, ErrorData> {
        self.set_archived(parts, args, false, "restore_flag").await
    }

    #[tool(
        name = "rollback_flag",
        description = "Restore a flag to an earlier version (default: the previous one), as a new \
                       version. Use when a change caused trouble; flag_health shows whether it \
                       did, flag_history shows the versions."
    )]
    pub async fn rollback_flag(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<RollbackArgs>,
    ) -> Result<Json<out::RollbackOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(FLAGS_WRITE).mcp()?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "rollback_flag").await?;
        let app = tx.resolve_app(&args.app).await.mcp()?;
        let (flag, restored_from) = tx
            .rollback_flag(
                &app,
                &args.key,
                args.to_version,
                meta(&caller, args.expected_version, args.reason),
            )
            .await
            .mcp()?;
        tx.commit().await.mcp()?;
        Ok(Json(out::RollbackOut {
            flag,
            restored_from,
        }))
    }
}

impl Flags {
    async fn set_archived(
        &self,
        parts: http::request::Parts,
        args: ChangeArgs,
        archived: bool,
        tool: &str,
    ) -> Result<Json<out::FlagOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(FLAGS_WRITE).mcp()?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, tool).await?;
        let app = tx.resolve_app(&args.app).await.mcp()?;
        let flag = tx
            .set_archived(
                &app,
                &args.key,
                archived,
                meta(&caller, args.expected_version, args.reason),
            )
            .await
            .mcp()?;
        tx.commit().await.mcp()?;
        Ok(Json(out::FlagOut { flag }))
    }
}
