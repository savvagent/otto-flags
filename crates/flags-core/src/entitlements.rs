//! Whether an org's plan includes a feature, asked of the otto platform.
//!
//! Shares the [`PlatformClient`] held by [`crate::usage::Meter`] (and its 60 s
//! cache) but none of the meter's behaviour: it ignores `FLAGS_ENFORCE_QUOTAS`
//! and has no fail-open window. A timeout or platform error is
//! [`Entitlement::Unknown`], which every caller treats as not entitled.
//!
//! Never called on the SDK request path (the SDK path never calls the
//! platform): only from MCP tools and the background checker. Each lookup is
//! bounded by a 2 s timeout and must not be made while holding a pooled
//! connection.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use otto_resource::{PlatformClient, UsageStatus};
use otto_tenant::ids::OrgId;

use crate::error::{Error, Result};
use crate::usage::plan_name;

/// The plan feature that gates automatic rollback.
pub const AUTO_ROLLBACK: &str = "auto_rollback";

const LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);

/// The platform's answer for one org and feature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Entitlement {
    Granted,
    /// The plan (capitalised, as shown to people) does not include it.
    Denied {
        plan: String,
    },
    /// The platform could not be asked. Callers must not grant on this.
    Unknown,
}

/// Where entitlements come from. Implemented by [`Entitlements`] and, with a
/// fixed answer for tests, by [`Entitlement`] itself.
pub trait EntitlementSource: Send + Sync {
    fn check(&self, org: OrgId, feature: &str) -> impl Future<Output = Entitlement> + Send;
}

impl EntitlementSource for Entitlement {
    fn check(&self, _org: OrgId, _feature: &str) -> impl Future<Output = Entitlement> + Send {
        let answer = self.clone();
        async move { answer }
    }
}

/// The platform-backed [`EntitlementSource`].
#[derive(Clone)]
pub struct Entitlements {
    platform: Arc<PlatformClient>,
    pub upgrade_url: String,
}

impl Entitlements {
    pub fn new(platform: Arc<PlatformClient>, upgrade_url: impl Into<String>) -> Self {
        Self {
            platform,
            upgrade_url: upgrade_url.into(),
        }
    }

    /// Ok, or [`Error::FeatureNotInPlan`] / [`Error::EntitlementUnavailable`].
    /// For MCP tools, before a transaction opens.
    pub async fn require(&self, org: OrgId, feature: &str) -> Result<()> {
        match self.check(org, feature).await {
            Entitlement::Granted => Ok(()),
            Entitlement::Denied { plan } => Err(Error::FeatureNotInPlan {
                feature: feature.to_string(),
                plan,
                upgrade_url: self.upgrade_url.clone(),
            }),
            Entitlement::Unknown => Err(Error::EntitlementUnavailable {
                feature: feature.to_string(),
            }),
        }
    }
}

impl EntitlementSource for Entitlements {
    fn check(&self, org: OrgId, feature: &str) -> impl Future<Output = Entitlement> + Send {
        let feature = feature.to_string();
        async move {
            let reason = match tokio::time::timeout(
                LOOKUP_TIMEOUT,
                self.platform.usage_status(org.as_uuid()),
            )
            .await
            {
                Ok(Ok(status)) => return from_status(&status, &feature),
                Ok(Err(e)) => e.to_string(),
                Err(_) => "timed out".to_string(),
            };
            tracing::warn!(org = %org, feature = %feature, reason = %reason,
                "could not confirm a plan feature with the platform; treating it as not granted");
            Entitlement::Unknown
        }
    }
}

pub(crate) fn from_status(status: &UsageStatus, feature: &str) -> Entitlement {
    if status.feature_enabled(feature) {
        Entitlement::Granted
    } else {
        Entitlement::Denied {
            plan: plan_name(&status.plan),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(features: serde_json::Value) -> UsageStatus {
        serde_json::from_value(serde_json::json!({
            "org_id": "6f9619ff-8b86-d011-b42d-00c04fc964ff",
            "plan": "free",
            "period_start": "2026-10-01",
            "billable_count": 0,
            "total_count": 0,
            "included_ops": 100,
            "hard_stop": true,
            "features": features,
        }))
        .expect("a valid usage status")
    }

    #[test]
    fn a_true_feature_is_granted() {
        let s = status(serde_json::json!({ "auto_rollback": true }));
        assert_eq!(from_status(&s, AUTO_ROLLBACK), Entitlement::Granted);
    }

    #[test]
    fn anything_but_true_is_denied_with_the_plan_name() {
        for features in [
            serde_json::json!({}),
            serde_json::json!({ "auto_rollback": false }),
            serde_json::json!({ "auto_rollback": "yes" }),
        ] {
            assert_eq!(
                from_status(&status(features), AUTO_ROLLBACK),
                Entitlement::Denied {
                    plan: "Free".into()
                }
            );
        }
    }

    #[tokio::test]
    async fn a_fixed_entitlement_answers_as_itself() {
        let org = OrgId::from(uuid::Uuid::nil());
        assert_eq!(
            Entitlement::Unknown.check(org, AUTO_ROLLBACK).await,
            Entitlement::Unknown
        );
    }
}
