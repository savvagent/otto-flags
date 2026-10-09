//! Who am I, and how much budget is left. Both free: a caller must never have
//! to spend an operation to find out how many it has.

use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use rmcp::{tool, tool_router};
use serde::Deserialize;

use super::out;
use crate::server::{Flags, McpResult};

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct NoArgs {}

#[tool_router(router = org_router, vis = "pub(crate)")]
impl Flags {
    #[tool(
        name = "whoami",
        description = "Who this token says you are: your user, the one organization it opens, your \
                       role there, the scopes it carries, and how much of this month's allowance is \
                       left. Call this first in a session. Free."
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
        description = "How much of this organization's monthly allowance (shared across every otto \
                       service it uses) has been used and how much is left. Changes to apps and \
                       flags consume it; reads, evaluate_flag and flag_health do not. Free. If \
                       `warning` is true, tell the person you are working for."
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
        tx.commit().await.mcp()?;
        Ok(Json(out::UsageOut { usage }))
    }
}
