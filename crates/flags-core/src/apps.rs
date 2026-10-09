//! Flag apps — the seam between MCP-driven flag management and SDK-driven
//! evaluation. See `migrations/0001_flag_apps.sql` for the schema this
//! module's queries assume.

use crate::error::{Error, Result};
use crate::ids::FlagAppId;
use otto_tenant::ids::OrgId;
use otto_tenant::Tx;
use serde::Serialize;
use sqlx::FromRow;

#[derive(Debug, Clone, PartialEq, Serialize, FromRow, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlagApp {
    pub id: FlagAppId,
    pub org_id: OrgId,
    pub name: String,
    pub environments: Vec<String>,
    pub client_key: String,
    pub server_key: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

const FLAG_APP_COLS: &str = "id, org_id, name, environments, client_key, server_key, created_at";

/// Not a secret-strength generator (no signing, no HMAC) — these are opaque
/// bearer identifiers an evaluation server looks up, not credentials this
/// crate verifies cryptographically. Revisit if/when the evaluation server's
/// own auth model is designed.
fn generate_key(prefix: &str) -> String {
    format!("{prefix}_{}", uuid::Uuid::new_v4().simple())
}

/// Extension methods on [`otto_tenant::Tx`] for flag-app management. An
/// extension trait rather than inherent methods because `Tx` is owned by
/// `otto-tenant`, which knows nothing of flag apps — see that crate's
/// `db.rs` doc comment.
pub trait AppsExt {
    fn create_app(
        &mut self,
        name: &str,
        environments: &[String],
    ) -> impl std::future::Future<Output = Result<FlagApp>> + Send;

    fn get_app(
        &mut self,
        id: FlagAppId,
    ) -> impl std::future::Future<Output = Result<Option<FlagApp>>> + Send;

    fn list_apps(&mut self) -> impl std::future::Future<Output = Result<Vec<FlagApp>>> + Send;
}

impl AppsExt for Tx<'_> {
    async fn create_app(&mut self, name: &str, environments: &[String]) -> Result<FlagApp> {
        let name = name.trim();
        if name.is_empty() {
            return Err(Error::Invalid("a flag app needs a name".into()));
        }
        let environments: Vec<String> = if environments.is_empty() {
            vec!["production".to_string()]
        } else {
            environments.to_vec()
        };
        let org = self.org();
        let client_key = generate_key("flagsdk_client");
        let server_key = generate_key("flagsdk_server");

        let app = sqlx::query_as(&format!(
            "INSERT INTO flag_apps (org_id, name, environments, client_key, server_key) \
             VALUES ($1,$2,$3,$4,$5) RETURNING {FLAG_APP_COLS}"
        ))
        .bind(org)
        .bind(name)
        .bind(&environments)
        .bind(&client_key)
        .bind(&server_key)
        .fetch_one(self.conn())
        .await?;
        Ok(app)
    }

    async fn get_app(&mut self, id: FlagAppId) -> Result<Option<FlagApp>> {
        let org = self.org();
        let app = sqlx::query_as(&format!(
            "SELECT {FLAG_APP_COLS} FROM flag_apps WHERE org_id = $1 AND id = $2"
        ))
        .bind(org)
        .bind(id)
        .fetch_optional(self.conn())
        .await?;
        Ok(app)
    }

    async fn list_apps(&mut self) -> Result<Vec<FlagApp>> {
        let org = self.org();
        let apps = sqlx::query_as(&format!(
            "SELECT {FLAG_APP_COLS} FROM flag_apps WHERE org_id = $1 ORDER BY name"
        ))
        .bind(org)
        .fetch_all(self.conn())
        .await?;
        Ok(apps)
    }
}
