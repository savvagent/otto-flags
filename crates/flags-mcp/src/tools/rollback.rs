//! Auto-rollback policies: arm one per flag and environment, see what it
//! would do, and read what it did. Creating a policy is billed and plan-gated;
//! everything else is free, so a downgraded or over-quota org can always look
//! at and clean up what it has.
//!
//! Nothing here returns reported text: policies, samples, and events are
//! numbers, ids, and enums.

use flags_core::apps::AppsExt;
use flags_core::entitlements::AUTO_ROLLBACK;
use flags_core::flags::FlagsExt;
use flags_core::rollback::{self, overlay, Mode, RollbackExt, ThresholdOverrides, Thresholds};
use flags_core::scopes::{FLAGS_READ, FLAGS_WRITE};
use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use rmcp::{tool, tool_router};
use serde::Deserialize;

use super::out;
use crate::server::{Flags, McpResult};

/// Shown with a new policy that has no absolute error-rate ceiling.
const NO_RATE_WARNING: &str = "Without maxErrorRate this policy cannot trip while the flag has \
                               too little off traffic (for example at 100% rollout).";

/// The six threshold fields, shared by `set_rollback_policy` and
/// `propose_rollback`. An omitted field takes its default when setting a
/// policy; propose_rollback keeps the stored value (the default only when
/// there is no policy). Each field's doc says so, since only field docs reach
/// the flattened schema.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ThresholdArgs {
    /// Evaluations needed before judging: with the flag on before anything
    /// trips, and with it off before the on and off rates are compared. At
    /// least 1; default 100 when setting a policy, while
    /// propose_rollback keeps the stored value.
    #[serde(default)]
    pub min_evaluations: Option<i64>,
    /// Errors with the flag on needed before judging. At least 1; default 10
    /// when setting a policy (propose_rollback keeps the stored value).
    #[serde(default)]
    pub min_errors: Option<i64>,
    /// Trip when the error rate with the flag on exceeds the rate with it off
    /// by more than this factor. Greater than 1.0; default 2.0 when setting a
    /// policy (propose_rollback keeps the stored value).
    #[serde(default)]
    pub max_error_ratio: Option<f64>,
    /// Trip when the error rate with the flag on exceeds this absolute rate
    /// (errors per evaluation). Above 0 and at most 1; default none when
    /// setting a policy (propose_rollback keeps the stored value, and cannot
    /// clear it). The only trigger that works when the flag has too little
    /// off traffic, as at 100%.
    #[serde(default)]
    pub max_error_rate: Option<f64>,
    /// How far back to look, in hours: the current hour so far plus this
    /// many minus one before it. 1 to 24; default 1 when setting a policy
    /// (propose_rollback keeps the stored value).
    #[serde(default)]
    pub window_hours: Option<i32>,
    /// Count errors and evaluations reported by public client keys too.
    /// Default false (server keys only) when setting a policy
    /// (propose_rollback keeps the stored value). Client keys are public, so
    /// anyone holding one can forge reports; turning this on lets forged
    /// reports trip, or hold back, the rollback, so setting it needs an
    /// owner or admin.
    #[serde(default)]
    pub include_client_reports: Option<bool>,
}

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
    #[serde(flatten)]
    pub thresholds: ThresholdArgs,
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
    /// Fields given here override the stored policy (else the defaults).
    #[serde(flatten)]
    pub thresholds: ThresholdArgs,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct EventsArgs {
    /// The app's name (or id).
    pub app: String,
    /// Only this flag's events. Defaults to every flag in the app.
    #[serde(default)]
    pub key: Option<String>,
    /// How many events, newest first. Default 50; values outside 1-200 are clamped.
    #[serde(default)]
    pub limit: Option<i64>,
}

impl From<ThresholdArgs> for ThresholdOverrides {
    fn from(a: ThresholdArgs) -> Self {
        Self {
            min_evaluations: a.min_evaluations,
            min_errors: a.min_errors,
            max_error_ratio: a.max_error_ratio,
            max_error_rate: a.max_error_rate,
            window_hours: a.window_hours,
            include_client_reports: a.include_client_reports,
        }
    }
}

/// Why opting in to client reports takes an owner or admin.
const CLIENT_REPORTS_NEED_ADMIN: &str = "counting client-key reports in an auto-rollback policy \
    (includeClientReports) needs an owner or admin of this organization, because anyone holding \
    the public client key can then trip or hold back the rollback; ask one to set it, or leave \
    includeClientReports off";

#[tool_router(router = rollback_router, vis = "pub(crate)")]
impl Flags {
    #[tool(
        name = "set_rollback_policy",
        description = "Arm auto-rollback for a flag in one environment, or replace its policy. \
                       Replaces the whole policy: any threshold you omit takes its default, not \
                       the stored value. Omitting mode resets it to enforce. To preview, call \
                       propose_rollback with every threshold you intend to set. When the error \
                       rate with the flag on crosses the thresholds, the environment is switched \
                       off (mode enforce) or the event is only recorded (mode observe). Setting a \
                       policy does not re-arm a tripped one (turn the environment back on to \
                       re-arm). By default only telemetry reported with the server key counts \
                       (includeClientReports opts in, needs an owner or admin). Needs a plan \
                       with auto-rollback; billed as a change.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = false
        )
    )]
    pub async fn set_rollback_policy(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(args): Parameters<SetPolicyArgs>,
    ) -> Result<Json<out::PolicyOut>, ErrorData> {
        let caller = self.caller(&parts)?;
        caller.require_scope(FLAGS_WRITE).mcp()?;
        // Platform calls: made before a pooled connection is held. A policy
        // replaces the whole stored one, so every set that counts client
        // reports opts in again and is checked.
        if args.thresholds.include_client_reports == Some(true) {
            self.require_admin(&caller, CLIENT_REPORTS_NEED_ADMIN)
                .await?;
        }
        self.entitlements()
            .require(caller.org_id, AUTO_ROLLBACK)
            .await
            .mcp()?;
        let warning = args
            .thresholds
            .max_error_rate
            .is_none()
            .then(|| NO_RATE_WARNING.into());
        let thresholds = overlay(Thresholds::default(), &args.thresholds.into());
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
        description = "Delete a flag's auto-rollback policy for one environment. Its history is \
                       kept: its rollback_events stay listed for the flag, with policyId null. \
                       Free and never plan-gated, so an org that downgraded can always clean up.",
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true
        )
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
                       and whether each is armed, tripped, or has lost its environment. tripped \
                       means an enforcing policy turned the environment off; observe, \
                       not-entitled and unknown-entitlement trips leave it armed and show only \
                       in rollback_events. A policy whose last check failed shows \
                       lastErrorCode; it stays armed and is retried. Free.",
        annotations(read_only_hint = true)
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
                       the stored policy (or the defaults when there is none) and overrides the \
                       fields you pass; it cannot clear a stored maxErrorRate. It judges the \
                       numbers only; it ignores the policy's mode and the plan, but reports \
                       canAct false for an archived flag or an environment the app no longer \
                       has. Changes nothing. Free.",
        annotations(read_only_hint = true)
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
        let proposal =
            rollback::propose(&mut tx, &app, &flag, environment, &args.thresholds.into())
                .await
                .mcp()?;
        // Read only: dropping the transaction rolls it back.
        drop(tx);
        Ok(Json(out::ProposalOut {
            key: flag.key,
            environment: environment.to_string(),
            proposal,
        }))
    }

    #[tool(
        name = "rollback_events",
        description = "What auto-rollback did, newest first, with the numbers it judged. \
                       Outcomes: rolled_back (an enforcing policy turned the environment off), \
                       would_roll_back (observe mode), not_entitled (the plan lacks \
                       auto-rollback), entitlement_unknown (the platform could not say; nothing \
                       changed). The last three are recorded at most once per hour per policy. \
                       Kept 90 days. For one flag or the whole app, at most 200. Events hold \
                       numbers only, no reported text. Free.",
        annotations(read_only_hint = true)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_threshold_arg_reaches_the_overrides() {
        let o: ThresholdOverrides = ThresholdArgs {
            min_evaluations: Some(7),
            min_errors: Some(3),
            max_error_ratio: Some(5.0),
            max_error_rate: Some(0.5),
            window_hours: Some(12),
            include_client_reports: Some(true),
        }
        .into();
        assert_eq!(
            o,
            ThresholdOverrides {
                min_evaluations: Some(7),
                min_errors: Some(3),
                max_error_ratio: Some(5.0),
                max_error_rate: Some(0.5),
                window_hours: Some(12),
                include_client_reports: Some(true),
            }
        );
    }

    #[test]
    fn the_warning_uses_the_wire_name() {
        assert!(NO_RATE_WARNING.contains("maxErrorRate"));
        assert!(!NO_RATE_WARNING.contains("max_error_rate"));
    }

    #[test]
    fn flattened_threshold_docs_reach_the_schema() {
        for schema in [
            serde_json::to_value(schemars::schema_for!(SetPolicyArgs)).unwrap(),
            serde_json::to_value(schemars::schema_for!(ProposeArgs)).unwrap(),
        ] {
            let text = schema.to_string();
            for field in [
                "minEvaluations",
                "minErrors",
                "maxErrorRatio",
                "maxErrorRate",
                "windowHours",
                "includeClientReports",
            ] {
                assert!(text.contains(field), "schema lacks {field}: {text}");
            }
            assert!(text.contains("default 100"), "docs lost: {text}");
            assert!(text.contains("forge"), "docs lost: {text}");
            // Which base an omitted field takes differs between the tools.
            assert!(text.contains("when setting a policy"), "docs lost: {text}");
            assert!(
                text.contains("propose_rollback keeps the stored value"),
                "docs lost: {text}"
            );
            assert!(text.contains("owner or admin"), "docs lost: {text}");
        }
    }

    fn description(name: &str) -> String {
        crate::tools::router()
            .list_all()
            .into_iter()
            .find(|t| t.name == name)
            .and_then(|t| t.description)
            .unwrap_or_else(|| panic!("no description for {name}"))
            .to_string()
    }

    fn says(name: &str, phrases: &[&str]) {
        let text = description(name);
        for phrase in phrases {
            assert!(
                text.contains(phrase),
                "{name} does not say {phrase:?}: {text}"
            );
        }
    }

    #[test]
    fn set_rollback_policy_says_what_a_set_resets_and_what_counts() {
        says(
            "set_rollback_policy",
            &[
                "Omitting mode resets it to enforce.",
                "Setting a policy does not re-arm a tripped one (turn the environment back on \
                 to re-arm).",
                "By default only telemetry reported with the server key counts \
                 (includeClientReports opts in, needs an owner or admin).",
            ],
        );
    }

    #[test]
    fn list_rollback_policies_says_what_tripped_means() {
        says(
            "list_rollback_policies",
            &[
                "tripped means an enforcing policy turned the environment off; observe, \
               not-entitled and unknown-entitlement trips leave it armed and show only in \
               rollback_events.",
                "A policy whose last check failed shows lastErrorCode; it stays armed and is \
                 retried.",
            ],
        );
    }

    #[test]
    fn rollback_events_names_its_outcomes_and_retention() {
        says(
            "rollback_events",
            &[
                "rolled_back",
                "would_roll_back",
                "not_entitled",
                "entitlement_unknown",
                "at most once per hour per policy",
                "90 days",
            ],
        );
        assert!(!description("rollback_events").contains("failure"));
    }

    #[test]
    fn remove_rollback_policy_says_its_history_is_kept() {
        says(
            "remove_rollback_policy",
            &["Its history is kept", "policyId null"],
        );
    }

    #[test]
    fn propose_rollback_says_it_judges_numbers_only() {
        says(
            "propose_rollback",
            &["judges the numbers only; it ignores the policy's mode and the plan"],
        );
    }
}
