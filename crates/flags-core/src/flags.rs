//! Feature flags and their history.
//!
//! Every change goes through one path, [`FlagsExt`]'s private `commit_change`:
//! bump the version (refusing if the caller's `expected_version` is stale),
//! write the new state, snapshot it into `flag_versions` with the actor and
//! reason, and queue a change notification for SDK streams. All in the caller's
//! transaction, so a failed call leaves no trace and a successful one leaves
//! exactly one version.
//!
//! History is the audit trail of flag changes, so no request path removes or
//! changes any of it (`otto_app` cannot update `flag_versions`, migration
//! 0006). Only the hourly background sweep ([`sweep_versions`]) deletes, and
//! only versions both older than [`VERSION_RETENTION_DAYS`] and outside their
//! flag's newest [`MIN_RETAINED_VERSIONS`] (#123). Writes can make an old
//! version eligible (100 newer ones push it out of the newest 100), but never
//! a recent one, and those writes are themselves on record for 90 days.
//!
//! Writes lock the flag row (`FOR NO KEY UPDATE`) before computing the new
//! state, so two agents changing different environments of one flag at once
//! both land: the second waits for the first and merges into its result
//! instead of overwriting it.
//!
//! `NO KEY` because no write changes the row's key (`id`): telemetry inserts
//! check their foreign key to the flag with `FOR KEY SHARE`, which `FOR
//! UPDATE` would block for as long as a write (or the rollback checker's
//! second transaction) holds the flag. Flag writers still serialize: `FOR NO
//! KEY UPDATE` conflicts with itself.

use otto_tenant::ids::{OrgId, UserId};
use otto_tenant::{Db, Tx};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use sqlx::FromRow;

use crate::apps::FlagApp;
use crate::error::{on_unique, Error, Result};
use crate::eval::{self, unstorable, EnvConfig};
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
    /// create, update, set_environment, archive, restore, rollback, or
    /// auto_rollback (a rollback policy disabled an environment; `actor_user_id`
    /// is `None` for it).
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
    /// `None` for a change the system makes itself (auto-rollback); the version
    /// then records no user and names the cause in `reason`.
    pub actor: Option<UserId>,
    pub reason: Option<String>,
    pub expected_version: Option<i32>,
}

const COLS: &str = "id, org_id, app_id, key, name, description, status, environments, \
                    variations, configuration, version, created_at, updated_at, archived_at";

/// Flag versions younger than this are never removed (#123).
///
/// `flag_versions` is the audit trail of flag changes (see [`crate::audit`]),
/// so retention is by age, not by count: however many writes follow a change,
/// it stays on record this long, and a caller cannot erase a change by making
/// more of them. 90 days is the retention of rollback events and evaluation
/// rollups (`telemetry::EVAL_RETENTION_DAYS`) and `flag_health`'s longest
/// window, so a change and the health data it can be judged by age out
/// together.
pub const VERSION_RETENTION_DAYS: i32 = 90;

/// A flag's newest versions, which are kept however old they are (#123), so
/// a flag changed rarely keeps its rollback depth and its last known-good
/// state. The current version is always among them.
pub const MIN_RETAINED_VERSIONS: i32 = 100;

/// Rows [`sweep_versions`] deletes per transaction, to keep each one short on
/// the small shared database.
const SWEEP_BATCH: i64 = 1_000;

/// The most a flag may hold, in bytes: its environments, variations, and
/// configuration as stored (Postgres's text form of each `jsonb`), checked on
/// every write that grows it (#123).
///
/// The SDK list and each stream event carry all three, so this bounds a
/// stream event, and with [`MAX_APP_FLAG_BYTES`] the list. It also bounds a
/// history snapshot. 128 KiB holds a flag with three maximal configurations
/// ([`eval::MAX_CONFIGURATION_BYTES`]) and room for its environments, or
/// thousands of rule values; real flags are a few hundred bytes. The stored text form puts a space after `:` and `,`, and
/// the JSON the server sends is never longer for any value the API accepts,
/// so what a client receives is at most this.
pub const MAX_FLAG_BYTES: i64 = 128 * 1024;

/// The most an app's active flags may hold together: each one's size (as for
/// [`MAX_FLAG_BYTES`]) plus [`FLAG_OVERHEAD_BYTES`] (#123). This is the bound
/// on `GET /api/sdk/flags`, which a public client key can request: the list
/// is at most this plus a fixed envelope. Archived flags do not count (the
/// list leaves them out); restoring one is a write that counts.
///
/// Not serialized with a lock: two concurrent writes to different flags of
/// one app can each pass against the other's old state. Each adds at most
/// `MAX_FLAG_BYTES + FLAG_OVERHEAD_BYTES`, one per open transaction, and once
/// an app is over, every growing write is refused, so the overshoot is
/// bounded by the pool size rather than prevented with a new lock in every
/// flag write (including the rollback checker's).
pub const MAX_APP_FLAG_BYTES: i64 = 1024 * 1024;

/// What the list adds per flag beyond its stored size: a key of up to 128
/// characters, `enabled`, `scope`, `version`, the field names, `null` for
/// empty variations and configuration, braces, and a comma, about 252 bytes
/// at most. It also caps an app at 4 096 active flags.
pub const FLAG_OVERHEAD_BYTES: i64 = 256;

/// A flag's size in SQL, over `feature_flags`' columns. One definition for
/// the stored flags; the new state is measured with the same expression over
/// bound parameters in [`check_size`].
const FLAG_BYTES: &str = "octet_length(environments::text) + octet_length(variations::text) \
                          + coalesce(octet_length(configuration::text), 0)";

/// Whether a write is held to the size budgets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SizeCheck {
    Enforce,
    /// An environment patch that only turns the environment off: the kill
    /// switch and every auto-rollback. Never refused for size: `false` is a
    /// byte longer than `true`, and the incident path must not fail on it.
    DisableOnly,
}

const MAX_DESCRIPTION: usize = 2_000;
const MAX_REASON: usize = 1_000;

/// Validates and trims a flag key. `.` and `..` are refused although their
/// characters are allowed: SDKs send the key as a URL path segment (`POST
/// /api/flags/{key}/evaluate`, `POST /api/evaluate/{key}`), where URL parsers
/// resolve them as dot segments and the request reaches another route (#105).
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
    if matches!(key, "." | "..") {
        return Err(Error::Invalid(format!(
            "flag key {key:?} is a URL dot segment, which SDKs cannot evaluate over REST; \
             choose another key"
        )));
    }
    Ok(key.to_string())
}

fn validate_text(what: &str, s: Option<String>, max: usize) -> Result<Option<String>> {
    match s.map(|s| s.trim().to_string()) {
        Some(s) if unstorable(&s) => Err(Error::Invalid(format!(
            "{what} must not contain NUL (U+0000)"
        ))),
        Some(s) if s.chars().count() > max => Err(Error::Invalid(format!(
            "{what} must be {max} characters or fewer"
        ))),
        Some(s) if s.is_empty() => Ok(None),
        other => Ok(other),
    }
}

/// Postgres `jsonb` cannot hold NUL, so configuration carrying it is refused
/// rather than failing the write's statement. Its size is capped at
/// [`eval::MAX_CONFIGURATION_BYTES`] (#115).
fn validate_configuration(c: &Value) -> Result<()> {
    if eval::contains_nul(c) {
        return Err(Error::Invalid(
            "configuration must not contain NUL (U+0000)".into(),
        ));
    }
    eval::check_configuration_size(|| "configuration".into(), c).map_err(invalid)
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
        if let Some(c) = &new.configuration {
            validate_configuration(c)?;
        }
        // Every environment the app knows starts present and off, so the
        // flag's state is explicit everywhere rather than "not configured".
        let environments: Map<String, Value> = app
            .environments
            .iter()
            .map(|e| (e.clone(), serde_json::json!({"enabled": false})))
            .collect();
        let environments = Value::Object(environments);
        check_size(
            self,
            app,
            None,
            &key,
            true,
            &environments,
            &variations,
            new.configuration.as_ref(),
            false,
        )
        .await?;
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
        .bind(&environments)
        .bind(&variations)
        .bind(&new.configuration)
        .bind(actor)
        .fetch_one(self.conn())
        .await
        .map_err(|e| on_unique(e, || Error::DuplicateFlagKey { key: key.clone() }))?;

        record_version(self, &flag, "create", Some(actor), reason.as_deref()).await?;
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
        if unstorable(key) {
            return Ok(None);
        }
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
            validate_configuration(&c)?;
            flag.configuration = (!c.is_null()).then_some(c);
        }
        commit_change(self, app, flag, "update", &meta, SizeCheck::Enforce).await
    }

    async fn set_environment(
        &mut self,
        app: &FlagApp,
        key: &str,
        environment: &str,
        patch: EnvPatch,
        meta: ChangeMeta,
    ) -> Result<FeatureFlag> {
        apply_env_patch(self, app, key, environment, patch, &meta, "set_environment").await
    }

    async fn set_archived(
        &mut self,
        app: &FlagApp,
        key: &str,
        archived: bool,
        meta: ChangeMeta,
    ) -> Result<FeatureFlag> {
        // Checked here too, not only in `commit_change`: a call that changes
        // nothing returns before it, and must not accept a reason a change
        // would refuse.
        validate_text("reason", meta.reason.clone(), MAX_REASON)?;
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
            app,
            flag,
            if archived { "archive" } else { "restore" },
            &meta,
            SizeCheck::Enforce,
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
        Ok((
            commit_change(self, app, flag, "rollback", &meta, SizeCheck::Enforce).await?,
            target,
        ))
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

/// `set_environment`'s body with the change name as a parameter, so the
/// rollback checker disables an environment through exactly the same code.
pub(crate) async fn apply_env_patch(
    tx: &mut Tx<'_>,
    app: &FlagApp,
    key: &str,
    environment: &str,
    patch: EnvPatch,
    meta: &ChangeMeta,
    change: &str,
) -> Result<FeatureFlag> {
    let environment = environment.trim();
    let check = if patch.enabled == Some(false)
        && patch.rollout_percentage.is_none()
        && patch.rules.is_none()
        && patch.default_variation.is_none()
    {
        SizeCheck::DisableOnly
    } else {
        SizeCheck::Enforce
    };
    if !app.environments.iter().any(|e| e == environment) {
        return Err(Error::Invalid(format!(
            "app {:?} has no environment {environment:?}; its environments are: {}. \
             Add one with update_app",
            app.name,
            app.environments.join(", ")
        )));
    }
    let mut flag = lock(tx, app, key, meta.expected_version).await?;
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
    commit_change(tx, app, flag, change, meta, check).await
}

/// Read a flag by id without locking it (the rollback checker's first,
/// read-only transaction). `None` if it no longer exists.
pub(crate) async fn get_by_id(tx: &mut Tx<'_>, flag: FlagId) -> Result<Option<FeatureFlag>> {
    let org = tx.org();
    Ok(sqlx::query_as(&format!(
        "SELECT {COLS} FROM feature_flags WHERE org_id = $1 AND id = $2"
    ))
    .bind(org)
    .bind(flag)
    .fetch_optional(tx.conn())
    .await?)
}

/// Lock a flag by id (the rollback checker has no app/key in hand). `None` if
/// it no longer exists. `FOR NO KEY UPDATE`, as in `lock` (see the module
/// docs): telemetry for the flag is not blocked while the lock is held.
pub(crate) async fn lock_by_id(tx: &mut Tx<'_>, flag: FlagId) -> Result<Option<FeatureFlag>> {
    let org = tx.org();
    Ok(sqlx::query_as(&format!(
        "SELECT {COLS} FROM feature_flags WHERE org_id = $1 AND id = $2 FOR NO KEY UPDATE"
    ))
    .bind(org)
    .bind(flag)
    .fetch_optional(tx.conn())
    .await?)
}

/// Fetch and lock a flag for writing, refusing if the caller's view is stale.
/// `FOR NO KEY UPDATE`: it serializes flag writers without blocking the
/// `FOR KEY SHARE` that telemetry inserts take through their foreign key.
async fn lock(
    tx: &mut Tx<'_>,
    app: &FlagApp,
    key: &str,
    expected_version: Option<i32>,
) -> Result<FeatureFlag> {
    let org = tx.org();
    let flag: Option<FeatureFlag> = if unstorable(key) {
        None
    } else {
        sqlx::query_as(&format!(
            "SELECT {COLS} FROM feature_flags WHERE org_id = $1 AND app_id = $2 AND key = $3 \
             FOR NO KEY UPDATE"
        ))
        .bind(org)
        .bind(app.id)
        .bind(key.trim())
        .fetch_optional(tx.conn())
        .await?
    };
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

/// Refuse a write that grows a flag past [`MAX_FLAG_BYTES`] or its app's
/// active flags past [`MAX_APP_FLAG_BYTES`] (#123). Runs before the write, so a
/// refusal leaves the transaction as it was. `flag` is `None` for a new flag;
/// otherwise the caller holds its row lock, so its stored size is the state
/// being replaced. A write that does not grow past a budget always lands, so
/// state written before the limits (or by a race) never blocks shrinking it.
#[allow(clippy::too_many_arguments)]
async fn check_size(
    tx: &mut Tx<'_>,
    app: &FlagApp,
    flag: Option<FlagId>,
    key: &str,
    active: bool,
    environments: &Value,
    variations: &Value,
    configuration: Option<&Value>,
    rollback: bool,
) -> Result<()> {
    let org = tx.org();
    let (new, old, was_active, others): (i64, Option<i64>, Option<bool>, i64) =
        sqlx::query_as(&format!(
            "SELECT (octet_length($3::jsonb::text) + octet_length($4::jsonb::text) \
                     + coalesce(octet_length($5::jsonb::text), 0))::bigint, \
                    (SELECT ({FLAG_BYTES})::bigint FROM feature_flags \
                      WHERE org_id = $1 AND id = $6), \
                    (SELECT status = 'active' FROM feature_flags \
                      WHERE org_id = $1 AND id = $6), \
                    (SELECT coalesce(sum({FLAG_BYTES} + $7), 0)::bigint FROM feature_flags \
                      WHERE org_id = $1 AND app_id = $2 AND status = 'active' \
                        AND id IS DISTINCT FROM $6)"
        ))
        .bind(org)
        .bind(app.id)
        .bind(environments)
        .bind(variations)
        .bind(configuration)
        .bind(flag)
        .bind(FLAG_OVERHEAD_BYTES)
        .fetch_one(tx.conn())
        .await?;
    let old_size = old.unwrap_or(0);
    if new > MAX_FLAG_BYTES && new > old_size {
        return Err(Error::Invalid(format!(
            "flag {key:?} would be {new} bytes (its environments, variations and \
             configurations as stored); the limit is {MAX_FLAG_BYTES} bytes. Remove rules, \
             variations, or configuration"
        )));
    }
    let share = |size: i64| size + FLAG_OVERHEAD_BYTES;
    let new_total = others + if active { share(new) } else { 0 };
    let old_total = others
        + if was_active == Some(true) {
            share(old_size)
        } else {
            0
        };
    if new_total > MAX_APP_FLAG_BYTES && new_total > old_total {
        let hint = if rollback {
            ". To stop a bad change now, turn the environment off with set_flag_environment \
             enabled=false"
        } else {
            ""
        };
        return Err(Error::Invalid(format!(
            "app {:?}'s active flags would total {new_total} bytes; the limit is \
             {MAX_APP_FLAG_BYTES} bytes (each flag's environments, variations and \
             configurations as stored, plus {FLAG_OVERHEAD_BYTES}). Archive flags you no \
             longer need, or shrink their configuration{hint}",
            app.name
        )));
    }
    Ok(())
}

/// Write `flag`'s new state as the next version.
///
/// Precondition: the caller holds the flag's row lock (`lock` or
/// `lock_by_id`, `FOR NO KEY UPDATE`) in `tx`. The before-read of `environments`
/// that re-arming compares against is only the state being replaced if no
/// one else can change the row in between.
async fn commit_change(
    tx: &mut Tx<'_>,
    app: &FlagApp,
    flag: FeatureFlag,
    change: &str,
    meta: &ChangeMeta,
    check: SizeCheck,
) -> Result<FeatureFlag> {
    let reason = validate_text("reason", meta.reason.clone(), MAX_REASON)?;
    if check == SizeCheck::Enforce {
        check_size(
            tx,
            app,
            Some(flag.id),
            &flag.key,
            flag.status == FlagStatus::Active,
            &flag.environments,
            &flag.variations,
            flag.configuration.as_ref(),
            change == "rollback",
        )
        .await?;
    }
    let org = tx.org();
    // The caller holds the row lock, so this is the state being replaced.
    let before: Value =
        sqlx::query_scalar("SELECT environments FROM feature_flags WHERE org_id = $1 AND id = $2")
            .bind(org)
            .bind(flag.id)
            .fetch_one(tx.conn())
            .await?;
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
    crate::rollback::rearm(tx, updated.id, &before, &updated.environments).await?;
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
    actor: Option<UserId>,
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

/// Retention: delete flag versions older than [`VERSION_RETENTION_DAYS`] that
/// are not among their flag's newest [`MIN_RETAINED_VERSIONS`], across every
/// org. Background code only (`flags-server`'s hourly sweep): each batch of at
/// most [`SWEEP_BATCH`] rows runs in its own maintenance transaction
/// ([`crate::maintenance`]), which migration 0006's policies require. Returns
/// the number of rows deleted.
///
/// A flag's newest version is its current one (every change records one), so
/// the newest 100 are measured from `max(version)` without reading
/// `feature_flags`. That maximum is a window over one scan of the table, not
/// a subquery per row: row-level security hides the table's statistics from
/// the planner (it estimates one row), and a correlated `max` became a nested
/// loop over every pair of versions. Batches continue from the last one's
/// highest `id`, so each batch deletes new rows; each one rescans the table,
/// which costs little at hourly cadence, where one batch is the norm.
pub async fn sweep_versions(db: &Db) -> Result<u64> {
    let mut swept = 0;
    let mut after: i64 = 0;
    loop {
        let mut tx = crate::maintenance::begin(db).await?;
        let ids: Vec<i64> = sqlx::query_scalar(
            "DELETE FROM flag_versions WHERE id IN ( \
               SELECT id FROM ( \
                 SELECT id, created_at, version, \
                        max(version) OVER (PARTITION BY flag_id) AS newest \
                   FROM flag_versions) v \
                WHERE id > $4 \
                  AND created_at < now() - make_interval(days => $1) \
                  AND version <= newest - $2 \
                ORDER BY id LIMIT $3) \
             RETURNING id",
        )
        .bind(VERSION_RETENTION_DAYS)
        .bind(MIN_RETAINED_VERSIONS)
        .bind(SWEEP_BATCH)
        .bind(after)
        .fetch_all(&mut *tx)
        .await?;
        tx.commit().await?;
        swept += ids.len() as u64;
        match ids.iter().max() {
            Some(&last) if ids.len() as i64 == SWEEP_BATCH => after = last,
            _ => return Ok(swept),
        }
    }
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
    ///
    /// `with_rules` is false for a client key. That key is public (it ships in
    /// web pages), and targeting rules routinely name people -- user ids, email
    /// addresses, customer accounts -- so a client key sees each environment's
    /// switch, rollout, and default variation, never its rules. Evaluation
    /// still applies them server-side.
    pub fn from_flag(flag: &FeatureFlag, environment: &str, with_rules: bool) -> Self {
        let enabled = flag.status == FlagStatus::Active
            && flag
                .environments
                .get(environment)
                .and_then(|e| e.get("enabled"))
                .and_then(Value::as_bool)
                .unwrap_or(false);
        let has_variations = flag.variations.as_object().is_some_and(|m| !m.is_empty());
        let mut environments = flag.environments.clone();
        if !with_rules {
            if let Some(map) = environments.as_object_mut() {
                for env in map.values_mut() {
                    if let Some(env) = env.as_object_mut() {
                        env.remove("rules");
                    }
                }
            }
        }
        Self {
            key: flag.key.clone(),
            enabled,
            scope: "application",
            environments,
            variations: has_variations.then(|| flag.variations.clone()),
            configuration: flag.configuration.clone(),
            version: flag.version,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_key_never_sees_targeting_rules() {
        let now = chrono::Utc::now();
        let flag = FeatureFlag {
            id: FlagId::new(),
            org_id: OrgId::new(),
            app_id: FlagAppId::new(),
            key: "beta".into(),
            name: "beta".into(),
            description: None,
            status: FlagStatus::Active,
            environments: serde_json::json!({"production": {
                "enabled": true,
                "rollout_percentage": 10,
                "rules": [{"attribute": "email", "operator": "in", "values": ["ceo@example.com"]}]
            }}),
            variations: serde_json::json!({}),
            configuration: None,
            version: 3,
            created_at: now,
            updated_at: now,
            archived_at: None,
        };
        let public = SdkFlag::from_flag(&flag, "production", false);
        assert!(public.enabled);
        assert!(!public.environments.to_string().contains("ceo@example.com"));
        assert_eq!(public.environments["production"]["rollout_percentage"], 10);
        let private = SdkFlag::from_flag(&flag, "production", true);
        assert!(private.environments.to_string().contains("ceo@example.com"));
    }

    /// The per-flag overhead in [`MAX_APP_FLAG_BYTES`] covers every field the
    /// list adds to a flag's stored size, at their largest (#123).
    #[test]
    fn the_list_overhead_covers_an_empty_flag() {
        let flag = SdkFlag {
            key: "k".repeat(128),
            enabled: false,
            scope: "application",
            environments: serde_json::json!({}),
            variations: None,
            configuration: None,
            version: i32::MAX,
        };
        let listed = serde_json::to_vec(&flag).unwrap().len() + 1; // and a comma
                                                                   // `{}` is counted in the stored size, not the overhead.
        assert!(
            (listed - 2) as i64 <= FLAG_OVERHEAD_BYTES,
            "{listed} bytes: {}",
            serde_json::to_string(&flag).unwrap()
        );
    }

    #[test]
    fn dot_segment_keys_are_refused() {
        for key in [".", "..", " .. "] {
            match validate_key(key) {
                Err(Error::Invalid(msg)) => assert!(msg.contains("dot segment"), "{key:?}: {msg}"),
                other => panic!("{key:?} was not refused: {other:?}"),
            }
        }
        for key in ["...", "a.b", ".a", "a."] {
            assert_eq!(validate_key(key).unwrap(), key);
        }
    }
}
