//! Feature flags and their history.
//!
//! Every change goes through one path, [`FlagsExt`]'s private `commit_change`:
//! bump the version (refusing if the caller's `expected_version` is stale),
//! write the new state, snapshot it into `flag_versions` with the actor and
//! reason, and queue a change notification for SDK streams. All in the caller's
//! transaction, so a failed call leaves no trace and a successful one leaves
//! exactly one version.
//!
//! Writes lock the flag row (`FOR UPDATE`) before computing the new state, so
//! two agents changing different environments of one flag at once both land:
//! the second waits for the first and merges into its result instead of
//! overwriting it.

use otto_tenant::ids::{OrgId, UserId};
use otto_tenant::Tx;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::FromRow;

use crate::apps::FlagApp;
use crate::error::{on_unique, Error, Result};
use crate::eval::{self, EnvConfig};
use crate::ids::{FlagAppId, FlagId};
use crate::notify;

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
    /// Per-environment state, keyed by environment name.
    pub environments: Value,
    /// Variations keyed by name.
    pub variations: Value,
    /// Configuration served when no variation supplies one.
    pub configuration: Option<Value>,
    pub version: i32,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub updated_at: chrono::DateTime<chrono::Utc>,
    pub archived_at: Option<chrono::DateTime<chrono::Utc>>,
}

impl FeatureFlag {
    pub fn view(&self) -> eval::FlagView<'_> {
        eval::FlagView {
            key: &self.key,
            archived: self.status == FlagStatus::Archived,
            environments: &self.environments,
            variations: &self.variations,
            configuration: self.configuration.as_ref(),
        }
    }

    /// The state a version snapshot records and a rollback restores.
    fn snapshot(&self) -> Value {
        serde_json::json!({
            "name": self.name,
            "description": self.description,
            "status": self.status,
            "environments": self.environments,
            "variations": self.variations,
            "configuration": self.configuration,
        })
    }
}

/// One entry in a flag's history.
#[derive(Debug, Clone, PartialEq, Serialize, FromRow, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlagVersion {
    pub version: i32,
    /// create, update, set_environment, archive, restore, or rollback.
    pub change: String,
    /// The flag's full state as of this version.
    pub snapshot: Value,
    pub actor_user_id: Option<UserId>,
    pub reason: Option<String>,
    pub created_at: chrono::DateTime<chrono::Utc>,
}

/// A new flag.
#[derive(Debug, Clone, Default)]
pub struct NewFlag {
    pub key: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub variations: Option<Value>,
    pub configuration: Option<Value>,
}

/// Fields an update may change; `None` leaves a field alone.
#[derive(Debug, Clone, Default)]
pub struct FlagPatch {
    pub name: Option<String>,
    pub description: Option<String>,
    pub variations: Option<Value>,
    pub configuration: Option<Value>,
}

/// A change to one environment; `None` leaves a field alone. `rules: Some(vec![])`
/// clears the rules.
#[derive(Debug, Clone, Default)]
pub struct EnvPatch {
    pub enabled: Option<bool>,
    pub rollout_percentage: Option<f64>,
    pub rules: Option<Vec<eval::Rule>>,
    /// `Some(None)` clears the default variation.
    pub default_variation: Option<Option<String>>,
}

/// Who is changing a flag and why, plus the version they believe they changed.
#[derive(Debug, Clone)]
pub struct ChangeMeta {
    pub actor: UserId,
    pub reason: Option<String>,
    pub expected_version: Option<i32>,
}

const COLS: &str = "id, org_id, app_id, key, name, description, status, environments, \
                    variations, configuration, version, created_at, updated_at, archived_at";

const MAX_DESCRIPTION: usize = 2_000;
const MAX_REASON: usize = 1_000;

fn validate_key(key: &str) -> Result<String> {
    let key = key.trim();
    if key.is_empty() || key.len() > 128 {
        return Err(Error::Invalid("a flag key must be 1-128 characters".into()));
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        return Err(Error::Invalid(format!(
            "flag key {key:?} may contain only letters, digits, '-', '_' and '.'"
        )));
    }
    Ok(key.to_string())
}

fn validate_text(what: &str, s: Option<String>, max: usize) -> Result<Option<String>> {
    match s.map(|s| s.trim().to_string()) {
        Some(s) if s.chars().count() > max => Err(Error::Invalid(format!(
            "{what} must be {max} characters or fewer"
        ))),
        Some(s) if s.is_empty() => Ok(None),
        other => Ok(other),
    }
}

fn invalid(e: String) -> Error {
    Error::Invalid(e)
}

pub trait FlagsExt {
    fn create_flag(
        &mut self,
        app: &FlagApp,
        new: NewFlag,
        actor: UserId,
        reason: Option<String>,
    ) -> impl std::future::Future<Output = Result<FeatureFlag>> + Send;

    fn list_flags(
        &mut self,
        app: &FlagApp,
        include_archived: bool,
    ) -> impl std::future::Future<Output = Result<Vec<FeatureFlag>>> + Send;

    fn find_flag(
        &mut self,
        app_id: FlagAppId,
        key: &str,
    ) -> impl std::future::Future<Output = Result<Option<FeatureFlag>>> + Send;

    /// [`FlagsExt::find_flag`], or a `flag_not_found` naming the app.
    fn require_flag(
        &mut self,
        app: &FlagApp,
        key: &str,
    ) -> impl std::future::Future<Output = Result<FeatureFlag>> + Send;

    fn update_flag(
        &mut self,
        app: &FlagApp,
        key: &str,
        patch: FlagPatch,
        meta: ChangeMeta,
    ) -> impl std::future::Future<Output = Result<FeatureFlag>> + Send;

    fn set_environment(
        &mut self,
        app: &FlagApp,
        key: &str,
        environment: &str,
        patch: EnvPatch,
        meta: ChangeMeta,
    ) -> impl std::future::Future<Output = Result<FeatureFlag>> + Send;

    fn set_archived(
        &mut self,
        app: &FlagApp,
        key: &str,
        archived: bool,
        meta: ChangeMeta,
    ) -> impl std::future::Future<Output = Result<FeatureFlag>> + Send;

    /// Restore the state recorded at `to_version` (default: the version before
    /// the current one), as a new version.
    fn rollback_flag(
        &mut self,
        app: &FlagApp,
        key: &str,
        to_version: Option<i32>,
        meta: ChangeMeta,
    ) -> impl std::future::Future<Output = Result<(FeatureFlag, i32)>> + Send;

    fn flag_history(
        &mut self,
        flag: &FeatureFlag,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Vec<FlagVersion>>> + Send;
}

impl FlagsExt for Tx<'_> {
    async fn create_flag(
        &mut self,
        app: &FlagApp,
        new: NewFlag,
        actor: UserId,
        reason: Option<String>,
    ) -> Result<FeatureFlag> {
        let key = validate_key(&new.key)?;
        let name = validate_text("name", new.name, 200)?.unwrap_or_else(|| key.clone());
        let description = validate_text("description", new.description, MAX_DESCRIPTION)?;
        let reason = validate_text("reason", reason, MAX_REASON)?;
        let variations =
            eval::validate_variations(&new.variations.unwrap_or(Value::Null)).map_err(invalid)?;
        // Every environment the app knows starts present and off, so the
        // flag's state is explicit everywhere rather than "not configured".
        let environments: Map<String, Value> = app
            .environments
            .iter()
            .map(|e| (e.clone(), serde_json::json!({"enabled": false})))
            .collect();
        let org = self.org();

        let flag: FeatureFlag = sqlx::query_as(&format!(
            "INSERT INTO feature_flags \
               (org_id, app_id, key, name, description, environments, variations, configuration, created_by) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9) RETURNING {COLS}"
        ))
        .bind(org)
        .bind(app.id)
        .bind(&key)
        .bind(&name)
        .bind(&description)
        .bind(Value::Object(environments))
        .bind(&variations)
        .bind(&new.configuration)
        .bind(actor)
        .fetch_one(self.conn())
        .await
        .map_err(|e| on_unique(e, || Error::DuplicateFlagKey { key: key.clone() }))?;

        record_version(self, &flag, "create", actor, reason.as_deref()).await?;
        notify::flag_changed(self, &flag, notify::Kind::Created).await?;
        Ok(flag)
    }

    async fn list_flags(
        &mut self,
        app: &FlagApp,
        include_archived: bool,
    ) -> Result<Vec<FeatureFlag>> {
        let org = self.org();
        Ok(sqlx::query_as(&format!(
            "SELECT {COLS} FROM feature_flags \
             WHERE org_id = $1 AND app_id = $2 AND ($3 OR status = 'active') ORDER BY key"
        ))
        .bind(org)
        .bind(app.id)
        .bind(include_archived)
        .fetch_all(self.conn())
        .await?)
    }

    async fn find_flag(&mut self, app_id: FlagAppId, key: &str) -> Result<Option<FeatureFlag>> {
        let org = self.org();
        Ok(sqlx::query_as(&format!(
            "SELECT {COLS} FROM feature_flags WHERE org_id = $1 AND app_id = $2 AND key = $3"
        ))
        .bind(org)
        .bind(app_id)
        .bind(key.trim())
        .fetch_optional(self.conn())
        .await?)
    }

    async fn require_flag(&mut self, app: &FlagApp, key: &str) -> Result<FeatureFlag> {
        self.find_flag(app.id, key)
            .await?
            .ok_or_else(|| Error::FlagNotFound {
                app: app.name.clone(),
                key: key.trim().to_string(),
            })
    }

    async fn update_flag(
        &mut self,
        app: &FlagApp,
        key: &str,
        patch: FlagPatch,
        meta: ChangeMeta,
    ) -> Result<FeatureFlag> {
        let mut flag = lock(self, app, key, meta.expected_version).await?;
        if let Some(name) = validate_text("name", patch.name, 200)? {
            flag.name = name;
        }
        if patch.description.is_some() {
            flag.description = validate_text("description", patch.description, MAX_DESCRIPTION)?;
        }
        if let Some(v) = patch.variations {
            let v = eval::validate_variations(&v).map_err(invalid)?;
            eval::validate_all_envs(&flag.environments, &v).map_err(|e| {
                Error::Invalid(format!(
                    "{e}. Change that environment with set_flag_environment first, then \
                     remove the variation"
                ))
            })?;
            flag.variations = v;
        }
        if let Some(c) = patch.configuration {
            flag.configuration = (!c.is_null()).then_some(c);
        }
        commit_change(self, flag, "update", &meta).await
    }

    async fn set_environment(
        &mut self,
        app: &FlagApp,
        key: &str,
        environment: &str,
        patch: EnvPatch,
        meta: ChangeMeta,
    ) -> Result<FeatureFlag> {
        let environment = environment.trim();
        if !app.environments.iter().any(|e| e == environment) {
            return Err(Error::Invalid(format!(
                "app {:?} has no environment {environment:?}; its environments are: {}. \
                 Add one with update_app",
                app.name,
                app.environments.join(", ")
            )));
        }
        let mut flag = lock(self, app, key, meta.expected_version).await?;
        let mut env: EnvConfig = flag
            .environments
            .get(environment)
            .map(|v| serde_json::from_value(v.clone()))
            .transpose()
            .map_err(|e| Error::Invalid(format!("stored environment is unreadable: {e}")))?
            .unwrap_or(EnvConfig {
                enabled: false,
                rollout_percentage: None,
                rules: vec![],
                default_variation: None,
            });
        if let Some(enabled) = patch.enabled {
            env.enabled = enabled;
        }
        if let Some(p) = patch.rollout_percentage {
            env.rollout_percentage = Some(p);
        }
        if let Some(rules) = patch.rules {
            env.rules = rules;
        }
        if let Some(v) = patch.default_variation {
            env.default_variation = v;
        }
        let env = eval::validate_env(&env, &flag.variations).map_err(invalid)?;
        let value = serde_json::to_value(&env).map_err(|e| Error::Invalid(e.to_string()))?;
        match flag.environments.as_object_mut() {
            Some(map) => {
                map.insert(environment.to_string(), value);
            }
            None => flag.environments = serde_json::json!({ environment: value }),
        }
        commit_change(self, flag, "set_environment", &meta).await
    }

    async fn set_archived(
        &mut self,
        app: &FlagApp,
        key: &str,
        archived: bool,
        meta: ChangeMeta,
    ) -> Result<FeatureFlag> {
        let mut flag = lock(self, app, key, meta.expected_version).await?;
        let target = if archived {
            FlagStatus::Archived
        } else {
            FlagStatus::Active
        };
        if flag.status == target {
            return Ok(flag);
        }
        flag.status = target;
        commit_change(
            self,
            flag,
            if archived { "archive" } else { "restore" },
            &meta,
        )
        .await
    }

    async fn rollback_flag(
        &mut self,
        app: &FlagApp,
        key: &str,
        to_version: Option<i32>,
        meta: ChangeMeta,
    ) -> Result<(FeatureFlag, i32)> {
        let mut flag = lock(self, app, key, meta.expected_version).await?;
        let target = to_version.unwrap_or(flag.version - 1);
        if target < 1 || target >= flag.version {
            return Err(Error::Invalid(format!(
                "flag {:?} is at version {}; roll back to a version from 1 to {}",
                flag.key,
                flag.version,
                flag.version - 1
            )));
        }
        let org = self.org();
        let snapshot: Option<Value> = sqlx::query_scalar(
            "SELECT snapshot FROM flag_versions WHERE org_id = $1 AND flag_id = $2 AND version = $3",
        )
        .bind(org)
        .bind(flag.id)
        .bind(target)
        .fetch_optional(self.conn())
        .await?;
        let snap = snapshot.ok_or_else(|| Error::VersionNotFound {
            key: flag.key.clone(),
            version: target,
        })?;
        let field = |name: &str| snap.get(name).cloned().unwrap_or(Value::Null);
        if let Some(name) = field("name").as_str() {
            flag.name = name.to_string();
        }
        flag.description = field("description").as_str().map(str::to_string);
        flag.status = serde_json::from_value(field("status")).unwrap_or(flag.status);
        flag.environments = field("environments");
        flag.variations = field("variations");
        flag.configuration = Some(field("configuration")).filter(|c| !c.is_null());
        let mut meta = meta;
        meta.reason = Some(match meta.reason.take() {
            Some(r) => format!("rollback to v{target}: {r}"),
            None => format!("rollback to v{target}"),
        });
        Ok((commit_change(self, flag, "rollback", &meta).await?, target))
    }

    async fn flag_history(&mut self, flag: &FeatureFlag, limit: i64) -> Result<Vec<FlagVersion>> {
        let org = self.org();
        Ok(sqlx::query_as(
            "SELECT version, change, snapshot, actor_user_id, reason, created_at \
             FROM flag_versions WHERE org_id = $1 AND flag_id = $2 \
             ORDER BY version DESC LIMIT $3",
        )
        .bind(org)
        .bind(flag.id)
        .bind(limit.clamp(1, 200))
        .fetch_all(self.conn())
        .await?)
    }
}

/// Fetch and lock a flag for writing, refusing if the caller's view is stale.
async fn lock(
    tx: &mut Tx<'_>,
    app: &FlagApp,
    key: &str,
    expected_version: Option<i32>,
) -> Result<FeatureFlag> {
    let org = tx.org();
    let flag: Option<FeatureFlag> = sqlx::query_as(&format!(
        "SELECT {COLS} FROM feature_flags WHERE org_id = $1 AND app_id = $2 AND key = $3 FOR UPDATE"
    ))
    .bind(org)
    .bind(app.id)
    .bind(key.trim())
    .fetch_optional(tx.conn())
    .await?;
    let flag = flag.ok_or_else(|| Error::FlagNotFound {
        app: app.name.clone(),
        key: key.trim().to_string(),
    })?;
    if let Some(expected) = expected_version {
        if expected != flag.version {
            return Err(Error::VersionConflict {
                key: flag.key,
                expected,
                actual: flag.version,
            });
        }
    }
    Ok(flag)
}

/// Write `flag`'s new state as the next version.
async fn commit_change(
    tx: &mut Tx<'_>,
    flag: FeatureFlag,
    change: &str,
    meta: &ChangeMeta,
) -> Result<FeatureFlag> {
    let reason = validate_text("reason", meta.reason.clone(), MAX_REASON)?;
    let org = tx.org();
    let updated: FeatureFlag = sqlx::query_as(&format!(
        "UPDATE feature_flags SET \
           name = $3, description = $4, status = $5, environments = $6, variations = $7, \
           configuration = $8, version = version + 1, updated_at = now(), \
           archived_at = CASE WHEN $5 = 'archived'::flag_status \
                              THEN COALESCE(archived_at, now()) ELSE NULL END \
         WHERE org_id = $1 AND id = $2 RETURNING {COLS}"
    ))
    .bind(org)
    .bind(flag.id)
    .bind(&flag.name)
    .bind(&flag.description)
    .bind(flag.status)
    .bind(&flag.environments)
    .bind(&flag.variations)
    .bind(&flag.configuration)
    .fetch_one(tx.conn())
    .await?;
    record_version(tx, &updated, change, meta.actor, reason.as_deref()).await?;
    let kind = if updated.status == FlagStatus::Archived {
        notify::Kind::Deleted
    } else {
        notify::Kind::Updated
    };
    notify::flag_changed(tx, &updated, kind).await?;
    Ok(updated)
}

async fn record_version(
    tx: &mut Tx<'_>,
    flag: &FeatureFlag,
    change: &str,
    actor: UserId,
    reason: Option<&str>,
) -> Result<()> {
    let org = tx.org();
    sqlx::query(
        "INSERT INTO flag_versions (org_id, flag_id, version, change, snapshot, actor_user_id, reason) \
         VALUES ($1,$2,$3,$4,$5,$6,$7)",
    )
    .bind(org)
    .bind(flag.id)
    .bind(flag.version)
    .bind(change)
    .bind(flag.snapshot())
    .bind(actor)
    .bind(reason)
    .execute(tx.conn())
    .await?;
    Ok(())
}

// ------------------------------------------------------------ the SDK's view

/// One flag as `GET /api/sdk/flags` lists it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SdkFlag {
    pub key: String,
    pub enabled: bool,
    pub scope: &'static str,
    pub environments: Value,
    pub variations: Option<Value>,
    pub configuration: Option<Value>,
    pub version: i32,
}

impl SdkFlag {
    /// `enabled` is the environment's master switch, not an evaluation: with no
    /// context there is nobody to evaluate for. SDKs that need per-user answers
    /// call the evaluate endpoint.
    pub fn from_flag(flag: &FeatureFlag, environment: &str) -> Self {
        let enabled = flag.status == FlagStatus::Active
            && flag
                .environments
                .get(environment)
                .and_then(|e| e.get("enabled"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
        let has_variations = flag.variations.as_object().is_some_and(|m| !m.is_empty());
        Self {
            key: flag.key.clone(),
            enabled,
            scope: "application",
            environments: flag.environments.clone(),
            variations: has_variations.then(|| flag.variations.clone()),
            configuration: flag.configuration.clone(),
            version: flag.version,
        }
    }
}
