//! The MCP service: the state a tool call runs against, and the shape every
//! tool has in common.
//!
//! **The caller is per request.** `rmcp` builds one service per MCP session,
//! but the principal belongs to the HTTP request: [`crate::auth::require_bearer`]
//! inserts it into the request's extensions and every handler reads it back
//! with [`Flags::caller`]. A revoked token stops working on the next call, not
//! at the end of a session.
//!
//! **The tenant comes from the token, and only from it.** No tool takes an org
//! argument. [`Flags::tx`] opens a transaction pinned to the token's org, which
//! is the only way `flags-core` hands out tenant data.
//!
//! **A tool body:** check the scope, open the transaction, charge, do one
//! thing, commit. Charging inside the tool's own transaction means a failed
//! call is never billed.

use std::sync::Arc;

use flags_core::usage::Meter;
use otto_resource::PlatformClient;
use otto_tenant::{Db, Tx};
use rmcp::handler::server::tool::ToolRouter;
use rmcp::model::{ErrorData, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool_handler, ServerHandler};

use crate::auth::{MissingScope, Principal};
use crate::error;

/// What the server tells an agent before it calls anything. Written as
/// operating guidance: the moves an agent gets wrong in its first session.
pub(crate) const INSTRUCTIONS: &str = "\
otto-flags manages feature flags. You create flags, target them, roll them out, \
watch their health, and roll them back here; running applications evaluate them \
through an SDK, which never goes through you.

Getting started in a session:
  1. Call whoami to see which organization this token opens and what you may do.
  2. Call list_apps. Every flag belongs to an app (one per codebase or service \
that evaluates flags). If the app you need is missing, create_app makes one and \
returns its SDK keys; that needs the apps:admin scope and an owner or admin role.
  3. Call list_flags for the app, then get_flag for the one you are working on.

Shipping a feature behind a flag:
  - create_flag creates it switched off in every environment.
  - set_flag_environment turns it on per environment, sets a rollout percentage, \
adds targeting rules, or picks a default variation. Pass expected_version (the \
version get_flag showed you) so a concurrent change is refused instead of \
overwritten, and pass a reason: it is kept in the flag's history.
  - Raise a rollout in steps. Between steps, call flag_health: it compares the \
error rate with the flag on against the rate with it off, from what the SDKs \
report.
  - If something is wrong, rollback_flag restores the previous version (or any \
version in flag_history), and set_flag_environment with enabled=false is the \
kill switch for one environment.

evaluate_flag answers what a given user would get, and why, without touching \
anything. Use it to check targeting before turning a flag on.

You are billed for changes, not for looking: reads, evaluate_flag, \
flag_health, whoami and usage are free. Call usage to see where you stand.";

/// The service. Cheap to clone, because `rmcp` builds one per session.
#[derive(Clone)]
pub struct Flags {
    db: Db,
    platform: Arc<PlatformClient>,
    meter: Arc<Meter>,
    tool_router: ToolRouter<Self>,
}

impl Flags {
    pub fn new(db: Db, platform: Arc<PlatformClient>, meter: Meter) -> Self {
        Self {
            db,
            platform,
            meter: Arc::new(meter),
            tool_router: crate::tools::router(),
        }
    }

    pub fn platform(&self) -> &Arc<PlatformClient> {
        &self.platform
    }

    pub fn meter(&self) -> &Meter {
        &self.meter
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    /// The principal for the HTTP request this tool call arrived on.
    pub fn caller(&self, parts: &http::request::Parts) -> Result<Principal, ErrorData> {
        crate::auth::principal_from(parts).ok_or_else(error::unauthenticated)
    }

    /// Open a transaction pinned to the caller's org, refused if the org was
    /// deleted or the caller removed since their token was introspected.
    pub async fn tx(&self, caller: &Principal) -> Result<Tx<'static>, ErrorData> {
        // The quota lookup is a network call: make it before a pooled
        // connection is held.
        self.meter.warm(caller.org_id).await;
        flags_core::platform_events::begin_live(&self.db, caller.org_id, Some(caller.user_id))
            .await
            .mcp()
    }

    /// Record this call (and refuse it if the org is out of budget), inside
    /// the tool's own transaction and before it does anything.
    pub async fn charge(
        &self,
        tx: &mut Tx<'_>,
        caller: &Principal,
        tool: &str,
    ) -> Result<(), ErrorData> {
        self.meter.charge(tx, caller.user_id, tool).await.mcp()
    }

    /// Refuse unless the caller is an owner or admin of the org *now*.
    ///
    /// The role on the principal is as of the last introspection, cached for up
    /// to a minute, so a just-demoted admin would still pass a check against
    /// it. This asks the platform (its own cache is seconds). Fails closed: a
    /// platform that cannot answer is an error, never "allowed".
    pub async fn require_admin(&self, caller: &Principal) -> Result<(), ErrorData> {
        let member = self
            .platform
            .member(caller.org_id.as_uuid(), caller.user_id.as_uuid())
            .await
            .mcp()?
            .ok_or_else(|| error::from_core(&flags_core::Error::AccessRevoked))?;
        if member.role.can_administer() {
            Ok(())
        } else {
            Err(error::forbidden(
                "managing apps and their SDK keys needs an owner or admin of this organization; \
                 ask one to do it, or to change your role at the otto platform",
            ))
        }
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Flags {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(ServerCapabilities::builder().enable_tools().build());
        // Left unset this reports rmcp's own crate name and version.
        info.server_info = Implementation::new("otto-flags", env!("CARGO_PKG_VERSION"));
        info.instructions = Some(INSTRUCTIONS.to_string());
        info
    }
}

/// Map a domain, tenant, scope, or platform failure into the MCP envelope at
/// the point of the call. An extension trait because both sides are foreign
/// types; an explicit `.mcp()?` also marks every place a domain error crosses
/// into protocol space.
pub trait McpResult<T> {
    fn mcp(self) -> Result<T, ErrorData>;
}

impl<T> McpResult<T> for flags_core::Result<T> {
    fn mcp(self) -> Result<T, ErrorData> {
        self.map_err(|e| error::from_core(&e))
    }
}

impl<T> McpResult<T> for otto_tenant::Result<T> {
    fn mcp(self) -> Result<T, ErrorData> {
        self.map_err(|e| error::from_tenant(&e))
    }
}

impl<T> McpResult<T> for Result<T, MissingScope> {
    fn mcp(self) -> Result<T, ErrorData> {
        self.map_err(|e| error::from_scope(&e))
    }
}

impl<T> McpResult<T> for otto_resource::Result<T> {
    fn mcp(self) -> Result<T, ErrorData> {
        self.map_err(|e| error::from_platform(&e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_instructions_name_the_opening_moves() {
        for tool in [
            "whoami",
            "list_apps",
            "create_app",
            "list_flags",
            "get_flag",
            "create_flag",
            "set_flag_environment",
            "flag_health",
            "rollback_flag",
            "flag_history",
            "evaluate_flag",
            "usage",
        ] {
            assert!(
                INSTRUCTIONS.contains(tool),
                "instructions should mention {tool}"
            );
        }
    }

    #[test]
    fn the_instructions_name_no_particular_agent() {
        let lowered = INSTRUCTIONS.to_lowercase();
        for client in ["claude", "copilot", "cursor", "codex", "gemini"] {
            assert!(!lowered.contains(client), "instructions mention {client}");
        }
    }
}
