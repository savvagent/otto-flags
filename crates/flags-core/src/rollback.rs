//! Auto-rollback: a per-flag, per-environment policy that turns a flag off
//! when the errors reported behind it say it is hurting the application.
//!
//! This part is pure: the thresholds a policy carries and [`decide`], the one
//! definition of "this flag is misbehaving". `flag_health`'s advice
//! ([`crate::telemetry`]'s `assess`) calls the same function with the default
//! thresholds, so what an agent is told and what the checker does cannot
//! disagree.

use std::future::Future;
use std::time::Duration;

use chrono::{DateTime, Utc};
use otto_tenant::ids::{OrgId, UserId};
use otto_tenant::{Db, Tx};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

use crate::apps::{AppsExt, FlagApp};
use crate::audit::action;
use crate::entitlements::{Entitlement, EntitlementSource, AUTO_ROLLBACK};
use crate::error::{Error, Result};
use crate::flags::{self, ChangeMeta, EnvPatch, FeatureFlag, FlagStatus};
use crate::ids::{FlagAppId, FlagId};
use crate::platform_events::begin_live;
use otto_tenant::audit::Entry;

/// When a policy trips. The defaults are the column defaults of
/// `rollback_policies` (a test pins that) and are what `flag_health` judges by.
#[derive(Debug, Clone, PartialEq, Serialize, FromRow, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Thresholds {
    /// Evaluations with the flag on needed before judging (>= 1).
    pub min_evaluations: i64,
    /// Errors with the flag on needed before judging (>= 1).
    pub min_errors: i64,
    /// Trip when the on error rate exceeds the off rate by more than this
    /// factor (> 1.0).
    pub max_error_ratio: f64,
    /// Trip when the on error rate exceeds this absolute rate (0 < r <= 1).
    /// The only trigger that works with no off traffic.
    pub max_error_rate: Option<f64>,
    /// How far back the sample reaches, in hours (1 to 24).
    pub window_hours: i32,
    /// Whether errors and evaluations reported by public client keys count.
    pub include_client_reports: bool,
}

impl Default for Thresholds {
    fn default() -> Self {
        Self {
            min_evaluations: 100,
            min_errors: 10,
            max_error_ratio: 2.0,
            max_error_rate: None,
            window_hours: 1,
            include_client_reports: false,
        }
    }
}

impl Thresholds {
    /// Check the values against the same limits as the table's CHECK
    /// constraints, with a message an agent can act on.
    ///
    /// ```
    /// use flags_core::rollback::Thresholds;
    /// assert!(Thresholds::default().validate().is_ok());
    /// let bad = Thresholds { window_hours: 25, ..Thresholds::default() };
    /// assert!(bad.validate().is_err());
    /// ```
    pub fn validate(&self) -> Result<()> {
        if self.min_evaluations < 1 {
            return Err(Error::Invalid(format!(
                "minEvaluations must be at least 1, got {}",
                self.min_evaluations
            )));
        }
        if self.min_errors < 1 {
            return Err(Error::Invalid(format!(
                "minErrors must be at least 1, got {}",
                self.min_errors
            )));
        }
        if !(self.max_error_ratio.is_finite() && self.max_error_ratio > 1.0) {
            return Err(Error::Invalid(format!(
                "maxErrorRatio must be greater than 1.0 (the factor by which the error rate \
                 with the flag on must exceed the rate with it off), got {}",
                self.max_error_ratio
            )));
        }
        if let Some(r) = self.max_error_rate {
            if !(r > 0.0 && r <= 1.0) {
                return Err(Error::Invalid(format!(
                    "maxErrorRate must be greater than 0 and at most 1 (errors per \
                     evaluation), or omitted, got {r}"
                )));
            }
        }
        if !(1..=24).contains(&self.window_hours) {
            return Err(Error::Invalid(format!(
                "windowHours must be between 1 and 24, got {}",
                self.window_hours
            )));
        }
        Ok(())
    }
}

/// Counts over one window, split by whether the flag was on or off.
#[derive(Debug, Clone, Default, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Sample {
    pub evals_on: i64,
    pub evals_off: i64,
    pub errors_on: i64,
    pub errors_off: i64,
}

impl Sample {
    /// Errors per evaluation with the flag on; `None` with no evaluations.
    pub fn rate_on(&self) -> Option<f64> {
        (self.evals_on > 0).then(|| self.errors_on as f64 / self.evals_on as f64)
    }

    /// Errors per evaluation with the flag off; `None` with no evaluations.
    pub fn rate_off(&self) -> Option<f64> {
        (self.evals_off > 0).then(|| self.errors_off as f64 / self.evals_off as f64)
    }
}

/// Which condition tripped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Trigger {
    /// The on error rate is above `max_error_rate`.
    MaxErrorRate,
    /// The on error rate is more than `max_error_ratio` times the off rate.
    ErrorRatio,
    /// The flag is erroring while on and the off side has traffic but no errors.
    OffHasNoErrors,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(tag = "decision", content = "trigger", rename_all = "camelCase")]
pub enum Decision {
    /// Not enough evaluations or errors with the flag on to judge.
    BelowMinimums,
    Healthy,
    Trip(Trigger),
}

/// The decision rule. Trips when the sample meets both minimums and any of:
/// the on rate exceeds `max_error_rate`; the off side has traffic and errors
/// and the on rate exceeds the off rate times `max_error_ratio` (strictly);
/// the off side has traffic but no errors.
///
/// ```
/// use flags_core::rollback::{decide, Decision, Sample, Thresholds, Trigger};
/// let s = Sample { evals_on: 1000, evals_off: 1000, errors_on: 50, errors_off: 5 };
/// assert_eq!(decide(&s, &Thresholds::default()), Decision::Trip(Trigger::ErrorRatio));
/// ```
pub fn decide(sample: &Sample, t: &Thresholds) -> Decision {
    if sample.evals_on < t.min_evaluations || sample.errors_on < t.min_errors {
        return Decision::BelowMinimums;
    }
    // evals_on >= min_evaluations >= 1 for valid thresholds; guard anyway so
    // invalid ones cannot panic or trip on a missing rate.
    let Some(on) = sample.rate_on() else {
        return Decision::BelowMinimums;
    };
    if t.max_error_rate.is_some_and(|max| on > max) {
        return Decision::Trip(Trigger::MaxErrorRate);
    }
    if let Some(off) = sample.rate_off() {
        if sample.errors_off == 0 {
            return Decision::Trip(Trigger::OffHasNoErrors);
        }
        if on > off * t.max_error_ratio {
            return Decision::Trip(Trigger::ErrorRatio);
        }
    }
    Decision::Healthy
}

/// False when `decide` can never trip on this sample's shape: no off traffic
/// and no `max_error_rate`, so there is nothing to compare against.
///
/// ```
/// use flags_core::rollback::{can_trip, Sample, Thresholds};
/// let at_full_rollout = Sample { evals_on: 5000, errors_on: 400, ..Sample::default() };
/// assert!(!can_trip(&at_full_rollout, &Thresholds::default()));
/// ```
pub fn can_trip(sample: &Sample, t: &Thresholds) -> bool {
    sample.evals_off > 0 || t.max_error_rate.is_some()
}

/// What a policy does when it trips.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type, schemars::JsonSchema,
)]
#[serde(rename_all = "lowercase")]
#[sqlx(type_name = "rollback_mode", rename_all = "lowercase")]
pub enum Mode {
    /// Turn the environment off.
    Enforce,
    /// Record what would have happened and change nothing.
    Observe,
}

/// A rollback policy for one flag in one environment.
#[derive(Debug, Clone, PartialEq, Serialize, FromRow, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Policy {
    pub id: Uuid,
    /// Not serialized: a caller already knows its own org.
    #[serde(skip)]
    pub org_id: OrgId,
    pub app_id: FlagAppId,
    pub flag_id: FlagId,
    pub environment: String,
    pub mode: Mode,
    #[sqlx(flatten)]
    #[serde(flatten)]
    pub thresholds: Thresholds,
    /// Set when the policy tripped; cleared when the environment is turned
    /// back on. A tripped policy does not act again.
    pub tripped_at: Option<DateTime<Utc>>,
    pub last_checked_at: Option<DateTime<Utc>>,
    pub created_by: Option<UserId>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum PolicyState {
    /// Watching.
    Armed,
    /// Acted (or would have); waiting for the environment to be turned back on.
    Tripped,
    /// The app no longer has this policy's environment; the policy cannot act.
    EnvironmentMissing,
}

/// A policy with the flag's key and where it stands.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PolicyView {
    #[serde(flatten)]
    pub policy: Policy,
    pub flag_key: String,
    pub state: PolicyState,
}

impl PolicyView {
    fn new(policy: Policy, flag_key: String, app_environments: &[String]) -> Self {
        // A missing environment wins over tripped: it says why the policy
        // cannot do anything, which a tripped flag no longer needs to know.
        let state = if !app_environments.contains(&policy.environment) {
            PolicyState::EnvironmentMissing
        } else if policy.tripped_at.is_some() {
            PolicyState::Tripped
        } else {
            PolicyState::Armed
        };
        Self {
            policy,
            flag_key,
            state,
        }
    }
}

/// What the checker did, or would have done, or could not do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    RolledBack,
    WouldRollBack,
    NotEntitled,
    EntitlementUnknown,
}

impl Outcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Outcome::RolledBack => "rolled_back",
            Outcome::WouldRollBack => "would_roll_back",
            Outcome::NotEntitled => "not_entitled",
            Outcome::EntitlementUnknown => "entitlement_unknown",
        }
    }
}

impl TryFrom<String> for Outcome {
    type Error = String;

    fn try_from(s: String) -> std::result::Result<Self, String> {
        Ok(match s.as_str() {
            "rolled_back" => Outcome::RolledBack,
            "would_roll_back" => Outcome::WouldRollBack,
            "not_entitled" => Outcome::NotEntitled,
            "entitlement_unknown" => Outcome::EntitlementUnknown,
            other => return Err(format!("unknown rollback outcome {other:?}")),
        })
    }
}

/// The numbers a decision was made on; stored as an event's `observed`.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Observation {
    pub window_start: DateTime<Utc>,
    pub sample: Sample,
    pub rate_on: Option<f64>,
    pub rate_off: Option<f64>,
    pub thresholds: Thresholds,
    pub decision: Decision,
}

/// One row of what policies did. Carries only numbers, ids, and enums: no
/// SDK-reported text.
#[derive(Debug, Clone, PartialEq, Serialize, FromRow, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub id: i64,
    pub policy_id: Uuid,
    pub flag_id: FlagId,
    pub flag_key: String,
    pub environment: String,
    #[sqlx(try_from = "String")]
    pub outcome: Outcome,
    pub observed: serde_json::Value,
    pub flag_version: Option<i32>,
    pub created_at: DateTime<Utc>,
}

#[derive(FromRow)]
struct PolicyRow {
    #[sqlx(flatten)]
    policy: Policy,
    flag_key: String,
}

const POLICY_COLS: &str = "p.id, p.org_id, p.app_id, p.flag_id, p.environment, p.mode, \
     p.min_evaluations, p.min_errors, p.max_error_ratio, p.max_error_rate, p.window_hours, \
     p.include_client_reports, p.tripped_at, p.last_checked_at, p.created_by, p.created_at, \
     p.updated_at";

const MAX_EVENTS: i64 = 200;

pub trait RollbackExt {
    /// Create the policy for `flag` in `environment`, or replace its mode and
    /// thresholds in place (id, creator, and `tripped_at` are kept: armed or
    /// tripped follows the environment's switch, not the thresholds).
    fn set_policy(
        &mut self,
        app: &FlagApp,
        flag: &FeatureFlag,
        environment: &str,
        mode: Mode,
        t: Thresholds,
        actor: UserId,
    ) -> impl Future<Output = Result<PolicyView>> + Send;

    /// Delete a policy; whether there was one.
    fn remove_policy(
        &mut self,
        flag: &FeatureFlag,
        environment: &str,
        actor: UserId,
    ) -> impl Future<Output = Result<bool>> + Send;

    /// The app's policies (or one flag's), in one query.
    fn list_policies(
        &mut self,
        app: &FlagApp,
        flag: Option<&FeatureFlag>,
    ) -> impl Future<Output = Result<Vec<PolicyView>>> + Send;

    fn find_policy(
        &mut self,
        flag: FlagId,
        environment: &str,
    ) -> impl Future<Output = Result<Option<Policy>>> + Send;

    /// Newest first; `limit` is clamped to 1..=200.
    fn rollback_events(
        &mut self,
        app: &FlagApp,
        flag: Option<&FeatureFlag>,
        limit: i64,
    ) -> impl Future<Output = Result<Vec<Event>>> + Send;

    /// The counts a policy with thresholds `t` judges: this hour so far plus
    /// the `window_hours - 1` before it. Returns the window's start.
    ///
    /// Unless `include_client_reports`, only server-key reports count
    /// (evaluations by `server_count`, errors with `reporter = 'server'`).
    /// Errors with no environment count toward `production`. Errors are
    /// windowed on `occurred_at`, as `flag_health` does.
    fn sample(
        &mut self,
        flag: FlagId,
        environment: &str,
        t: &Thresholds,
    ) -> impl Future<Output = Result<(DateTime<Utc>, Sample)>> + Send;
}

impl RollbackExt for Tx<'_> {
    async fn set_policy(
        &mut self,
        app: &FlagApp,
        flag: &FeatureFlag,
        environment: &str,
        mode: Mode,
        t: Thresholds,
        actor: UserId,
    ) -> Result<PolicyView> {
        if flag.app_id != app.id {
            return Err(Error::Invalid(format!(
                "flag '{}' does not belong to app '{}'",
                flag.key, app.name
            )));
        }
        if !app.environments.iter().any(|e| e == environment) {
            return Err(Error::Invalid(format!(
                "app '{}' has no environment '{environment}'; its environments are: {}",
                app.name,
                app.environments.join(", ")
            )));
        }
        t.validate()?;
        let org = self.org();
        let policy: Policy = sqlx::query_as(&format!(
            "INSERT INTO rollback_policies AS p \
               (org_id, app_id, flag_id, environment, mode, min_evaluations, min_errors, \
                max_error_ratio, max_error_rate, window_hours, include_client_reports, created_by) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12) \
             ON CONFLICT (flag_id, environment) DO UPDATE SET \
               mode = EXCLUDED.mode, \
               min_evaluations = EXCLUDED.min_evaluations, \
               min_errors = EXCLUDED.min_errors, \
               max_error_ratio = EXCLUDED.max_error_ratio, \
               max_error_rate = EXCLUDED.max_error_rate, \
               window_hours = EXCLUDED.window_hours, \
               include_client_reports = EXCLUDED.include_client_reports, \
               updated_at = now() \
             RETURNING {POLICY_COLS}"
        ))
        .bind(org)
        .bind(app.id)
        .bind(flag.id)
        .bind(environment)
        .bind(mode)
        .bind(t.min_evaluations)
        .bind(t.min_errors)
        .bind(t.max_error_ratio)
        .bind(t.max_error_rate)
        .bind(t.window_hours)
        .bind(t.include_client_reports)
        .bind(actor)
        .fetch_one(self.conn())
        .await?;
        self.audit(
            Entry::new(action::ROLLBACK_POLICY_SET)
                .actor(actor)
                .target("flag", flag.id.to_string())
                .detail(serde_json::json!({
                    "flag": flag.key,
                    "environment": environment,
                    "mode": mode,
                    "thresholds": t,
                })),
        )
        .await?;
        Ok(PolicyView::new(policy, flag.key.clone(), &app.environments))
    }

    async fn remove_policy(
        &mut self,
        flag: &FeatureFlag,
        environment: &str,
        actor: UserId,
    ) -> Result<bool> {
        let org = self.org();
        let removed: Option<Uuid> = sqlx::query_scalar(
            "DELETE FROM rollback_policies WHERE org_id = $1 AND flag_id = $2 AND environment = $3 \
             RETURNING id",
        )
        .bind(org)
        .bind(flag.id)
        .bind(environment)
        .fetch_optional(self.conn())
        .await?;
        if removed.is_none() {
            return Ok(false);
        }
        self.audit(
            Entry::new(action::ROLLBACK_POLICY_REMOVED)
                .actor(actor)
                .target("flag", flag.id.to_string())
                .detail(serde_json::json!({"flag": flag.key, "environment": environment})),
        )
        .await?;
        Ok(true)
    }

    async fn list_policies(
        &mut self,
        app: &FlagApp,
        flag: Option<&FeatureFlag>,
    ) -> Result<Vec<PolicyView>> {
        let org = self.org();
        let rows: Vec<PolicyRow> = sqlx::query_as(&format!(
            "SELECT {POLICY_COLS}, f.key AS flag_key \
             FROM rollback_policies p \
             JOIN feature_flags f ON f.org_id = p.org_id AND f.id = p.flag_id \
             WHERE p.org_id = $1 AND p.app_id = $2 AND ($3::uuid IS NULL OR p.flag_id = $3) \
             ORDER BY f.key, p.environment"
        ))
        .bind(org)
        .bind(app.id)
        .bind(flag.map(|f| f.id))
        .fetch_all(self.conn())
        .await?;
        Ok(rows
            .into_iter()
            .map(|r| PolicyView::new(r.policy, r.flag_key, &app.environments))
            .collect())
    }

    async fn find_policy(&mut self, flag: FlagId, environment: &str) -> Result<Option<Policy>> {
        let org = self.org();
        Ok(sqlx::query_as(&format!(
            "SELECT {POLICY_COLS} FROM rollback_policies p \
             WHERE p.org_id = $1 AND p.flag_id = $2 AND p.environment = $3"
        ))
        .bind(org)
        .bind(flag)
        .bind(environment)
        .fetch_optional(self.conn())
        .await?)
    }

    async fn rollback_events(
        &mut self,
        app: &FlagApp,
        flag: Option<&FeatureFlag>,
        limit: i64,
    ) -> Result<Vec<Event>> {
        let org = self.org();
        Ok(sqlx::query_as(
            "SELECT e.id, e.policy_id, e.flag_id, f.key AS flag_key, e.environment, e.outcome, \
                    e.observed, e.flag_version, e.created_at \
             FROM rollback_events e \
             JOIN feature_flags f ON f.org_id = e.org_id AND f.id = e.flag_id \
             WHERE e.org_id = $1 AND f.app_id = $2 AND ($3::uuid IS NULL OR e.flag_id = $3) \
             ORDER BY e.created_at DESC, e.id DESC \
             LIMIT $4",
        )
        .bind(org)
        .bind(app.id)
        .bind(flag.map(|f| f.id))
        .bind(limit.clamp(1, MAX_EVENTS))
        .fetch_all(self.conn())
        .await?)
    }

    async fn sample(
        &mut self,
        flag: FlagId,
        environment: &str,
        t: &Thresholds,
    ) -> Result<(DateTime<Utc>, Sample)> {
        let org = self.org();
        // The window start is computed once, in SQL, and reused by the second
        // query so both sides cover the same span.
        let (since, evals_on, evals_off): (DateTime<Utc>, i64, i64) = sqlx::query_as(
            "SELECT w.since, \
                    coalesce(sum(CASE WHEN $5 THEN e.count ELSE e.server_count END) \
                             FILTER (WHERE e.enabled), 0)::bigint, \
                    coalesce(sum(CASE WHEN $5 THEN e.count ELSE e.server_count END) \
                             FILTER (WHERE NOT e.enabled), 0)::bigint \
             FROM (SELECT date_trunc('hour', now()) - make_interval(hours => $4 - 1) AS since) w \
             LEFT JOIN flag_eval_hourly e \
               ON e.org_id = $1 AND e.flag_id = $2 AND e.environment = $3 AND e.hour >= w.since \
             GROUP BY w.since",
        )
        .bind(org)
        .bind(flag)
        .bind(environment)
        .bind(t.window_hours)
        .bind(t.include_client_reports)
        .fetch_one(self.conn())
        .await?;

        let (errors_on, errors_off): (i64, i64) = sqlx::query_as(
            "SELECT count(*) FILTER (WHERE flag_enabled)::bigint, \
                    count(*) FILTER (WHERE NOT flag_enabled)::bigint \
             FROM flag_errors \
             WHERE org_id = $1 AND flag_id = $2 AND occurred_at >= $4 \
               AND (environment = $3 OR ($3 = 'production' AND environment IS NULL)) \
               AND ($5 OR reporter = 'server')",
        )
        .bind(org)
        .bind(flag)
        .bind(environment)
        .bind(since)
        .bind(t.include_client_reports)
        .fetch_one(self.conn())
        .await?;

        Ok((
            since,
            Sample {
                evals_on,
                evals_off,
                errors_on,
                errors_off,
            },
        ))
    }
}

/// Of `flags`, those with an armed (untripped) policy, deduplicated. One
/// indexed SELECT, for use inside the telemetry transaction.
pub async fn armed(tx: &mut Tx<'_>, flags: &[FlagId]) -> Result<Vec<FlagId>> {
    if flags.is_empty() {
        return Ok(Vec::new());
    }
    let org = tx.org();
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT flag_id FROM rollback_policies \
         WHERE org_id = $1 AND flag_id = ANY($2) AND tripped_at IS NULL",
    )
    .bind(org)
    .bind(flags)
    .fetch_all(tx.conn())
    .await?)
}

/// Clear `tripped_at` on the policies of every environment that went from off
/// (or absent) to on between `before` and `after`, so a tripped policy guards
/// the environment again once someone turns it back on. Runs inside the
/// change's own transaction.
pub(crate) async fn rearm(
    tx: &mut Tx<'_>,
    flag: FlagId,
    before: &serde_json::Value,
    after: &serde_json::Value,
) -> Result<()> {
    let enabled = |envs: &serde_json::Value, name: &str| {
        envs.get(name)
            .and_then(|e| e.get("enabled"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    };
    let turned_on: Vec<String> = after
        .as_object()
        .into_iter()
        .flatten()
        .filter(|(name, _)| enabled(after, name) && !enabled(before, name))
        .map(|(name, _)| name.clone())
        .collect();
    if turned_on.is_empty() {
        return Ok(());
    }
    let org = tx.org();
    sqlx::query(
        "UPDATE rollback_policies SET tripped_at = NULL, updated_at = now() \
         WHERE org_id = $1 AND flag_id = $2 AND environment = ANY($3) AND tripped_at IS NOT NULL",
    )
    .bind(org)
    .bind(flag)
    .bind(&turned_on)
    .execute(tx.conn())
    .await?;
    Ok(())
}

// ------------------------------------------------------------ the checker

/// What [`check_flag`] did for one flag.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CheckReport {
    /// Armed policies the flag had when the check began.
    pub checked: usize,
    /// The outcome for each policy that still tripped once the entitlement was
    /// known, whether or not its (deduplicated) event was written this time.
    pub outcomes: Vec<(Uuid, Outcome)>,
}

/// Unpinned: armed policies whose `last_checked_at` is NULL or older than
/// `older_than`, least recently checked first, as distinct (org, flag) pairs.
/// `limit` bounds the policies read, so a flag with several due policies can
/// make the result shorter than `limit`.
///
/// The one statement the checker runs without pinning an org: the
/// `rollback_policies_checker` row-level policy allows exactly this SELECT
/// when `current_org()` is NULL. Acting on a pair goes through [`check_flag`],
/// which pins the org.
pub async fn due_policies(
    db: &Db,
    older_than: Duration,
    limit: i64,
) -> Result<Vec<(OrgId, FlagId)>> {
    // The inner scan walks rollback_policies_due_idx in order and stops at
    // `limit`; the outer query only folds a flag's environments together.
    Ok(sqlx::query_as(
        "SELECT org_id, flag_id FROM ( \
           SELECT org_id, flag_id, last_checked_at FROM rollback_policies \
           WHERE tripped_at IS NULL \
             AND (last_checked_at IS NULL \
                  OR last_checked_at < now() - make_interval(secs => $1)) \
           ORDER BY last_checked_at NULLS FIRST \
           LIMIT $2 \
         ) due \
         GROUP BY org_id, flag_id \
         ORDER BY bool_or(last_checked_at IS NULL) DESC, min(last_checked_at)",
    )
    .bind(older_than.as_secs_f64())
    .bind(limit.max(0))
    .fetch_all(db.pool())
    .await?)
}

/// Check every armed policy of one flag and act on the ones that trip
/// (spec §4, "Acting on a trip").
///
/// 1. A `begin_live` transaction (refused for a deleted org) reads the armed
///    policies and samples each. Policies that do not trip, or cannot act
///    (archived flag, environment no longer in the app), have
///    `last_checked_at` advanced; the transaction commits.
/// 2. If any tripped, the platform is asked whether the org's plan includes
///    auto-rollback, with no pooled connection held.
/// 3. A second `begin_live` transaction locks the flag, re-reads each tripped
///    policy, re-samples, and re-decides (the world may have moved during the
///    network call), then acts by outcome and advances `last_checked_at`.
///
/// Holds at most one pooled connection at a time. Writes are not metered: no
/// user called a tool.
pub async fn check_flag(
    db: &Db,
    entitlements: &impl EntitlementSource,
    org: OrgId,
    flag: FlagId,
) -> Result<CheckReport> {
    // Transaction 1: sample without locking anything.
    let mut tx = begin_live(db, org, None).await?;
    let policies = armed_policies(&mut tx, flag).await?;
    let mut report = CheckReport {
        checked: policies.len(),
        outcomes: Vec::new(),
    };
    if policies.is_empty() {
        tx.commit().await?;
        return Ok(report);
    }
    let Some((current, app)) = flag_and_app(&mut tx, flag).await? else {
        // The flag (and so its policies) is gone, or its app is.
        tx.commit().await?;
        return Ok(report);
    };
    let mut done = Vec::new();
    let mut tripped = Vec::new();
    for policy in policies {
        if can_act(&current, &app, &policy) {
            let (_, sample) = tx
                .sample(flag, &policy.environment, &policy.thresholds)
                .await?;
            if matches!(decide(&sample, &policy.thresholds), Decision::Trip(_)) {
                tripped.push(policy.id);
                continue;
            }
        }
        done.push(policy.id);
    }
    // Only policy rows are written here, never the flag row, so this cannot
    // deadlock with a change holding the flag lock.
    mark_checked(&mut tx, &done).await?;
    tx.commit().await?;
    if tripped.is_empty() {
        return Ok(report);
    }

    // Between transactions: the only network call, with no connection held.
    let entitlement = entitlements.check(org, AUTO_ROLLBACK).await;

    // Transaction 2: re-check under the flag's lock, then act.
    let mut tx = begin_live(db, org, None).await?;
    // Lock order: the flag row before any policy row. `commit_change` updates
    // rollback_policies (re-arming) while holding the flag lock, so touching a
    // policy first here could deadlock with a concurrent set_environment.
    let Some(mut current) = flags::lock_by_id(&mut tx, flag).await? else {
        tx.commit().await?;
        return Ok(report);
    };
    let Some(app) = tx.get_app(current.app_id).await? else {
        tx.commit().await?;
        return Ok(report);
    };
    let mut checked = Vec::new();
    for id in tripped {
        // Still there and still armed? Mode and thresholds are taken as they
        // are now: an edit during the network call wins.
        let Some(policy) = armed_policy(&mut tx, id).await? else {
            continue;
        };
        checked.push(policy.id);
        if !can_act(&current, &app, &policy) {
            continue;
        }
        let t = &policy.thresholds;
        let (window_start, sample) = tx.sample(flag, &policy.environment, t).await?;
        let decision = decide(&sample, t);
        if !matches!(decision, Decision::Trip(_)) {
            continue;
        }
        let observation = Observation {
            window_start,
            rate_on: sample.rate_on(),
            rate_off: sample.rate_off(),
            sample,
            thresholds: t.clone(),
            decision,
        };
        let outcome = match (&entitlement, policy.mode) {
            (Entitlement::Granted, Mode::Enforce) => {
                let version = if env_enabled(&current, &policy.environment) {
                    current = flags::apply_env_patch(
                        &mut tx,
                        &app,
                        &current.key,
                        &policy.environment,
                        EnvPatch {
                            enabled: Some(false),
                            ..Default::default()
                        },
                        &ChangeMeta {
                            actor: None,
                            reason: Some(rollback_reason(policy.id, &observation.sample)),
                            expected_version: None,
                        },
                        "auto_rollback",
                    )
                    .await?;
                    Some(current.version)
                } else {
                    None
                };
                mark_tripped(&mut tx, policy.id).await?;
                record_event(&mut tx, &policy, Outcome::RolledBack, &observation, version).await?;
                Outcome::RolledBack
            }
            (Entitlement::Granted, Mode::Observe) => Outcome::WouldRollBack,
            (Entitlement::Denied { .. }, _) => Outcome::NotEntitled,
            (Entitlement::Unknown, _) => Outcome::EntitlementUnknown,
        };
        if outcome != Outcome::RolledBack {
            record_suppressed(&mut tx, &policy, outcome, &observation).await?;
        }
        report.outcomes.push((policy.id, outcome));
    }
    mark_checked(&mut tx, &checked).await?;
    tx.commit().await?;
    Ok(report)
}

/// A policy can act only on an active flag, in an environment its app still has.
fn can_act(flag: &FeatureFlag, app: &FlagApp, policy: &Policy) -> bool {
    flag.status == FlagStatus::Active && app.environments.contains(&policy.environment)
}

fn env_enabled(flag: &FeatureFlag, environment: &str) -> bool {
    flag.environments
        .get(environment)
        .and_then(|e| e.get("enabled"))
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false)
}

/// The version's reason: numbers and an id only, never reported text.
fn rollback_reason(policy: Uuid, s: &Sample) -> String {
    let on = s.rate_on().unwrap_or(0.0) * 100.0;
    let off = s
        .rate_off()
        .map_or_else(|| "n/a".to_string(), |r| format!("{:.2}%", r * 100.0));
    let n = s.evals_on + s.evals_off;
    format!("auto-rollback by policy {policy}: error rate on {on:.2}% vs off {off} over {n} evaluations")
}

/// The flag (unlocked) and its app, for transaction 1.
async fn flag_and_app(tx: &mut Tx<'_>, flag: FlagId) -> Result<Option<(FeatureFlag, FlagApp)>> {
    let org = tx.org();
    let found: Option<FeatureFlag> = sqlx::query_as(&format!(
        "SELECT {} FROM feature_flags WHERE org_id = $1 AND id = $2",
        flags::COLS
    ))
    .bind(org)
    .bind(flag)
    .fetch_optional(tx.conn())
    .await?;
    let Some(found) = found else {
        return Ok(None);
    };
    Ok(tx.get_app(found.app_id).await?.map(|app| (found, app)))
}

async fn armed_policies(tx: &mut Tx<'_>, flag: FlagId) -> Result<Vec<Policy>> {
    let org = tx.org();
    Ok(sqlx::query_as(&format!(
        "SELECT {POLICY_COLS} FROM rollback_policies p \
         WHERE p.org_id = $1 AND p.flag_id = $2 AND p.tripped_at IS NULL \
         ORDER BY p.environment"
    ))
    .bind(org)
    .bind(flag)
    .fetch_all(tx.conn())
    .await?)
}

/// Re-read one policy, if it still exists and is armed. The caller holds the
/// flag's lock, and re-arming and tripping both happen under it, so the
/// answer cannot change before the transaction ends.
async fn armed_policy(tx: &mut Tx<'_>, id: Uuid) -> Result<Option<Policy>> {
    let org = tx.org();
    Ok(sqlx::query_as(&format!(
        "SELECT {POLICY_COLS} FROM rollback_policies p \
         WHERE p.org_id = $1 AND p.id = $2 AND p.tripped_at IS NULL"
    ))
    .bind(org)
    .bind(id)
    .fetch_optional(tx.conn())
    .await?)
}

async fn mark_checked(tx: &mut Tx<'_>, ids: &[Uuid]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let org = tx.org();
    sqlx::query(
        "UPDATE rollback_policies SET last_checked_at = now() WHERE org_id = $1 AND id = ANY($2)",
    )
    .bind(org)
    .bind(ids)
    .execute(tx.conn())
    .await?;
    Ok(())
}

async fn mark_tripped(tx: &mut Tx<'_>, id: Uuid) -> Result<()> {
    let org = tx.org();
    sqlx::query(
        "UPDATE rollback_policies SET tripped_at = now(), updated_at = now() \
         WHERE org_id = $1 AND id = $2",
    )
    .bind(org)
    .bind(id)
    .execute(tx.conn())
    .await?;
    Ok(())
}

/// Record a `rolled_back` event. Not deduplicated: a policy trips at most
/// once until it is re-armed.
async fn record_event(
    tx: &mut Tx<'_>,
    policy: &Policy,
    outcome: Outcome,
    observation: &Observation,
    flag_version: Option<i32>,
) -> Result<()> {
    insert_event(tx, policy, outcome, observation, flag_version, false).await
}

/// Record a suppressed outcome (`would_roll_back`, `not_entitled`,
/// `entitlement_unknown`) unless this policy recorded the same one in the last
/// hour, so a policy that keeps tripping without acting writes at most one row
/// per outcome per hour.
async fn record_suppressed(
    tx: &mut Tx<'_>,
    policy: &Policy,
    outcome: Outcome,
    observation: &Observation,
) -> Result<()> {
    insert_event(tx, policy, outcome, observation, None, true).await
}

async fn insert_event(
    tx: &mut Tx<'_>,
    policy: &Policy,
    outcome: Outcome,
    observation: &Observation,
    flag_version: Option<i32>,
    dedup: bool,
) -> Result<()> {
    let org = tx.org();
    let observed = serde_json::to_value(observation)
        .map_err(|e| Error::Invalid(format!("observation does not serialize: {e}")))?;
    // The dedup lookup is served by rollback_events_policy_idx.
    sqlx::query(
        "INSERT INTO rollback_events \
           (org_id, policy_id, flag_id, environment, outcome, observed, flag_version) \
         SELECT $1, $2, $3, $4, $5, $6, $7 \
         WHERE NOT $8 OR NOT EXISTS ( \
           SELECT 1 FROM rollback_events \
           WHERE org_id = $1 AND policy_id = $2 AND outcome = $5 \
             AND created_at > now() - interval '1 hour')",
    )
    .bind(org)
    .bind(policy.id)
    .bind(policy.flag_id)
    .bind(&policy.environment)
    .bind(outcome.as_str())
    .bind(observed)
    .bind(flag_version)
    .bind(dedup)
    .execute(tx.conn())
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::telemetry::{assess, Counts};

    fn s(evals_on: i64, evals_off: i64, errors_on: i64, errors_off: i64) -> Sample {
        Sample {
            evals_on,
            evals_off,
            errors_on,
            errors_off,
        }
    }

    fn d(sample: &Sample) -> Decision {
        decide(sample, &Thresholds::default())
    }

    #[test]
    fn the_rollback_reason_is_numbers_only() {
        let id = Uuid::nil();
        assert_eq!(
            rollback_reason(id, &s(200, 200, 30, 2)),
            format!(
                "auto-rollback by policy {id}: error rate on 15.00% vs off 1.00% \
                 over 400 evaluations"
            )
        );
        assert_eq!(
            rollback_reason(id, &s(1000, 0, 50, 0)),
            format!(
                "auto-rollback by policy {id}: error rate on 5.00% vs off n/a \
                 over 1000 evaluations"
            )
        );
    }

    #[test]
    fn outcome_text_round_trips_and_unknown_text_errs() {
        for o in [
            Outcome::RolledBack,
            Outcome::WouldRollBack,
            Outcome::NotEntitled,
            Outcome::EntitlementUnknown,
        ] {
            assert_eq!(Outcome::try_from(o.as_str().to_string()), Ok(o));
            assert_eq!(
                serde_json::to_value(o).unwrap(),
                serde_json::json!(o.as_str())
            );
        }
        assert!(Outcome::try_from("exploded".to_string()).is_err());
    }

    #[test]
    fn policy_views_serialize_flat_in_camel_case() {
        let now = Utc::now();
        let policy = Policy {
            id: Uuid::nil(),
            org_id: OrgId::new(),
            app_id: FlagAppId::new(),
            flag_id: FlagId::new(),
            environment: "production".into(),
            mode: Mode::Observe,
            thresholds: Thresholds::default(),
            tripped_at: None,
            last_checked_at: None,
            created_by: None,
            created_at: now,
            updated_at: now,
        };
        let v = serde_json::to_value(PolicyView {
            policy,
            flag_key: "f".into(),
            state: PolicyState::EnvironmentMissing,
        })
        .unwrap();
        let obj = v.as_object().unwrap();
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort();
        assert_eq!(
            keys,
            [
                "appId",
                "createdAt",
                "createdBy",
                "environment",
                "flagId",
                "flagKey",
                "id",
                "includeClientReports",
                "lastCheckedAt",
                "maxErrorRate",
                "maxErrorRatio",
                "minErrors",
                "minEvaluations",
                "mode",
                "state",
                "trippedAt",
                "updatedAt",
                "windowHours"
            ]
        );
        assert_eq!(v["mode"], "observe");
        assert_eq!(v["state"], "environmentMissing");
        assert_eq!(v["minEvaluations"], 100);
    }

    #[test]
    fn too_few_evaluations_is_below_minimums() {
        assert_eq!(d(&s(99, 1000, 99, 0)), Decision::BelowMinimums);
    }

    #[test]
    fn too_few_errors_is_below_minimums() {
        assert_eq!(d(&s(1000, 1000, 9, 0)), Decision::BelowMinimums);
    }

    #[test]
    fn exactly_the_minimums_is_enough_to_trip() {
        assert_eq!(
            d(&s(100, 1000, 10, 0)),
            Decision::Trip(Trigger::OffHasNoErrors)
        );
    }

    #[test]
    fn decisions_serialize_as_one_object_shape() {
        assert_eq!(
            serde_json::to_value(Decision::Healthy).unwrap(),
            serde_json::json!({"decision": "healthy"})
        );
        assert_eq!(
            serde_json::to_value(Decision::Trip(Trigger::ErrorRatio)).unwrap(),
            serde_json::json!({"decision": "trip", "trigger": "errorRatio"})
        );
    }

    #[test]
    fn validation_messages_use_the_wire_field_names() {
        let ok = Thresholds::default();
        for (bad, name) in [
            (
                Thresholds {
                    min_evaluations: 0,
                    ..ok.clone()
                },
                "minEvaluations",
            ),
            (
                Thresholds {
                    min_errors: 0,
                    ..ok.clone()
                },
                "minErrors",
            ),
            (
                Thresholds {
                    max_error_ratio: 1.0,
                    ..ok.clone()
                },
                "maxErrorRatio",
            ),
            (
                Thresholds {
                    max_error_rate: Some(0.0),
                    ..ok.clone()
                },
                "maxErrorRate",
            ),
            (
                Thresholds {
                    window_hours: 25,
                    ..ok.clone()
                },
                "windowHours",
            ),
        ] {
            let msg = bad.validate().unwrap_err().to_string();
            assert!(msg.contains(name), "{msg}");
        }
    }

    #[test]
    fn a_clear_ratio_trips() {
        assert_eq!(
            d(&s(1000, 1000, 50, 5)),
            Decision::Trip(Trigger::ErrorRatio)
        );
    }

    #[test]
    fn exactly_the_ratio_does_not_trip() {
        // 20/1000 = 2.0 x 10/1000: strictly greater is required.
        assert_eq!(d(&s(1000, 1000, 20, 10)), Decision::Healthy);
    }

    #[test]
    fn errors_only_with_the_flag_on_trip() {
        assert_eq!(
            d(&s(1000, 1000, 20, 0)),
            Decision::Trip(Trigger::OffHasNoErrors)
        );
    }

    #[test]
    fn no_off_traffic_cannot_trip_without_a_max_rate() {
        let sample = s(1000, 0, 500, 0);
        assert_eq!(d(&sample), Decision::Healthy);
        assert!(!can_trip(&sample, &Thresholds::default()));
    }

    #[test]
    fn a_max_rate_trips_without_off_traffic() {
        let t = Thresholds {
            max_error_rate: Some(0.01),
            ..Thresholds::default()
        };
        let sample = s(1000, 0, 50, 0);
        assert!(can_trip(&sample, &t));
        assert_eq!(decide(&sample, &t), Decision::Trip(Trigger::MaxErrorRate));
    }

    #[test]
    fn validate_refuses_values_the_table_would() {
        let ok = Thresholds::default();
        assert!(ok.validate().is_ok());
        for bad in [
            Thresholds {
                max_error_ratio: 1.0,
                ..ok.clone()
            },
            Thresholds {
                window_hours: 25,
                ..ok.clone()
            },
            Thresholds {
                window_hours: 0,
                ..ok.clone()
            },
            Thresholds {
                max_error_rate: Some(0.0),
                ..ok.clone()
            },
            Thresholds {
                max_error_rate: Some(1.5),
                ..ok.clone()
            },
            Thresholds {
                min_evaluations: 0,
                ..ok.clone()
            },
            Thresholds {
                min_errors: 0,
                ..ok.clone()
            },
        ] {
            assert!(matches!(bad.validate(), Err(Error::Invalid(_))), "{bad:?}");
        }
        let edge = Thresholds {
            max_error_rate: Some(1.0),
            window_hours: 24,
            ..ok
        };
        assert!(edge.validate().is_ok());
    }

    /// Wiring check: `assess` says "Consider rolling back" only when `decide`
    /// returns `Trip`, over these cases.
    #[test]
    fn assess_and_decide_agree() {
        let cases = [
            (1000, 1000, 50, 5),
            (10, 0, 0, 0),
            (1000, 1000, 9, 0),
            (150, 1000, 5, 0),
            (100, 1000, 10, 0),
            (99, 1000, 99, 0),
            (1000, 1000, 20, 10),
            (1000, 1000, 20, 0),
            (1000, 0, 500, 0),
            (0, 1000, 0, 50),
            (0, 0, 0, 0),
            (1000, 1000, 30, 10),
            (100, 1, 10, 1),
            (100, 1, 10, 0),
        ];
        for (evals_on, evals_off, errors_on, errors_off) in cases {
            let sample = s(evals_on, evals_off, errors_on, errors_off);
            let text = assess(
                &Counts {
                    enabled: evals_on,
                    disabled: evals_off,
                },
                &Counts {
                    enabled: errors_on,
                    disabled: errors_off,
                },
                sample.rate_on(),
                sample.rate_off(),
            );
            assert_eq!(
                text.contains("Consider rolling back"),
                matches!(d(&sample), Decision::Trip(_)),
                "{sample:?}: {text}"
            );
        }
    }
}
