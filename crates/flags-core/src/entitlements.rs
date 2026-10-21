//! Whether an org's plan includes a feature, asked of the otto platform.
//!
//! Shares the [`PlatformClient`] held by [`crate::usage::Meter`] (and its 60 s
//! cache) but none of the meter's behaviour. A timeout or platform error is
//! [`Entitlement::Unknown`], which every caller treats as not entitled.
//!
//! Never called on the SDK request path (the SDK path never calls the
//! platform): only from MCP tools and the background checker. Each lookup is
//! bounded by a 2 s timeout and must not be made while holding a pooled
//! connection.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use otto_resource::{PlatformClient, UsageStatus};
use otto_tenant::ids::OrgId;

use crate::error::{Error, Result};
use crate::usage::plan_name;

/// The plan feature that gates automatic rollback.
pub const AUTO_ROLLBACK: &str = "auto_rollback";

const LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a failed lookup is remembered (still fail closed).
const UNKNOWN_TTL: Duration = Duration::from_secs(10);

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

/// Orgs whose last lookup failed, remembered briefly so an outage costs one
/// timeout per org per [`UNKNOWN_TTL`] rather than one per call. The platform
/// client caches successes but never failures.
#[derive(Default)]
struct NegativeCache {
    until: Mutex<HashMap<OrgId, Instant>>,
}

impl NegativeCache {
    fn is_unknown(&self, org: OrgId, now: Instant) -> bool {
        self.until
            .lock()
            .ok()
            .and_then(|m| m.get(&org).copied())
            .is_some_and(|until| until > now)
    }

    /// Remember `org` as unknown until `now + UNKNOWN_TTL`, dropping expired
    /// entries so the map stays bounded by the orgs failing right now.
    fn mark(&self, org: OrgId, now: Instant) {
        if let Ok(mut m) = self.until.lock() {
            m.retain(|_, until| *until > now);
            m.insert(org, now + UNKNOWN_TTL);
        }
    }
}

/// Turn an answer into the caller's `Result`.
fn decide(answer: Entitlement, feature: &str, upgrade_url: &str) -> Result<()> {
    match answer {
        Entitlement::Granted => Ok(()),
        Entitlement::Denied { plan } => Err(Error::FeatureNotInPlan {
            feature: feature.to_string(),
            plan,
            upgrade_url: upgrade_url.to_string(),
        }),
        Entitlement::Unknown => Err(Error::EntitlementUnavailable {
            feature: feature.to_string(),
        }),
    }
}

/// The platform-backed [`EntitlementSource`].
#[derive(Clone)]
pub struct Entitlements {
    platform: Arc<PlatformClient>,
    upgrade_url: String,
    failed: Arc<NegativeCache>,
}

impl Entitlements {
    pub fn new(platform: Arc<PlatformClient>, upgrade_url: impl Into<String>) -> Self {
        Self {
            platform,
            upgrade_url: upgrade_url.into(),
            failed: Arc::default(),
        }
    }

    /// Ok, or [`Error::FeatureNotInPlan`] / [`Error::EntitlementUnavailable`].
    /// For MCP tools, before a transaction opens.
    pub async fn require(&self, org: OrgId, feature: &str) -> Result<()> {
        decide(self.check(org, feature).await, feature, &self.upgrade_url)
    }
}

impl EntitlementSource for Entitlements {
    fn check(&self, org: OrgId, feature: &str) -> impl Future<Output = Entitlement> + Send {
        let feature = feature.to_string();
        async move {
            if self.failed.is_unknown(org, Instant::now()) {
                return Entitlement::Unknown;
            }
            let f = match tokio::time::timeout(
                LOOKUP_TIMEOUT,
                self.platform.usage_status(org.as_uuid()),
            )
            .await
            {
                Ok(Ok(status)) => return from_status(&status, &feature),
                // Any failure is Unknown, `NotFound` (the org is gone) included,
                // so the feature fails closed.
                Ok(Err(e)) => crate::error::platform_failure_reason(&e),
                Err(_) => crate::error::Failure::reason("timeout"),
            };
            self.failed.mark(org, Instant::now());
            tracing::warn!(org = %org, feature = %feature, reason = f.reason, status = f.status, transport = f.transport,
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

    #[test]
    fn a_platform_without_features_denies() {
        let mut v = serde_json::json!({
            "org_id": "6f9619ff-8b86-d011-b42d-00c04fc964ff",
            "plan": "team",
            "period_start": "2026-10-01",
            "billable_count": 0,
            "total_count": 0,
            "included_ops": 100,
            "hard_stop": false,
        });
        assert!(v.get("features").is_none());
        let s: UsageStatus = serde_json::from_value(v.take()).expect("older body");
        assert_eq!(
            from_status(&s, AUTO_ROLLBACK),
            Entitlement::Denied {
                plan: "Team".into()
            }
        );
    }

    #[test]
    fn decide_maps_each_answer() {
        let url = "https://example.test/upgrade";
        assert!(decide(Entitlement::Granted, AUTO_ROLLBACK, url).is_ok());
        match decide(
            Entitlement::Denied {
                plan: "Free".into(),
            },
            AUTO_ROLLBACK,
            url,
        ) {
            Err(Error::FeatureNotInPlan {
                feature,
                plan,
                upgrade_url,
            }) => {
                assert_eq!(feature, AUTO_ROLLBACK);
                assert_eq!(plan, "Free");
                assert_eq!(upgrade_url, url);
            }
            other => panic!("expected FeatureNotInPlan, got {other:?}"),
        }
        assert!(matches!(
            decide(Entitlement::Unknown, AUTO_ROLLBACK, url),
            Err(Error::EntitlementUnavailable { .. })
        ));
    }

    #[test]
    fn a_failure_is_remembered_for_the_ttl_then_forgotten() {
        let cache = NegativeCache::default();
        let org = OrgId::from(uuid::Uuid::from_u128(1));
        let other = OrgId::from(uuid::Uuid::from_u128(2));
        let t0 = Instant::now();
        assert!(!cache.is_unknown(org, t0));
        cache.mark(org, t0);
        assert!(cache.is_unknown(org, t0 + UNKNOWN_TTL / 2));
        assert!(!cache.is_unknown(other, t0));
        assert!(!cache.is_unknown(org, t0 + UNKNOWN_TTL));
    }

    #[test]
    fn marking_prunes_expired_entries() {
        let cache = NegativeCache::default();
        let t0 = Instant::now();
        for i in 0..5 {
            cache.mark(OrgId::from(uuid::Uuid::from_u128(i)), t0);
        }
        cache.mark(OrgId::from(uuid::Uuid::from_u128(99)), t0 + UNKNOWN_TTL * 2);
        assert_eq!(cache.until.lock().expect("lock").len(), 1);
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
