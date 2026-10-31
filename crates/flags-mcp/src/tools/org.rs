//! Who am I, and where does the org stand. Every tool here is free, these two
//! included. Billing is on production evaluations by the SDKs
//! (`otto-flags/evaluations`), so `whoami` and `usage` report the org's
//! `meters`, `budget` and `blockedReason` from the platform. A blocked reason
//! never refuses a tool: evaluations keep serving and are not billed while it
//! holds.

use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use rmcp::{tool, tool_router};
use serde::Deserialize;

use super::out;
use crate::server::{Flags, McpResult};
use flags_core::metering::MeteringExt;

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct NoArgs {}

#[tool_router(router = org_router, vis = "pub(crate)")]
impl Flags {
    #[tool(
        name = "whoami",
        description = "Who this token says you are at Savvagent Flags: your user, the one organization it opens, your \
                       role there, the scopes it carries, and where the organization stands on \
                       billing: `meters` (this month's usage), `budget`, and `blockedReason`. \
                       Every tool is free; billing is on production evaluations by the SDKs \
                       (otto-flags/evaluations). Call this first in a session.",
        annotations(read_only_hint = true)
    )]
    pub async fn whoami(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(_): Parameters<NoArgs>,
    ) -> Result<Json<out::WhoAmI>, ErrorData> {
        let caller = self.caller(&parts)?;
        let member = self
            .platform()
            .member(caller.org_id.as_uuid(), caller.user_id.as_uuid())
            .await
            .mcp()?;
        let usage = self.meter().report(caller.org_id).await.mcp()?;

        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "whoami").await?;
        tx.commit().await.mcp()?;

        let user = member.as_ref().map(|m| &m.user);
        let org = member.as_ref().map(|m| &m.org);
        Ok(Json(out::WhoAmI {
            user: out::UserOut {
                id: caller.user_id,
                email: user.and_then(|u| u.email.clone()),
                name: user.and_then(|u| u.name.clone()),
            },
            org: out::OrgOut {
                id: caller.org_id,
                slug: org.map(|o| o.slug.clone()),
                name: org.map(|o| o.name.clone()),
                plan: org.map(|o| o.plan.clone()),
            },
            role: member.as_ref().map(|m| m.role.into()),
            token: out::TokenOut {
                kind: match caller.kind {
                    otto_resource::TokenKind::Oauth => "oauth",
                    otto_resource::TokenKind::Pat => "pat",
                },
                client_id: caller.client_id,
                scopes: caller.scopes,
                expires_at: caller.expires_at,
            },
            usage,
        }))
    }

    #[tool(
        name = "usage",
        description = "Where this organization stands on billing: `meters` (this month's \
                       usage, such as otto-flags/evaluations), `budget` (the monthly budget and \
                       spend, if one is set), and `blockedReason` (set when the platform blocks \
                       billable usage; evaluations keep serving and are not billed meanwhile). \
                       Every tool is free; billing is on evaluations by the SDKs. \
                       `evaluations` splits this month's closed hours by key kind: `clientServed` \
                       (public client keys, which anyone holding the key can drive), \
                       `serverServed`, and `clipped` (client-served above the per-app hourly \
                       ceiling: answered, not billed), with the busiest `apps`. A client-served \
                       spike the organization did not cause is a reason to rotate the app's client \
                       key. The older allowance fields are deprecated. If `blockedReason` is set, \
                       tell the person you are working for.",
        annotations(read_only_hint = true)
    )]
    pub async fn usage(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(_): Parameters<NoArgs>,
    ) -> Result<Json<out::UsageOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        let usage = self.meter().report(caller.org_id).await.mcp()?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "usage").await?;
        let evaluations = tx.evaluation_split().await.mcp()?;
        tx.commit().await.mcp()?;
        Ok(Json(out::UsageOut { usage, evaluations }))
    }
}
