//! Feature flags — key, name, per-environment state, and variations for one
//! flag, scoped to one [`crate::apps::FlagApp`]. See
//! `migrations/0002_feature_flags.sql` for the schema and
//! `docs/specs/2026-09-15-otto-flags-design.md` §5–§6 for how this maps onto
//! the eventual MCP tool surface (`create_flag`, `update_flag`,
//! `archive_flag`, `set_targeting_rule`).

use crate::error::{Error, Result};
use crate::ids::{FlagAppId, FlagId};
use otto_tenant::ids::OrgId;
use otto_tenant::Tx;
use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, schemars::JsonSchema,
)]
#[sqlx(type_name = "flag_status", rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum FlagStatus {
    Active,
    Archived,
}

#[derive(Debug, Clone, PartialEq, Serialize, FromRow, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FeatureFlag {
    pub id: FlagId,
    pub org_id: OrgId,
    pub app_id: FlagAppId,
    pub key: String,
    pub name: String,
    pub description: Option<String>,
    pub status: FlagStatus,
    /// Per-environment state, keyed by environment name. See the migration's
    /// column comment for the shape.
    pub environments: serde_json::Value,
    /// Variation definitions this flag can resolve to, keyed by variation
    /// name.
    pub variations: serde_json::Value,
    pub version: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub archived_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// Fields an update may change. `None` leaves a field alone — the same PATCH
/// shape `otto-core::teams::TeamPatch` uses, for the same reason: an agent
/// updating one field of a flag should not have to first read and re-send
/// every other field.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FlagPatch {
    pub name: Option<String>,
    pub description: Option<String>,
    pub environments: Option<serde_json::Value>,
    pub variations: Option<serde_json::Value>,
}

const FLAG_COLS: &str = "id, org_id, app_id, key, name, description, status, environments, \
                          variations, version, created_at, updated_at, archived_at";

fn validate_key(key: &str) -> Result<String> {
    let key = key.trim();
    if key.is_empty() {
        return Err(Error::Invalid("a flag needs a key".into()));
    }
    if key.len() > 128 {
        return Err(Error::Invalid(
            "a flag key must be 128 characters or fewer".into(),
        ));
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
    {
        return Err(Error::Invalid(format!(
            "flag key {key:?} may contain only letters, digits, '-', '_' and '.'"
        )));
    }
    Ok(key.to_string())
}

/// Extension methods on [`otto_tenant::Tx`] for flag management. An
/// extension trait rather than inherent methods because `Tx` is owned by
/// `otto-tenant`, which knows nothing of flags.
pub trait FlagsExt {
    fn create_flag(
        &mut self,
        app_id: FlagAppId,
        key: &str,
        name: &str,
    ) -> impl std::future::Future<Output = Result<FeatureFlag>> + Send;

    fn get_flag(
        &mut self,
        id: FlagId,
    ) -> impl std::future::Future<Output = Result<Option<FeatureFlag>>> + Send;

    fn get_flag_by_key(
        &mut self,
        app_id: FlagAppId,
        key: &str,
    ) -> impl std::future::Future<Output = Result<Option<FeatureFlag>>> + Send;

    fn list_flags(
        &mut self,
        app_id: FlagAppId,
    ) -> impl std::future::Future<Output = Result<Vec<FeatureFlag>>> + Send;

    fn update_flag(
        &mut self,
        id: FlagId,
        patch: FlagPatch,
    ) -> impl std::future::Future<Output = Result<FeatureFlag>> + Send;

    fn archive_flag(
        &mut self,
        id: FlagId,
    ) -> impl std::future::Future<Output = Result<FeatureFlag>> + Send;
}

impl FlagsExt for Tx<'_> {
    async fn create_flag(
        &mut self,
        app_id: FlagAppId,
        key: &str,
        name: &str,
    ) -> Result<FeatureFlag> {
        let key = validate_key(key)?;
        let name = name.trim();
        let name = if name.is_empty() { &key } else { name };
        let org = self.org();

        sqlx::query_as(&format!(
            "INSERT INTO feature_flags (org_id, app_id, key, name) \
             VALUES ($1,$2,$3,$4) RETURNING {FLAG_COLS}"
        ))
        .bind(org)
        .bind(app_id)
        .bind(&key)
        .bind(name)
        .fetch_one(self.conn())
        .await
        .map_err(|e| match &e {
            sqlx::Error::Database(db) if db.is_unique_violation() => {
                Error::DuplicateFlagKey { key: key.clone() }
            }
            _ => Error::Db(e),
        })
    }

    async fn get_flag(&mut self, id: FlagId) -> Result<Option<FeatureFlag>> {
        let org = self.org();
        let flag = sqlx::query_as(&format!(
            "SELECT {FLAG_COLS} FROM feature_flags WHERE org_id = $1 AND id = $2"
        ))
        .bind(org)
        .bind(id)
        .fetch_optional(self.conn())
        .await?;
        Ok(flag)
    }

    async fn get_flag_by_key(
        &mut self,
        app_id: FlagAppId,
        key: &str,
    ) -> Result<Option<FeatureFlag>> {
        let org = self.org();
        let flag = sqlx::query_as(&format!(
            "SELECT {FLAG_COLS} FROM feature_flags WHERE org_id = $1 AND app_id = $2 AND key = $3"
        ))
        .bind(org)
        .bind(app_id)
        .bind(key)
        .fetch_optional(self.conn())
        .await?;
        Ok(flag)
    }

    async fn list_flags(&mut self, app_id: FlagAppId) -> Result<Vec<FeatureFlag>> {
        let org = self.org();
        let flags = sqlx::query_as(&format!(
            "SELECT {FLAG_COLS} FROM feature_flags \
             WHERE org_id = $1 AND app_id = $2 ORDER BY key"
        ))
        .bind(org)
        .bind(app_id)
        .fetch_all(self.conn())
        .await?;
        Ok(flags)
    }

    async fn update_flag(&mut self, id: FlagId, patch: FlagPatch) -> Result<FeatureFlag> {
        let org = self.org();
        let flag = sqlx::query_as(&format!(
            "UPDATE feature_flags SET \
               name = COALESCE($3, name), \
               description = COALESCE($4, description), \
               environments = COALESCE($5, environments), \
               variations = COALESCE($6, variations), \
               version = version + 1, \
               updated_at = now() \
             WHERE org_id = $1 AND id = $2 \
             RETURNING {FLAG_COLS}"
        ))
        .bind(org)
        .bind(id)
        .bind(patch.name)
        .bind(patch.description)
        .bind(patch.environments)
        .bind(patch.variations)
        .fetch_optional(self.conn())
        .await?;
        flag.ok_or(Error::FlagNotFound(id))
    }

    async fn archive_flag(&mut self, id: FlagId) -> Result<FeatureFlag> {
        let org = self.org();
        let flag = sqlx::query_as(&format!(
            "UPDATE feature_flags SET \
               status = 'archived', archived_at = now(), version = version + 1, updated_at = now() \
             WHERE org_id = $1 AND id = $2 \
             RETURNING {FLAG_COLS}"
        ))
        .bind(org)
        .bind(id)
        .fetch_optional(self.conn())
        .await?;
        flag.ok_or(Error::FlagNotFound(id))
    }
}
