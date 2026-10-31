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
    /// there. Omit to leave the environments as they are.
    #[serde(default)]
    pub environments: Option<Vec<String>>,
    /// The complete new list of browser origins the app's public client key
    /// may be used from, for example ["https://app.example.com",
    /// "https://*.example.com"] (scheme and host, optional port, no path; a
    /// leading `*.` matches subdomains). An empty list removes the
    /// restriction. Omit to leave it as it is. Requests that send no `Origin`
    /// header (native apps, servers) are not checked, and the server key never
    /// is: this keeps other websites from using the public key, nothing more.
    #[serde(default)]
    pub allowed_origins: Option<Vec<String>>,
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

/// Why managing apps takes an owner or admin.
const APPS_NEED_ADMIN: &str = "managing apps and their SDK keys needs an owner or admin of this \
    organization; ask one to do it, or to change your role at the Savvagent platform";

const KEYS_NOTE: &str = "The server key (srv_…) is a secret and is shown only now: store it in \
    the application's secret manager as its Savvagent Flags API key for server-side SDKs. The client \
    key (sdk_…) is public and can ship in browser or mobile code; list_apps shows it again.";

#[tool_router(router = apps_router, vis = "pub(crate)")]
impl Flags {
    #[tool(
        name = "list_apps",
        description = "The apps in this organization (each codebase or service that evaluates \
                       flags), with their environments, allowed origins for the public client \
                       key (empty means unrestricted), and public client keys. Free.",
        annotations(read_only_hint = true)
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
                       secret. Needs the apps:admin scope and an owner or admin role.",
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = false
        )
    )]
    pub async fn create_app(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<CreateAppArgs>,
    ) -> Result<Json<out::AppKeysOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(APPS_ADMIN).mcp()?;
        self.require_admin(&caller, APPS_NEED_ADMIN).await?;
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
        description = "Replace an app's list of environments and/or the list of browser origins \
                       its public client key may be used from (allowedOrigins; empty removes the \
                       restriction). Send at least one. Needs the apps:admin scope and an owner \
                       or admin role.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    pub async fn update_app(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<UpdateAppArgs>,
    ) -> Result<Json<out::AppOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(APPS_ADMIN).mcp()?;
        self.require_admin(&caller, APPS_NEED_ADMIN).await?;
        if args.environments.is_none() && args.allowed_origins.is_none() {
            return Err(flags_core::Error::Invalid(
                "send environments, allowedOrigins, or both: there is nothing to change".into(),
            ))
            .mcp();
        }
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "update_app").await?;
        let mut app = tx.resolve_app(&args.app).await.mcp()?;
        if let Some(environments) = &args.environments {
            app = tx
                .set_app_environments(&app, environments, caller.user_id)
                .await
                .mcp()?;
        }
        if let Some(origins) = &args.allowed_origins {
            app = tx
                .set_app_allowed_origins(&app, origins, caller.user_id)
                .await
                .mcp()?;
        }
        tx.commit().await.mcp()?;
        Ok(Json(out::AppOut { app }))
    }

    #[tool(
        name = "rotate_app_keys",
        description = "Replace an app's SDK key(s). The old key stops working at once, so the new \
                       one must be deployed promptly. Use when a server key may have leaked. Needs \
                       the apps:admin scope and an owner or admin role.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    pub async fn rotate_app_keys(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<RotateKeysArgs>,
    ) -> Result<Json<out::AppKeysOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(APPS_ADMIN).mcp()?;
        self.require_admin(&caller, APPS_NEED_ADMIN).await?;
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
