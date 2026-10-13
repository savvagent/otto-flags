//! Auto-rollback policies: arm one per flag and environment, see what it
//! would do, and read what it did. Creating a policy is billed and plan-gated;
//! everything else is free, so a downgraded or over-quota org can always look
//! at and clean up what it has.
//!
//! Nothing here returns reported text: policies, samples, and events are
//! numbers, ids, and enums.

use flags_core::apps::AppsExt;
use flags_core::entitlements::AUTO_ROLLBACK;
use flags_core::error::Error as CoreError;
use flags_core::flags::FlagsExt;
use flags_core::rollback::{
    can_trip, decide, Decision, Mode, Observation, RollbackExt, Thresholds,
};
use flags_core::scopes::{FLAGS_READ, FLAGS_WRITE};
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use rmcp::{tool, tool_router};
use serde::Deserialize;

use super::out;
use crate::server::{Flags, McpResult};

/// Shown with a new policy that has no absolute error-rate ceiling.
const NO_RATE_WARNING: &str = "this policy can trip only on max_error_rate while the flag has no \
                               off traffic; set max_error_rate to guard a flag at 100%";

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct SetPolicyArgs {
    /// The app's name (or id).
    pub app: String,
    /// The flag's key.
    pub key: String,
    /// One of the app's environments, for example "production".
    pub environment: String,
    /// "enforce" turns the environment off when the policy trips; "observe"
    /// only records what would have happened. Defaults to enforce.
    #[serde(default)]
    pub mode: Option<Mode>,
    /// Evaluations with the flag on needed before judging (at least 1).
    /// Defaults to 100.
    #[serde(default)]
    pub min_evaluations: Option<i64>,
    /// Errors with the flag on needed before judging (at least 1). Defaults to 10.
    #[serde(default)]
    pub min_errors: Option<i64>,
    /// Trip when the error rate with the flag on exceeds the rate with it off
    /// by more than this factor (greater than 1). Defaults to 2.
    #[serde(default)]
    pub max_error_ratio: Option<f64>,
    /// Trip when the error rate with the flag on exceeds this absolute rate
    /// (errors per evaluation, above 0 and at most 1). The only trigger that
    /// works when the flag has no off traffic, as at 100%. Unset by default.
    #[serde(default)]
    pub max_error_rate: Option<f64>,
    /// How far back to look, in hours (1 to 24). Defaults to 1.
    #[serde(default)]
    pub window_hours: Option<i32>,
    /// Count errors and evaluations reported by public client keys too.
    /// Defaults to false (server keys only).
    #[serde(default)]
    pub include_client_reports: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PolicyRefArgs {
    /// The app's name (or id).
    pub app: String,
    /// The flag's key.
    pub key: String,
    /// The policy's environment.
    pub environment: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ListPoliciesArgs {
    /// The app's name (or id).
    pub app: String,
    /// Only this flag's policies. Defaults to every flag in the app.
    #[serde(default)]
    pub key: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ProposeArgs {
    /// The app's name (or id).
    pub app: String,
    /// The flag's key.
    pub key: String,
    /// One of the app's environments.
    pub environment: String,
    /// Start from the policy that exists for this flag and environment (else
    /// the defaults); any field given here overrides it. See set_rollback_policy
    /// for what each means.
    #[serde(default)]
    pub min_evaluations: Option<i64>,
    #[serde(default)]
    pub min_errors: Option<i64>,
    #[serde(default)]
    pub max_error_ratio: Option<f64>,
    #[serde(default)]
    pub max_error_rate: Option<f64>,
    #[serde(default)]
    pub window_hours: Option<i32>,
    #[serde(default)]
    pub include_client_reports: Option<bool>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EventsArgs {
    /// The app's name (or id).
    pub app: String,
    /// Only this flag's events. Defaults to every flag in the app.
    #[serde(default)]
    pub key: Option<String>,
    /// How many events, newest first. Defaults to 50, at most 200.
    #[serde(default)]
    pub limit: Option<i64>,
}

/// The optional threshold fields, as given by an agent.
#[derive(Default)]
struct Overrides {
    min_evaluations: Option<i64>,
    min_errors: Option<i64>,
    max_error_ratio: Option<f64>,
    max_error_rate: Option<f64>,
    window_hours: Option<i32>,
    include_client_reports: Option<bool>,
}

/// `base` with every given field replaced.
fn overlay(base: Thresholds, o: Overrides) -> Thresholds {
    Thresholds {
        min_evaluations: o.min_evaluations.unwrap_or(base.min_evaluations),
        min_errors: o.min_errors.unwrap_or(base.min_errors),
        max_error_ratio: o.max_error_ratio.unwrap_or(base.max_error_ratio),
        max_error_rate: o.max_error_rate.or(base.max_error_rate),
        window_hours: o.window_hours.unwrap_or(base.window_hours),
        include_client_reports: o
            .include_client_reports
            .unwrap_or(base.include_client_reports),
    }
}

/// One sentence saying what the sample means for these thresholds.
fn note(decision: Decision, can_trip: bool) -> String {
    match decision {
        Decision::Trip(_) => "this policy would trip on the current sample".into(),
        Decision::BelowMinimums if can_trip => {
            "the sample is below the minimums, so the policy would not act yet".into()
        }
        Decision::Healthy if can_trip => {
            "the sample is within the thresholds, so the policy would not act".into()
        }
        _ => "the flag has no off traffic and maxErrorRate is unset, so this policy cannot trip \
              until it does; set maxErrorRate to guard a flag at 100%"
            .into(),
    }
}

#[tool_router(router = rollback_router, vis = "pub(crate)")]
impl Flags {
    #[tool(
        name = "set_rollback_policy",
        description = "Arm auto-rollback for a flag in one environment, or replace its policy. \
                       When the error rate with the flag on crosses the thresholds, the \
                       environment is switched off (mode enforce) or the event is only recorded \
                       (mode observe). Omitted thresholds take the defaults. Needs a plan with \
                       auto-rollback; billed as a change. Call propose_rollback first to see what \
                       it would do now."
    )]
    pub async fn set_rollback_policy(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<SetPolicyArgs>,
    ) -> Result<Json<out::PolicyOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(FLAGS_WRITE).mcp()?;
        // A platform call: made before a pooled connection is held.
        self.entitlements()
            .require(caller.org_id, AUTO_ROLLBACK)
            .await
            .mcp()?;
        let warning = args
            .max_error_rate
            .is_none()
            .then(|| NO_RATE_WARNING.into());
        let thresholds = overlay(
            Thresholds::default(),
            Overrides {
                min_evaluations: args.min_evaluations,
                min_errors: args.min_errors,
                max_error_ratio: args.max_error_ratio,
                max_error_rate: args.max_error_rate,
                window_hours: args.window_hours,
                include_client_reports: args.include_client_reports,
            },
        );
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "set_rollback_policy").await?;
        let app = tx.resolve_app(&args.app).await.mcp()?;
        let flag = tx.require_flag(&app, &args.key).await.mcp()?;
        let policy = tx
            .set_policy(
                &app,
                &flag,
                args.environment.trim(),
                args.mode.unwrap_or(Mode::Enforce),
                thresholds,
                caller.user_id,
            )
            .await
            .mcp()?;
        tx.commit().await.mcp()?;
        Ok(Json(out::PolicyOut { policy, warning }))
    }

    #[tool(
        name = "remove_rollback_policy",
        description = "Delete a flag's auto-rollback policy for one environment, and its \
                       events. Free and never plan-gated, so an org that downgraded can always \
                       clean up."
    )]
    pub async fn remove_rollback_policy(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<PolicyRefArgs>,
    ) -> Result<Json<out::RemovedPolicyOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(FLAGS_WRITE).mcp()?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "remove_rollback_policy")
            .await?;
        let app = tx.resolve_app(&args.app).await.mcp()?;
        let flag = tx.require_flag(&app, &args.key).await.mcp()?;
        let removed = tx
            .remove_policy(&flag, args.environment.trim(), caller.user_id)
            .await
            .mcp()?;
        tx.commit().await.mcp()?;
        Ok(Json(out::RemovedPolicyOut { removed }))
    }

    #[tool(
        name = "list_rollback_policies",
        description = "The auto-rollback policies in an app, or on one flag: mode, thresholds, \
                       and whether each is armed, tripped, or has lost its environment. Free."
    )]
    pub async fn list_rollback_policies(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ListPoliciesArgs>,
    ) -> Result<Json<out::PoliciesOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(FLAGS_READ).mcp()?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "list_rollback_policies")
            .await?;
        let app = tx.resolve_app(&args.app).await.mcp()?;
        let flag = match &args.key {
            Some(key) => Some(tx.require_flag(&app, key).await.mcp()?),
            None => None,
        };
        let policies = tx.list_policies(&app, flag.as_ref()).await.mcp()?;
        tx.commit().await.mcp()?;
        Ok(Json(out::PoliciesOut {
            app: app.name,
            policies,
        }))
    }

    #[tool(
        name = "propose_rollback",
        description = "Dry run: the error counts auto-rollback would judge right now for a flag \
                       in an environment, the thresholds, and what it would decide. Starts from \
                       the existing policy if there is one, else the defaults; fields you pass \
                       override them. Changes nothing. Free."
    )]
    pub async fn propose_rollback(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<ProposeArgs>,
    ) -> Result<Json<out::ProposalOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(FLAGS_READ).mcp()?;
        let mut tx = self.tx(&caller).await?;
        let app = tx.resolve_app(&args.app).await.mcp()?;
        let flag = tx.require_flag(&app, &args.key).await.mcp()?;
        let environment = args.environment.trim();
        if !app.environments.iter().any(|e| e == environment) {
            return Err(CoreError::Invalid(format!(
                "app '{}' has no environment '{environment}'; its environments are: {}",
                app.name,
                app.environments.join(", ")
            )))
            .mcp();
        }
        let existing = tx.find_policy(flag.id, environment).await.mcp()?;
        let thresholds = overlay(
            existing
                .as_ref()
                .map_or_else(Thresholds::default, |p| p.thresholds.clone()),
            Overrides {
                min_evaluations: args.min_evaluations,
                min_errors: args.min_errors,
                max_error_ratio: args.max_error_ratio,
                max_error_rate: args.max_error_rate,
                window_hours: args.window_hours,
                include_client_reports: args.include_client_reports,
            },
        );
        thresholds.validate().mcp()?;
        let (window_start, sample) = tx.sample(flag.id, environment, &thresholds).await.mcp()?;
        // Read only: dropping the transaction rolls it back.
        drop(tx);
        let decision = decide(&sample, &thresholds);
        let can_trip = can_trip(&sample, &thresholds);
        Ok(Json(out::ProposalOut {
            key: flag.key,
            environment: environment.to_string(),
            policy_id: existing.map(|p| p.id),
            would_trip: matches!(decision, Decision::Trip(_)),
            can_trip,
            note: note(decision, can_trip),
            observation: Observation {
                window_start,
                rate_on: sample.rate_on(),
                rate_off: sample.rate_off(),
                sample,
                thresholds,
                decision,
            },
        }))
    }

    #[tool(
        name = "rollback_events",
        description = "What auto-rollback did, newest first: each trip, observation, or failure \
                       with the numbers it judged. For one flag or the whole app, at most 200. \
                       Events hold numbers only, no reported text. Free."
    )]
    pub async fn rollback_events(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<EventsArgs>,
    ) -> Result<Json<out::EventsOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(FLAGS_READ).mcp()?;
        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "rollback_events").await?;
        let app = tx.resolve_app(&args.app).await.mcp()?;
        let flag = match &args.key {
            Some(key) => Some(tx.require_flag(&app, key).await.mcp()?),
            None => None,
        };
        let events = tx
            .rollback_events(&app, flag.as_ref(), args.limit.unwrap_or(50))
            .await
            .mcp()?;
        tx.commit().await.mcp()?;
        Ok(Json(out::EventsOut { events }))
    }
}
