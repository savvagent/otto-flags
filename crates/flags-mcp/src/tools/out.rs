//! What tools return. Every result is an object with named fields; payloads are
//! `flags-core`'s own domain types rather than mirrored view structs, so a new
//! column cannot be missed in one copy.

use flags_core::apps::{FlagApp, IssuedKeys};
use flags_core::eval::Evaluation;
use flags_core::flags::{FeatureFlag, FlagVersion};
use flags_core::telemetry::Health;
use flags_core::usage::Status;
use otto_tenant::ids::{OrgId, UserId};
use schemars::JsonSchema;
use serde::Serialize;

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AppOut {
    pub app: FlagApp,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AppsOut {
    pub apps: Vec<FlagApp>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct AppKeysOut {
    pub app: FlagApp,
    /// The keys just issued. The server key is shown only this once.
    pub keys: IssuedKeys,
    /// What to do with them.
    pub note: String,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlagOut {
    pub flag: FeatureFlag,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlagsOut {
    pub app: String,
    pub flags: Vec<FeatureFlag>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RollbackOut {
    pub flag: FeatureFlag,
    /// The version whose state was restored.
    pub restored_from: i32,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HistoryOut {
    pub key: String,
    pub current_version: i32,
    /// Newest first.
    pub versions: Vec<FlagVersion>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EvaluationOut {
    pub key: String,
    pub environment: String,
    pub version: i32,
    pub evaluation: Evaluation,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct HealthOut {
    pub key: String,
    pub version: i32,
    pub health: Health,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Owner,
    Admin,
    Member,
}

impl From<otto_resource::Role> for Role {
    fn from(r: otto_resource::Role) -> Self {
        match r {
            otto_resource::Role::Owner => Role::Owner,
            otto_resource::Role::Admin => Role::Admin,
            otto_resource::Role::Member => Role::Member,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct WhoAmI {
    pub user: UserOut,
    pub org: OrgOut,
    /// The caller's role in this org, as the platform reports it now.
    pub role: Option<Role>,
    pub token: TokenOut,
    pub usage: Status,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UserOut {
    pub id: UserId,
    pub email: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct OrgOut {
    pub id: OrgId,
    pub slug: Option<String>,
    pub name: Option<String>,
    pub plan: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct TokenOut {
    /// "oauth" for a browser-authorized token, "pat" for a personal access token.
    pub kind: &'static str,
    pub client_id: Option<String>,
    pub scopes: Vec<String>,
    pub expires_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UsageOut {
    pub usage: Status,
}
