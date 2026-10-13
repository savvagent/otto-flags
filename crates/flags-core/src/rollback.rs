//! Auto-rollback: a per-flag, per-environment policy that turns a flag off
//! when the errors reported behind it say it is hurting the application.
//!
//! The decision is pure: the thresholds a policy carries and [`decide`], the one
//! definition of "this flag is misbehaving". `flag_health`'s advice
//! ([`crate::telemetry`]'s `assess`) calls the same function with the default
//! thresholds, so what an agent is told and what the checker does cannot
//! disagree.
//!
//! Comparing the error rate with the flag on against the rate with it off
//! needs evidence on both sides: the comparative triggers apply only when the
//! off side has at least `min_evaluations` evaluations too. Near 100% rollout
//! the few evaluations left off would show no errors by chance, so there only
//! `max_error_rate` can trip.
//!
//! ## When the checker runs
//!
//! The Fly machine suspends when idle, and a suspended process runs no
//! timers, so a timer alone would check nothing while the app is quiet and
//! then everything at once on wake. What wakes the machine is SDK telemetry,
//! so telemetry drives the checks: after its transaction commits, a telemetry
//! handler sends the org and its armed flags to the [`Checker`] through
//! [`Hints::send`]. The send never blocks and never touches the platform; a
//! full channel drops the hint and counts it (logged once a minute).
//!
//! Hints are debounced per flag: a flag is checked at most once per
//! [`CheckerConfig::debounce`] (30 s). A hint for a flag checked more recently
//! than that is deferred to the end of the interval (the trailing edge), never
//! dropped, so the batch that crossed a threshold is checked within 30 s.
//!
//! While the process is awake, a backstop pass every
//! [`CheckerConfig::backstop_every`] (5 min) asks [`due_policies`] for
//! policies not checked that long and hints each one through the same
//! debounce. It covers dropped hints and policies created after their
//! telemetry arrived.
//!
//! A flag whose check fails never advances `last_checked_at`, so the backstop
//! would keep returning it. A failure that retrying cannot fix (an org
//! deleted but not yet purged, a stored environment the patch refuses, a
//! constraint violation) leaves the flag alone for [`FAILURE_COOLDOWN`]; a
//! successful check clears that. A transient failure ([`Error::is_transient`]:
//! a pool timeout, a serialization failure or deadlock, a lock or statement
//! timeout, the platform unreachable) counts as a check, so the next hint or
//! backstop pass retries it after the usual debounce: a telemetry burst, the
//! likeliest time for a pool timeout, is also when a misbehaving flag most
//! needs checking. If at least `backstop_limit` policies fail persistently
//! they can still fill the backstop's slots; that is accepted, since hints
//! still reach every other flag.
//!
//! Every check is bounded: its transactions wait at most 2 s for a row lock,
//! and the whole check at most [`CheckerConfig::check_timeout`] (15 s), after
//! which it is abandoned and retried. Within one check each policy acts inside
//! its own savepoint, so a policy that fails changes nothing, is reported in
//! [`CheckReport::failed`], and does not undo the others; the check is then
//! judged by its worst failure.
//!
//! Flags are checked one at a time, and [`check_flag`] holds at most one
//! pooled connection at a time, so the checker never holds more than one of
//! the pool's connections.

use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

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
use crate::keys::KeyKind;
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

/// The decision rule. Judges only a sample with at least `min_evaluations`
/// evaluations and `min_errors` errors with the flag on, and then trips on
/// any of:
///
/// - the on rate exceeds `max_error_rate`, when one is set;
/// - with at least `min_evaluations` evaluations with the flag off (the same
///   minimum, so both sides of the comparison carry as much evidence): the
///   off side has errors and the on rate exceeds the off rate times
///   `max_error_ratio` (strictly), or the off side has no errors at all.
///
/// With less off traffic than that (a rollout near 100%, where the few
/// evaluations left off would show no errors by chance), only
/// `max_error_rate` can trip.
///
/// ```
/// use flags_core::rollback::{decide, Decision, Sample, Thresholds, Trigger};
/// let s = Sample { evals_on: 1000, evals_off: 1000, errors_on: 50, errors_off: 5 };
/// assert_eq!(decide(&s, &Thresholds::default()), Decision::Trip(Trigger::ErrorRatio));
/// // At 99%, the 99 evaluations left off are too few to compare against.
/// let s = Sample { evals_on: 10_000, evals_off: 99, errors_on: 10, errors_off: 0 };
/// assert_eq!(decide(&s, &Thresholds::default()), Decision::Healthy);
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
    if enough_off_traffic(sample, t) {
        if let Some(off) = sample.rate_off() {
            if sample.errors_off == 0 {
                return Decision::Trip(Trigger::OffHasNoErrors);
            }
            if on > off * t.max_error_ratio {
                return Decision::Trip(Trigger::ErrorRatio);
            }
        }
    }
    Decision::Healthy
}

/// Whether the off side has enough evaluations for the comparative triggers.
fn enough_off_traffic(sample: &Sample, t: &Thresholds) -> bool {
    // max(1): with invalid thresholds, no off traffic is still not enough.
    sample.evals_off >= t.min_evaluations.max(1)
}

/// False when `decide` can never trip on this sample's shape: fewer than
/// `min_evaluations` evaluations with the flag off and no `max_error_rate`,
/// so there is nothing to compare against.
///
/// ```
/// use flags_core::rollback::{can_trip, Sample, Thresholds};
/// let at_full_rollout = Sample { evals_on: 5000, errors_on: 400, ..Sample::default() };
/// assert!(!can_trip(&at_full_rollout, &Thresholds::default()));
/// let a_sliver_off = Sample { evals_off: 99, ..at_full_rollout };
/// assert!(!can_trip(&a_sliver_off, &Thresholds::default()));
/// ```
pub fn can_trip(sample: &Sample, t: &Thresholds) -> bool {
    enough_off_traffic(sample, t) || t.max_error_rate.is_some()
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
    /// Flattened: `"decision": "trip", "trigger": "errorRatio"` sit beside
    /// the numbers rather than in a nested object.
    #[serde(flatten)]
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

/// Of `flags`, those with an armed (untripped) policy that counts telemetry
/// from `reporter` (every policy counts a server key; only
/// `include_client_reports` policies count a client key), deduplicated. One
/// indexed SELECT, for use inside the telemetry transaction, so public-key
/// telemetry cannot wake the checker for policies that ignore it.
pub async fn armed(tx: &mut Tx<'_>, flags: &[FlagId], reporter: KeyKind) -> Result<Vec<FlagId>> {
    if flags.is_empty() {
        return Ok(Vec::new());
    }
    let org = tx.org();
    Ok(sqlx::query_scalar(
        "SELECT DISTINCT flag_id FROM rollback_policies \
         WHERE org_id = $1 AND flag_id = ANY($2) AND tripped_at IS NULL \
           AND (include_client_reports OR $3 = 'server')",
    )
    .bind(org)
    .bind(flags)
    .bind(reporter.as_str())
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
    /// Policies whose action failed. Each was rolled back to a savepoint, so
    /// it changed nothing, and its `last_checked_at` was not advanced; the
    /// other policies' actions stand.
    pub failed: Vec<PolicyFailure>,
}

/// One policy whose action failed during a check.
#[derive(Debug, Clone, PartialEq)]
pub struct PolicyFailure {
    pub policy: Uuid,
    /// [`Error::code`]; the error's text is not kept.
    pub code: &'static str,
    /// [`Error::is_transient`].
    pub transient: bool,
}

/// How long the checker's transactions wait for a row lock (a flag held by a
/// person's change, another checker) before failing with a transient error.
const CHECK_LOCK_TIMEOUT: &str = "2s";

/// Open a checker transaction: `begin_live`, with a bounded lock wait.
async fn begin_check(db: &Db, org: OrgId) -> Result<Tx<'static>> {
    let mut tx = begin_live(db, org, None).await?;
    sqlx::query(&format!("SET LOCAL lock_timeout = '{CHECK_LOCK_TIMEOUT}'"))
        .execute(tx.conn())
        .await?;
    Ok(tx)
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
    let mut tx = begin_check(db, org).await?;
    let policies = armed_policies(&mut tx, flag).await?;
    let mut report = CheckReport {
        checked: policies.len(),
        ..CheckReport::default()
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
    // No flag lock is held here, so take only policy rows nobody else holds.
    mark_checked_unlocked(&mut tx, &done).await?;
    tx.commit().await?;
    if tripped.is_empty() {
        return Ok(report);
    }

    // Between transactions: the only network call, with no connection held.
    let entitlement = entitlements.check(org, AUTO_ROLLBACK).await;

    // Transaction 2: re-check under the flag's lock, then act.
    let mut tx = begin_check(db, org).await?;
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
        let Decision::Trip(trigger) = decision else {
            continue;
        };
        let observation = Observation {
            window_start,
            rate_on: sample.rate_on(),
            rate_off: sample.rate_off(),
            sample,
            thresholds: t.clone(),
            decision,
        };
        let outcome = match (&entitlement, policy.mode) {
            (Entitlement::Granted, Mode::Enforce) => Outcome::RolledBack,
            (Entitlement::Granted, Mode::Observe) => Outcome::WouldRollBack,
            (Entitlement::Denied { .. }, _) => Outcome::NotEntitled,
            (Entitlement::Unknown, _) => Outcome::EntitlementUnknown,
        };
        // Each policy acts inside its own savepoint, so one that fails (a
        // stored environment the patch refuses, say) leaves no partial change
        // and does not discard what the others did in this transaction.
        sqlx::query("SAVEPOINT rollback_policy")
            .execute(tx.conn())
            .await?;
        let before = current.clone();
        let acted = match outcome {
            Outcome::RolledBack => {
                enforce(&mut tx, &app, &mut current, &policy, trigger, &observation).await
            }
            _ => record_suppressed(&mut tx, &policy, outcome, &observation).await,
        };
        match acted {
            Ok(()) => {
                sqlx::query("RELEASE SAVEPOINT rollback_policy")
                    .execute(tx.conn())
                    .await?;
                report.outcomes.push((policy.id, outcome));
            }
            Err(e) => {
                sqlx::query("ROLLBACK TO SAVEPOINT rollback_policy")
                    .execute(tx.conn())
                    .await?;
                // The flag as it was before this policy's undone patch.
                current = before;
                checked.retain(|id| *id != policy.id);
                report.failed.push(PolicyFailure {
                    policy: policy.id,
                    code: e.code(),
                    transient: e.is_transient(),
                });
                // The code only: database text never reaches a log above debug.
                tracing::warn!(%org, %flag, policy = %policy.id, code = e.code(),
                    "auto-rollback could not act on a policy");
                tracing::debug!(%org, %flag, policy = %policy.id, error = %e,
                    "auto-rollback policy failure");
            }
        }
    }
    mark_checked(&mut tx, &checked).await?;
    tx.commit().await?;
    Ok(report)
}

/// Act on an enforcing policy that tripped with the entitlement granted: turn
/// its environment off as an `auto_rollback` version (unless it already is
/// off), mark the policy tripped, and record `rolled_back` with the version
/// (NULL when there was nothing to turn off). `flag` is the locked flag and is
/// replaced by the new version.
async fn enforce(
    tx: &mut Tx<'_>,
    app: &FlagApp,
    flag: &mut FeatureFlag,
    policy: &Policy,
    trigger: Trigger,
    observation: &Observation,
) -> Result<()> {
    let version = if env_enabled(flag, &policy.environment) {
        *flag = flags::apply_env_patch(
            tx,
            app,
            &flag.key,
            &policy.environment,
            EnvPatch {
                enabled: Some(false),
                ..Default::default()
            },
            &ChangeMeta {
                actor: None,
                reason: Some(rollback_reason(policy.id, trigger, &observation.sample)),
                expected_version: None,
            },
            "auto_rollback",
        )
        .await?;
        Some(flag.version)
    } else {
        None
    };
    mark_tripped(tx, policy.id).await?;
    record_event(tx, policy, Outcome::RolledBack, observation, version).await
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

/// A trigger's wire name (camelCase, as `Trigger` serializes).
fn trigger_name(t: Trigger) -> &'static str {
    match t {
        Trigger::MaxErrorRate => "maxErrorRate",
        Trigger::ErrorRatio => "errorRatio",
        Trigger::OffHasNoErrors => "offHasNoErrors",
    }
}

/// The version's reason: an id, a trigger name, and numbers only, never
/// reported text.
fn rollback_reason(policy: Uuid, trigger: Trigger, s: &Sample) -> String {
    let on = s.rate_on().unwrap_or(0.0) * 100.0;
    let off = s
        .rate_off()
        .map_or_else(|| "n/a".to_string(), |r| format!("{:.2}%", r * 100.0));
    let n = s.evals_on + s.evals_off;
    format!(
        "auto-rollback by policy {policy} ({}): error rate on {on:.2}% vs off {off} \
         over {n} evaluations",
        trigger_name(trigger)
    )
}

/// The flag (unlocked) and its app, for transaction 1.
async fn flag_and_app(tx: &mut Tx<'_>, flag: FlagId) -> Result<Option<(FeatureFlag, FlagApp)>> {
    let Some(found) = flags::get_by_id(tx, flag).await? else {
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

/// Advance `last_checked_at` in transaction 1, which holds no flag lock.
/// Only armed rows nobody else holds are touched (`SKIP LOCKED`), so this
/// never waits on a row lock and cannot close a lock cycle with a concurrent
/// checker's transaction 2 or a re-arm, which take policy rows in a different
/// order. A skipped row is just picked up again sooner by the backstop.
async fn mark_checked_unlocked(tx: &mut Tx<'_>, ids: &[Uuid]) -> Result<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let org = tx.org();
    sqlx::query(
        "UPDATE rollback_policies SET last_checked_at = now() \
         WHERE org_id = $1 AND id IN ( \
           SELECT id FROM rollback_policies \
           WHERE org_id = $1 AND id = ANY($2) AND tripped_at IS NULL \
           FOR UPDATE SKIP LOCKED)",
    )
    .bind(org)
    .bind(ids)
    .execute(tx.conn())
    .await?;
    Ok(())
}

/// Advance `last_checked_at` in transaction 2, under the flag's row lock.
/// This one may wait: every writer that takes several of a flag's policy rows
/// at once (re-arming, another checker's transaction 2) holds the same flag
/// lock and so runs strictly before or after, and transaction 1 never waits,
/// so a wait here cannot be part of a cycle.
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

// ------------------------------------------------------------- the loop

/// How long a flag whose check failed is left alone, by hints and the
/// backstop alike.
pub const FAILURE_COOLDOWN: Duration = Duration::from_secs(3600);

/// How often a non-zero count of dropped hints is logged.
const DROPPED_REPORT_EVERY: Duration = Duration::from_secs(60);

type Key = (OrgId, FlagId);

/// One org's flags that just received telemetry and have an armed policy.
#[derive(Debug)]
struct Hint {
    org: OrgId,
    flags: Vec<FlagId>,
}

/// The sending side of the checker's hint channel. Cheap to clone; held by the
/// SDK API's telemetry handlers.
#[derive(Debug, Clone)]
pub struct Hints {
    /// `None` for [`Hints::detached`].
    tx: Option<mpsc::Sender<Hint>>,
    /// Hints dropped on a full channel.
    dropped: Arc<AtomicU64>,
    /// Hints sent after the checker stopped.
    closed: Arc<AtomicU64>,
}

impl Hints {
    fn channel(capacity: usize) -> (Self, mpsc::Receiver<Hint>) {
        let (tx, rx) = mpsc::channel(capacity);
        let hints = Self {
            tx: Some(tx),
            dropped: Arc::new(AtomicU64::new(0)),
            closed: Arc::new(AtomicU64::new(0)),
        };
        (hints, rx)
    }

    /// Tell the checker these flags of `org` just received telemetry.
    /// Synchronous and never blocks: a full channel drops the hint (the
    /// backstop picks the flags up) and counts it. A stopped checker cannot
    /// be told anything: that is counted separately and logged at `error`
    /// the first time, since no flag is then rolled back automatically. An
    /// empty `flags` sends nothing.
    ///
    /// ```
    /// use flags_core::ids::FlagId;
    /// use flags_core::rollback::Hints;
    /// use otto_tenant::ids::OrgId;
    /// Hints::detached().send(OrgId::new(), vec![FlagId::new()]); // a no-op
    /// ```
    pub fn send(&self, org: OrgId, flags: Vec<FlagId>) {
        if flags.is_empty() {
            return;
        }
        let Some(tx) = &self.tx else {
            return;
        };
        match tx.try_send(Hint { org, flags }) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                if self.closed.fetch_add(1, Ordering::Relaxed) == 0 {
                    tracing::error!(
                        "the auto-rollback checker is not running: telemetry hints go nowhere \
                         and no flag will be rolled back automatically until restart"
                    );
                }
            }
        }
    }

    /// Hints that go nowhere, silently: for tests, and for routers built
    /// without a checker.
    pub fn detached() -> Self {
        Self {
            tx: None,
            dropped: Arc::new(AtomicU64::new(0)),
            closed: Arc::new(AtomicU64::new(0)),
        }
    }
}

/// The checker's timing. [`Default`] is what production runs.
#[derive(Debug, Clone)]
pub struct CheckerConfig {
    /// The least time between two checks of one flag.
    pub debounce: Duration,
    /// How often, while awake, policies not checked for this long are hinted.
    /// Raised to 1 s if smaller.
    pub backstop_every: Duration,
    /// The most policies one backstop pass reads.
    pub backstop_limit: i64,
    /// Hints that can wait before new ones are dropped.
    pub channel_capacity: usize,
    /// The longest one flag's check may take; a check still running then is
    /// abandoned (its open transaction rolls back) and retried after the
    /// debounce, so one stuck check cannot stall every other flag.
    pub check_timeout: Duration,
}

impl Default for CheckerConfig {
    fn default() -> Self {
        Self {
            debounce: Duration::from_secs(30),
            backstop_every: Duration::from_secs(300),
            backstop_limit: 200,
            channel_capacity: 1024,
            check_timeout: Duration::from_secs(15),
        }
    }
}

/// The background task that runs [`check_flag`] for hinted and due flags.
pub struct Checker;

impl Checker {
    /// Start the checker. Returns the [`Hints`] to hand to the SDK API, and
    /// the task to await on shutdown (it returns once `shutdown` changes or
    /// its sender is dropped, after the check in progress, if any).
    pub fn spawn<E: EntitlementSource + 'static>(
        db: Db,
        entitlements: E,
        config: CheckerConfig,
        shutdown: watch::Receiver<bool>,
    ) -> (Hints, JoinHandle<()>) {
        let (hints, rx) = Hints::channel(config.channel_capacity.max(1));
        let dropped = hints.dropped.clone();
        let task = tokio::spawn(async move {
            match run(db, entitlements, config, rx, dropped, shutdown).await {
                Exit::Requested => tracing::info!("auto-rollback checker stopped on shutdown"),
                Exit::SenderDropped => tracing::warn!(
                    "auto-rollback checker stopped: its shutdown sender was dropped without a \
                     shutdown, so no flag will be rolled back automatically until restart"
                ),
            }
        });
        (hints, task)
    }
}

/// Why the checker loop returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Exit {
    /// Shutdown was signalled.
    Requested,
    /// The shutdown sender was dropped without signalling.
    SenderDropped,
}

async fn run<E: EntitlementSource>(
    db: Db,
    entitlements: E,
    config: CheckerConfig,
    mut rx: mpsc::Receiver<Hint>,
    dropped: Arc<AtomicU64>,
    mut shutdown: watch::Receiver<bool>,
) -> Exit {
    let mut schedule = Schedule::new(config.debounce);
    // The first backstop tick is immediate: a pass on start-up catches up on
    // hints a restart lost. `interval` panics on a zero period.
    let backstop_every = config.backstop_every.max(Duration::from_secs(1));
    let mut backstop = tokio::time::interval(backstop_every);
    backstop.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut housekeeping = tokio::time::interval(DROPPED_REPORT_EVERY);
    housekeeping.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut hints_open = true;

    loop {
        if *shutdown.borrow() {
            return Exit::Requested;
        }
        for key in schedule.take_due(Instant::now()) {
            let result = check_one(&db, &entitlements, key, config.check_timeout).await;
            let now = Instant::now();
            match result {
                Checked::Ok => schedule.checked(key, now),
                Checked::Retry => schedule.retry(key, now),
                Checked::GiveUp => schedule.failed(key, now),
            }
            if *shutdown.borrow() {
                return Exit::Requested;
            }
        }
        let wake = schedule.next_due();

        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() {
                    return Exit::SenderDropped;
                }
                if *shutdown.borrow() {
                    return Exit::Requested;
                }
            }
            hint = rx.recv(), if hints_open => match hint {
                Some(Hint { org, flags }) => {
                    let now = Instant::now();
                    for flag in flags {
                        schedule.hint((org, flag), now);
                    }
                }
                // Every sender is gone; the backstop still runs.
                None => hints_open = false,
            },
            () = sleep_until(wake) => {}
            _ = backstop.tick() => {
                match due_policies(&db, backstop_every, config.backstop_limit).await {
                    Ok(due) => {
                        let now = Instant::now();
                        for key in due {
                            schedule.hint(key, now);
                        }
                    }
                    Err(e) => {
                        tracing::warn!(code = e.code(), "auto-rollback backstop could not list due policies");
                        tracing::debug!(error = %e, "auto-rollback backstop failure");
                    }
                }
            }
            _ = housekeeping.tick() => {
                let n = dropped.swap(0, Ordering::Relaxed);
                if n > 0 {
                    tracing::warn!(
                        dropped = n,
                        "auto-rollback hints dropped on a full channel; the backstop will check those flags"
                    );
                }
                schedule.prune(Instant::now());
            }
        }
    }
}

/// Sleep until `due`, or forever with nothing scheduled.
async fn sleep_until(due: Option<Instant>) {
    match due {
        Some(due) => tokio::time::sleep_until(due.into()).await,
        None => std::future::pending().await,
    }
}

/// How a check of one flag ended, for scheduling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Checked {
    Ok,
    /// Failed in a way a later attempt could fix: retry after the debounce.
    Retry,
    /// Failed in a way retrying will not fix: cool down.
    GiveUp,
}

/// Whether a failed check is worth retrying soon: only for failures that
/// pass on their own ([`Error::is_transient`]). Anything else (a constraint
/// violation, a refused patch, a deleted org) would fail the same way again.
fn classify(e: &Error) -> Checked {
    if e.is_transient() {
        Checked::Retry
    } else {
        Checked::GiveUp
    }
}

/// A check that completed, judged by its worst policy: any permanent failure
/// gives up, else any transient one retries.
fn classify_report(report: &CheckReport) -> Checked {
    if report.failed.iter().any(|f| !f.transient) {
        Checked::GiveUp
    } else if report.failed.is_empty() {
        Checked::Ok
    } else {
        Checked::Retry
    }
}

/// Check one flag within `deadline` and log what happened. A check past its
/// deadline is dropped, which rolls its open transaction back, and retried.
async fn check_one(
    db: &Db,
    entitlements: &impl EntitlementSource,
    (org, flag): Key,
    deadline: Duration,
) -> Checked {
    let Ok(result) = tokio::time::timeout(deadline, check_flag(db, entitlements, org, flag)).await
    else {
        tracing::warn!(%org, %flag, timeout_ms = deadline.as_millis(), retry = true,
            "auto-rollback check timed out");
        return Checked::Retry;
    };
    match result {
        Ok(report) => {
            for (policy, outcome) in &report.outcomes {
                if *outcome == Outcome::RolledBack {
                    tracing::info!(%org, %flag, %policy, "auto-rollback turned a flag's environment off");
                } else {
                    tracing::debug!(%org, %flag, %policy, outcome = outcome.as_str(), "auto-rollback trip recorded, not acted on");
                }
            }
            classify_report(&report)
        }
        // A deleted org, not yet purged: expected, and nothing to do.
        Err(e @ Error::AccessRevoked) => {
            tracing::debug!(%org, %flag, "auto-rollback skipped a flag of a deleted org");
            classify(&e)
        }
        Err(e) => {
            let result = classify(&e);
            // The code only: database text never reaches a log above debug.
            tracing::warn!(
                %org, %flag, code = e.code(), retry = result == Checked::Retry,
                "auto-rollback check failed"
            );
            tracing::debug!(%org, %flag, error = %e, "auto-rollback check failure");
            result
        }
    }
}

/// What stands in for "never" when `at + d` would overflow.
const FAR_FUTURE: Duration = Duration::from_secs(30 * 365 * 24 * 3600);

/// `at + d`, saturating at a far-future instant instead of panicking, so a
/// huge debounce cannot crash the checker.
fn later(at: Instant, d: Duration) -> Instant {
    at.checked_add(d)
        .or_else(|| at.checked_add(FAR_FUTURE))
        .unwrap_or(at)
}

/// When each hinted flag is due. Pure: every method takes `now`, so it is
/// tested with synthetic instants.
#[derive(Debug)]
pub(crate) struct Schedule {
    debounce: Duration,
    /// Hinted flags and when each is due.
    pending: HashMap<Key, Instant>,
    /// When each flag was last checked, kept for one debounce interval.
    last_checked: HashMap<Key, Instant>,
    /// Flags whose last check failed, and until when they are left alone.
    cooldown: HashMap<Key, Instant>,
}

impl Schedule {
    pub(crate) fn new(debounce: Duration) -> Self {
        Self {
            debounce,
            pending: HashMap::new(),
            last_checked: HashMap::new(),
            cooldown: HashMap::new(),
        }
    }

    /// Schedule `key`: now, or at the end of its debounce interval or
    /// cooldown, whichever is later. An already pending key keeps its
    /// earliest due time.
    pub(crate) fn hint(&mut self, key: Key, now: Instant) {
        let mut due = now;
        if let Some(&at) = self.last_checked.get(&key) {
            due = due.max(later(at, self.debounce));
        }
        if let Some(&until) = self.cooldown.get(&key) {
            due = due.max(until);
        }
        self.pending
            .entry(key)
            .and_modify(|d| *d = (*d).min(due))
            .or_insert(due);
    }

    /// The earliest due time, if anything is pending.
    pub(crate) fn next_due(&self) -> Option<Instant> {
        self.pending.values().min().copied()
    }

    /// Remove and return the keys due by `now`, earliest first.
    pub(crate) fn take_due(&mut self, now: Instant) -> Vec<Key> {
        let mut due: Vec<(Instant, Key)> = self
            .pending
            .iter()
            .filter(|(_, &at)| at <= now)
            .map(|(&k, &at)| (at, k))
            .collect();
        due.sort_by_key(|(at, _)| *at);
        for (_, k) in &due {
            self.pending.remove(k);
        }
        due.into_iter().map(|(_, k)| k).collect()
    }

    /// `key` was checked successfully at `now`.
    pub(crate) fn checked(&mut self, key: Key, now: Instant) {
        self.last_checked.insert(key, now);
        self.cooldown.remove(&key);
    }

    /// `key`'s check failed at `now` in a way a retry could fix: it counts as
    /// checked, so the next hint is due after the debounce.
    pub(crate) fn retry(&mut self, key: Key, now: Instant) {
        self.last_checked.insert(key, now);
    }

    /// `key`'s check failed at `now` in a way retrying will not fix: it counts
    /// as checked, and is left alone for [`FAILURE_COOLDOWN`].
    pub(crate) fn failed(&mut self, key: Key, now: Instant) {
        self.last_checked.insert(key, now);
        self.cooldown.insert(key, later(now, FAILURE_COOLDOWN));
    }

    /// Forget checks older than the debounce and expired cooldowns, so the
    /// maps hold only flags that still affect scheduling.
    pub(crate) fn prune(&mut self, now: Instant) {
        let debounce = self.debounce;
        self.last_checked.retain(|_, at| later(*at, debounce) > now);
        self.cooldown.retain(|_, until| *until > now);
    }
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

    // ------------------------------------------------------- schedule

    const DEBOUNCE: Duration = Duration::from_secs(30);

    fn key() -> (OrgId, FlagId) {
        (OrgId::new(), FlagId::new())
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn a_first_hint_is_due_now() {
        let mut s = Schedule::new(DEBOUNCE);
        let t0 = Instant::now();
        let k = key();
        assert_eq!(s.next_due(), None);
        s.hint(k, t0);
        assert_eq!(s.next_due(), Some(t0));
        assert_eq!(s.take_due(t0), vec![k]);
        assert_eq!(s.next_due(), None);
        assert!(s.take_due(t0 + secs(60)).is_empty());
    }

    #[test]
    fn a_hint_soon_after_a_check_waits_for_the_trailing_edge() {
        let mut s = Schedule::new(DEBOUNCE);
        let t0 = Instant::now();
        let k = key();
        s.checked(k, t0);
        s.hint(k, t0 + secs(10));
        assert_eq!(s.next_due(), Some(t0 + secs(30)));
        assert!(s.take_due(t0 + secs(29)).is_empty(), "too early");
        // Deferred, never dropped: still there at the end of the interval.
        assert_eq!(s.take_due(t0 + secs(30)), vec![k]);
    }

    #[test]
    fn many_hints_for_a_pending_flag_stay_one_entry_at_the_earliest_time() {
        let mut s = Schedule::new(DEBOUNCE);
        let t0 = Instant::now();
        let k = key();
        let other = key();
        s.checked(k, t0);
        s.hint(k, t0 + secs(5));
        for n in 6..20 {
            s.hint(k, t0 + secs(n));
        }
        s.hint(other, t0 + secs(40));
        assert_eq!(s.pending.len(), 2);
        assert_eq!(s.next_due(), Some(t0 + secs(30)));
        // Due order: the earliest first.
        assert_eq!(s.take_due(t0 + secs(40)), vec![k, other]);
    }

    #[test]
    fn prune_forgets_checks_older_than_the_debounce() {
        let mut s = Schedule::new(DEBOUNCE);
        let t0 = Instant::now();
        let (old, recent) = (key(), key());
        s.checked(old, t0);
        s.checked(recent, t0 + secs(20));
        s.prune(t0 + secs(31));
        assert!(!s.last_checked.contains_key(&old));
        assert!(s.last_checked.contains_key(&recent));
        // A hint for the pruned flag is due at once.
        s.hint(old, t0 + secs(31));
        assert_eq!(s.next_due(), Some(t0 + secs(31)));
    }

    #[test]
    fn a_failed_check_cools_the_flag_down_for_an_hour() {
        let mut s = Schedule::new(DEBOUNCE);
        let t0 = Instant::now();
        let k = key();
        s.failed(k, t0);
        // A hint (or a backstop pass) a minute later waits out the cooldown.
        s.hint(k, t0 + secs(60));
        assert_eq!(s.next_due(), Some(t0 + FAILURE_COOLDOWN));
        assert!(s.take_due(t0 + FAILURE_COOLDOWN - secs(1)).is_empty());
        assert_eq!(s.take_due(t0 + FAILURE_COOLDOWN), vec![k]);
    }

    #[test]
    fn a_retriable_failure_retries_after_the_debounce_not_the_cooldown() {
        let mut s = Schedule::new(DEBOUNCE);
        let t0 = Instant::now();
        let k = key();
        s.retry(k, t0);
        assert!(!s.cooldown.contains_key(&k));
        s.hint(k, t0 + secs(1));
        assert_eq!(s.next_due(), Some(t0 + DEBOUNCE));
        assert_eq!(s.take_due(t0 + DEBOUNCE), vec![k]);
    }

    #[test]
    fn only_non_retriable_errors_give_up() {
        assert_eq!(classify(&Error::AccessRevoked), Checked::GiveUp);
        assert_eq!(classify(&Error::Invalid("bad".into())), Checked::GiveUp);
        assert_eq!(
            classify(&Error::Db(sqlx::Error::PoolTimedOut)),
            Checked::Retry
        );
        assert_eq!(
            classify(&Error::EntitlementUnavailable {
                feature: AUTO_ROLLBACK.into()
            }),
            Checked::Retry
        );
        // A database error that retrying cannot fix cools down.
        assert_eq!(
            classify(&Error::Db(sqlx::Error::RowNotFound)),
            Checked::GiveUp
        );
        assert_eq!(
            classify(&Error::Db(sqlx::Error::Protocol("bad".into()))),
            Checked::GiveUp
        );
    }

    fn failure(transient: bool) -> PolicyFailure {
        PolicyFailure {
            policy: Uuid::new_v4(),
            code: "x",
            transient,
        }
    }

    #[test]
    fn a_report_is_judged_by_its_worst_policy() {
        let mut report = CheckReport::default();
        assert_eq!(classify_report(&report), Checked::Ok);
        report.failed.push(failure(true));
        assert_eq!(classify_report(&report), Checked::Retry);
        report.failed.push(failure(false));
        report.failed.push(failure(true));
        assert_eq!(classify_report(&report), Checked::GiveUp);
    }

    #[test]
    fn a_successful_check_clears_the_cooldown() {
        let mut s = Schedule::new(DEBOUNCE);
        let t0 = Instant::now();
        let k = key();
        s.failed(k, t0);
        s.checked(k, t0 + secs(1));
        s.hint(k, t0 + secs(60));
        assert_eq!(s.next_due(), Some(t0 + secs(60)));
    }

    #[test]
    fn prune_drops_expired_cooldowns_only() {
        let mut s = Schedule::new(DEBOUNCE);
        let t0 = Instant::now();
        let (old, recent) = (key(), key());
        s.failed(old, t0);
        s.failed(recent, t0 + secs(600));
        s.prune(t0 + FAILURE_COOLDOWN);
        assert!(!s.cooldown.contains_key(&old));
        assert!(s.cooldown.contains_key(&recent));
    }

    #[test]
    fn a_huge_debounce_saturates_instead_of_panicking() {
        let mut s = Schedule::new(Duration::MAX);
        let t0 = Instant::now();
        let k = key();
        s.checked(k, t0);
        s.hint(k, t0 + secs(1));
        let due = s.next_due().unwrap();
        assert!(due >= t0 + FAR_FUTURE, "deferred far into the future");
        assert!(s.take_due(t0 + secs(3600)).is_empty());
        s.prune(t0 + secs(3600));
        assert!(s.last_checked.contains_key(&k));
    }

    // ---------------------------------------------------------- hints

    #[test]
    fn sending_on_a_full_channel_counts_a_drop_and_returns() {
        let (hints, mut rx) = Hints::channel(1);
        let org = OrgId::new();
        hints.send(org, vec![FlagId::new()]);
        hints.send(org, vec![FlagId::new()]);
        hints.send(org, vec![FlagId::new()]);
        assert_eq!(hints.dropped.load(Ordering::Relaxed), 2);
        assert!(rx.try_recv().is_ok());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn sending_on_a_detached_channel_does_nothing() {
        let hints = Hints::detached();
        hints.send(OrgId::new(), vec![FlagId::new()]);
        assert_eq!(hints.dropped.load(Ordering::Relaxed), 0);
        assert_eq!(hints.closed.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn sending_after_the_checker_stopped_is_counted_apart_from_drops() {
        let (hints, rx) = Hints::channel(4);
        drop(rx);
        let org = OrgId::new();
        hints.send(org, vec![FlagId::new()]);
        hints.send(org, vec![FlagId::new()]);
        assert_eq!(hints.closed.load(Ordering::Relaxed), 2);
        assert_eq!(hints.dropped.load(Ordering::Relaxed), 0);
        // Clones share the counters.
        hints.clone().send(org, vec![FlagId::new()]);
        assert_eq!(hints.closed.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn an_empty_hint_is_not_sent() {
        let (hints, mut rx) = Hints::channel(1);
        hints.send(OrgId::new(), Vec::new());
        assert!(rx.try_recv().is_err());
        assert_eq!(hints.dropped.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn the_rollback_reason_is_numbers_only() {
        let id = Uuid::nil();
        assert_eq!(
            rollback_reason(id, Trigger::ErrorRatio, &s(200, 200, 30, 2)),
            format!(
                "auto-rollback by policy {id} (errorRatio): error rate on 15.00% vs off 1.00% \
                 over 400 evaluations"
            )
        );
        assert_eq!(
            rollback_reason(id, Trigger::MaxErrorRate, &s(1000, 0, 50, 0)),
            format!(
                "auto-rollback by policy {id} (maxErrorRate): error rate on 5.00% vs off n/a \
                 over 1000 evaluations"
            )
        );
    }

    #[test]
    fn trigger_names_match_their_wire_form() {
        for t in [
            Trigger::MaxErrorRate,
            Trigger::ErrorRatio,
            Trigger::OffHasNoErrors,
        ] {
            assert_eq!(serde_json::to_value(t).unwrap(), trigger_name(t));
        }
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
    fn a_sliver_of_off_traffic_cannot_trip_a_comparison() {
        // 99% rollout: 0.1% errors on, and the 99 evaluations left off saw
        // none, as they almost always would (0.1 expected). Too little off
        // traffic to compare against, so only maxErrorRate could trip.
        let sample = s(10_000, 99, 10, 0);
        assert_eq!(d(&sample), Decision::Healthy);
        assert!(!can_trip(&sample, &Thresholds::default()));
        // Nor does a ratio over a few off evaluations.
        assert_eq!(d(&s(1000, 50, 100, 1)), Decision::Healthy);
        // maxErrorRate still guards it.
        let t = Thresholds {
            max_error_rate: Some(0.0005),
            ..Thresholds::default()
        };
        assert!(can_trip(&sample, &t));
        assert_eq!(decide(&sample, &t), Decision::Trip(Trigger::MaxErrorRate));
    }

    #[test]
    fn exactly_the_minimum_off_traffic_may_trip() {
        // min_evaluations (100) off evaluations: both comparisons apply.
        let at_min = s(1000, 100, 50, 0);
        assert!(can_trip(&at_min, &Thresholds::default()));
        assert_eq!(d(&at_min), Decision::Trip(Trigger::OffHasNoErrors));
        assert_eq!(d(&s(1000, 100, 50, 1)), Decision::Trip(Trigger::ErrorRatio));
        // One fewer, and neither does.
        let below = s(1000, 99, 50, 0);
        assert!(!can_trip(&below, &Thresholds::default()));
        assert_eq!(d(&below), Decision::Healthy);
        // The off minimum follows the policy's own min_evaluations.
        let t = Thresholds {
            min_evaluations: 20,
            ..Thresholds::default()
        };
        assert_eq!(
            decide(&s(1000, 20, 50, 0), &t),
            Decision::Trip(Trigger::OffHasNoErrors)
        );
        assert_eq!(decide(&s(1000, 19, 50, 0), &t), Decision::Healthy);
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
            // Too little off traffic to compare: no longer a trip.
            (100, 1, 10, 0),
            (10_000, 99, 10, 0),
            (10_000, 100, 10, 0),
            (1000, 100, 50, 0),
            (1000, 99, 50, 0),
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
