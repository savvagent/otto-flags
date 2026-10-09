//! Flag apps and their SDK keys.

use flags_core::apps::{AppsExt, Rotate};
use flags_core::scopes::{APPS_ADMIN, FLAGS_READ};
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use rmcp::{tool, tool_router};
use serde::Deserialize;

use super::org::NoArgs;
use super::out;
use crate::server::{Flags, McpResult};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CreateAppArgs {
    /// Short, stable name agents will use for this app, unique in your
    /// organization: usually the codebase or service name (for example
    /// "checkout-web"). Letters, digits, '-', '_' and '.'.
    pub name: String,
    /// Environments this app evaluates flags in, for example
    /// ["production", "staging", "development"]. Defaults to ["production"].
    #[serde(default)]
    pub environments: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UpdateAppArgs {
    /// The app's name (or id).
    pub app: String,
    /// The complete new list of environments. Removing one does not delete
    /// flags' settings for it, but they can no longer be changed or evaluated
    /// there.
    pub environments: Vec<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RotateKeysArgs {
    /// The app's name (or id).
    pub app: String,
    /// Which key to replace: "server" (secret, `srv_…`), "client" (public,
    /// `sdk_…`), or "both". The old key stops working immediately.
    pub which: Rotate,
}

const KEYS_NOTE: &str = "The server key (srv_…) is a secret and is shown only now: store it in \
    the application's secret manager as its Otto Flags API key for server-side SDKs. The client \
    key (sdk_…) is public and can ship in browser or mobile code; list_apps shows it again.";

#[tool_router(router = apps_router, vis = "pub(crate)")]
impl Flags {
    #[tool(
        name = "list_apps",
        description = "The apps in this organization (each codebase or service that evaluates \
                       flags), with their environments and public client keys. Free."
    )]
    pub async fn list_apps(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(_): Parameters<NoArgs>,
    ) -> Result<Json<out::AppsOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(FLAGS_READ).mcp()?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "list_apps").await?;
        let apps = tx.list_apps().await.mcp()?;
        tx.commit().await.mcp()?;
        Ok(Json(out::AppsOut { apps }))
    }

    #[tool(
        name = "create_app",
        description = "Register an app (a codebase or service that will evaluate flags) and issue \
                       its SDK keys. Returns the server key once; tell the person to store it as a \
                       secret. Needs the apps:admin scope and an owner or admin role."
    )]
    pub async fn create_app(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<CreateAppArgs>,
    ) -> Result<Json<out::AppKeysOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(APPS_ADMIN).mcp()?;
        self.require_admin(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "create_app").await?;
        let (app, keys) = tx
            .create_app(&args.name, &args.environments, caller.user_id)
            .await
            .mcp()?;
        tx.commit().await.mcp()?;
        Ok(Json(out::AppKeysOut {
            app,
            keys,
            note: KEYS_NOTE.into(),
        }))
    }

    #[tool(
        name = "update_app",
        description = "Replace an app's list of environments. Needs the apps:admin scope and an \
                       owner or admin role."
    )]
    pub async fn update_app(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<UpdateAppArgs>,
    ) -> Result<Json<out::AppOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(APPS_ADMIN).mcp()?;
        self.require_admin(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "update_app").await?;
        let app = tx.resolve_app(&args.app).await.mcp()?;
        let app = tx
            .set_app_environments(&app, &args.environments, caller.user_id)
            .await
            .mcp()?;
        tx.commit().await.mcp()?;
        Ok(Json(out::AppOut { app }))
    }

    #[tool(
        name = "rotate_app_keys",
        description = "Replace an app's SDK key(s). The old key stops working at once, so the new \
                       one must be deployed promptly. Use when a server key may have leaked. Needs \
                       the apps:admin scope and an owner or admin role."
    )]
    pub async fn rotate_app_keys(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<RotateKeysArgs>,
    ) -> Result<Json<out::AppKeysOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(APPS_ADMIN).mcp()?;
        self.require_admin(&caller).await?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "rotate_app_keys").await?;
        let app = tx.resolve_app(&args.app).await.mcp()?;
        let (app, keys) = tx
            .rotate_keys(&app, args.which, caller.user_id)
            .await
            .mcp()?;
        tx.commit().await.mcp()?;
        Ok(Json(out::AppKeysOut {
            app,
            keys,
            note: KEYS_NOTE.into(),
        }))
    }
}
