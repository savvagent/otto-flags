//! Metering MCP tool calls against the org's plan, and shipping the record to
//! the platform. The same design as otto-factory's `of-billing`, condensed:
//!
//! - **Recording is local and atomic.** [`Meter::charge`] writes a
//!   `usage_outbox` row in the tool's own transaction, so a failed call rolls
//!   its record back and a successful one is recorded even while the platform
//!   is down.
//! - **Shipping is a background task** ([`run`]) that posts rows to the
//!   platform and deletes what it answered for. The platform dedupes on the
//!   row's `event_id`, so retries are free.
//! - **Enforcing** reads the platform's cached usage status (and counts this
//!   org's unshipped rows on top). Off by default. If the platform cannot
//!   answer, the call is allowed and still recorded.
//!
//! Writes are billable; reads, dry-run evaluation, and the identity tools are
//! recorded but free. The SDK evaluation surface is not metered.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use otto_resource::{PlatformClient, UsageEvent, UsageStatus};
use otto_tenant::ids::{OrgId, UserId};
use otto_tenant::{Db, Tx};
use serde::Serialize;
use sqlx::FromRow;
use tokio::sync::watch;
use uuid::Uuid;

use crate::error::{Error, Result};

/// Tools that consume the plan's allowance. Everything else is free.
pub const BILLABLE: &[&str] = &[
    "create_app",
    "update_app",
    "rotate_app_keys",
    "create_flag",
    "update_flag",
    "set_flag_environment",
    "archive_flag",
    "restore_flag",
    "rollback_flag",
];

pub fn is_billable(tool: &str) -> bool {
    BILLABLE.contains(&tool)
}

const QUOTA_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);
const FAIL_OPEN_FOR: Duration = Duration::from_secs(5);
pub const WARN_AT: f64 = 0.8;

#[derive(Clone)]
pub struct Meter {
    platform: Arc<PlatformClient>,
    fail_open: Arc<Mutex<HashMap<OrgId, Instant>>>,
    pub enforce: bool,
    pub upgrade_url: String,
}

impl std::fmt::Debug for Meter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Meter")
            .field("enforce", &self.enforce)
            .finish_non_exhaustive()
    }
}

impl Meter {
    pub fn new(
        platform: Arc<PlatformClient>,
        enforce: bool,
        upgrade_url: impl Into<String>,
    ) -> Self {
        Self {
            platform,
            fail_open: Arc::default(),
            enforce,
            upgrade_url: upgrade_url.into(),
        }
    }

    /// Look the org's standing up before a transaction opens, so a slow
    /// platform never holds a pooled connection. A no-op unless enforcing.
    pub async fn warm(&self, org: OrgId) {
        if self.enforce {
            let _ = self.status_for(org).await;
        }
    }

    /// Record one tool call in `tx`, or refuse it if the org is out of budget.
    pub async fn charge(&self, tx: &mut Tx<'_>, user: UserId, tool: &str) -> Result<()> {
        let billable = is_billable(tool);
        if self.enforce && billable {
            if let Some(mut status) = self.status_for(tx.org()).await {
                let org = tx.org();
                let unshipped: i64 = sqlx::query_scalar(
                    "SELECT count(*) FROM usage_outbox WHERE org_id = $1 AND billable",
                )
                .bind(org)
                .fetch_one(tx.conn())
                .await?;
                status.billable_count += unshipped;
                if status.is_blocked() {
                    return Err(Error::QuotaExceeded {
                        tool: tool.to_string(),
                        used: status.billable_count,
                        included: status.included_ops,
                        plan: plan_name(&status.plan),
                        upgrade_url: self.upgrade_url.clone(),
                    });
                }
            }
        }
        let org = tx.org();
        sqlx::query(
            "INSERT INTO usage_outbox (org_id, user_id, tool, billable) VALUES ($1,$2,$3,$4)",
        )
        .bind(org)
        .bind(user)
        .bind(tool)
        .bind(billable)
        .execute(tx.conn())
        .await?;
        Ok(())
    }

    async fn status_for(&self, org: OrgId) -> Option<UsageStatus> {
        if let Some(until) = self
            .fail_open
            .lock()
            .ok()
            .and_then(|m| m.get(&org).copied())
        {
            if until > Instant::now() {
                return None;
            }
        }
        let failure = match tokio::time::timeout(
            QUOTA_LOOKUP_TIMEOUT,
            self.platform.usage_status(org.as_uuid()),
        )
        .await
        {
            Ok(Ok(status)) => {
                if let Ok(mut m) = self.fail_open.lock() {
                    m.remove(&org);
                }
                return Some(status);
            }
            Ok(Err(e)) => e.to_string(),
            Err(_) => "timed out".to_string(),
        };
        tracing::warn!(org = %org, reason = %failure,
            "could not read usage from the platform; allowing billable calls for {FAIL_OPEN_FOR:?}");
        if let Ok(mut m) = self.fail_open.lock() {
            m.insert(org, Instant::now() + FAIL_OPEN_FOR);
        }
        None
    }

    /// The org's standing, for the free `whoami` and `usage` tools.
    pub async fn report(&self, org: OrgId) -> Result<Status> {
        let usage = self.platform.usage_status(org.as_uuid()).await?;
        Ok(Status::new(usage, self.enforce))
    }
}

fn plan_name(plan: &str) -> String {
    let mut chars = plan.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Where an org stands against its monthly allowance (shared by every otto-*
/// service the org uses).
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub plan: String,
    pub included_ops: i64,
    pub billable_used: i64,
    pub remaining: i64,
    pub total_calls: i64,
    pub period_start: chrono::NaiveDate,
    /// True past 80 % of the allowance. Worth telling a human.
    pub warning: bool,
    pub hard_stop: bool,
    /// Whether this server refuses billable calls over the allowance at all.
    pub enforced: bool,
}

impl Status {
    fn new(u: UsageStatus, enforced: bool) -> Self {
        let remaining = u.included_ops.saturating_sub(u.billable_count).max(0);
        let warning =
            u.included_ops <= 0 || u.billable_count as f64 >= u.included_ops as f64 * WARN_AT;
        Self {
            plan: plan_name(&u.plan),
            included_ops: u.included_ops,
            billable_used: u.billable_count,
            remaining,
            total_calls: u.total_count,
            period_start: u.period_start,
            warning,
            hard_stop: u.hard_stop,
            enforced,
        }
    }
}

// --------------------------------------------------------------------- shipper

#[derive(Debug, Clone)]
pub struct ShipperConfig {
    pub batch: usize,
    pub poll: Duration,
    pub claim_lease: Duration,
    pub base_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for ShipperConfig {
    fn default() -> Self {
        Self {
            batch: otto_resource::MAX_USAGE_BATCH,
            poll: Duration::from_secs(5),
            claim_lease: Duration::from_secs(120),
            base_backoff: Duration::from_secs(5),
            max_backoff: Duration::from_secs(15 * 60),
        }
    }
}

#[derive(Debug, FromRow)]
struct Row {
    id: i64,
    event_id: Uuid,
    org_id: Uuid,
    user_id: Option<Uuid>,
    tool: String,
    billable: bool,
    occurred_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, thiserror::Error)]
pub enum ShipError {
    #[error("the platform did not accept the batch: {0}")]
    Platform(#[from] otto_resource::Error),
    #[error("outbox database error: {0}")]
    Db(#[from] sqlx::Error),
}

/// Claim a batch of due rows, ship them, delete what the platform answered
/// for. Returns how many rows were claimed. Never loses a row: on any failure
/// nothing is deleted and the rows are rescheduled with backoff.
pub async fn ship_once(
    db: &Db,
    platform: &PlatformClient,
    cfg: &ShipperConfig,
) -> std::result::Result<usize, ShipError> {
    let mut rows: Vec<Row> = sqlx::query_as(
        "UPDATE usage_outbox SET attempts = attempts + 1, \
                next_attempt_at = now() + make_interval(secs => $2) \
         WHERE id IN ( \
             SELECT id FROM usage_outbox WHERE next_attempt_at <= now() \
             ORDER BY id LIMIT $1 FOR UPDATE SKIP LOCKED) \
         RETURNING id, event_id, org_id, user_id, tool, billable, occurred_at",
    )
    .bind(cfg.batch as i64)
    .bind(cfg.claim_lease.as_secs_f64())
    .fetch_all(db.pool())
    .await?;
    if rows.is_empty() {
        return Ok(0);
    }
    rows.sort_by_key(|r| r.id);
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let events: Vec<UsageEvent> = rows
        .iter()
        .map(|r| UsageEvent {
            event_id: r.event_id,
            org_id: r.org_id,
            user_id: r.user_id,
            tool: r.tool.clone(),
            billable: r.billable,
            occurred_at: r.occurred_at,
        })
        .collect();

    let receipt = match platform.ship_usage(&events).await {
        Ok(r) => r,
        Err(e) => {
            reschedule(db, &ids, &e.to_string(), cfg).await;
            return Err(e.into());
        }
    };
    let answered = receipt.accepted as usize + receipt.duplicates as usize + receipt.rejected.len();
    if answered != ids.len() {
        let msg = format!(
            "the platform accounted for {answered} of {} usage events",
            ids.len()
        );
        tracing::error!("{msg}; keeping every row and retrying");
        reschedule(db, &ids, &msg, cfg).await;
        return Err(otto_resource::Error::Decode(msg).into());
    }

    let mut tx = db.pool().begin().await?;
    for r in &receipt.rejected {
        tracing::error!(event_id = %r.event_id, reason = %r.reason,
            "the platform refused a usage event; moving it to usage_outbox_rejected");
        sqlx::query(
            "INSERT INTO usage_outbox_rejected \
               (event_id, org_id, user_id, tool, billable, occurred_at, reason) \
             SELECT event_id, org_id, user_id, tool, billable, occurred_at, $2 \
             FROM usage_outbox WHERE event_id = $1",
        )
        .bind(r.event_id)
        .bind(&r.reason)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query("DELETE FROM usage_outbox WHERE id = ANY($1)")
        .bind(&ids)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(ids.len())
}

async fn reschedule(db: &Db, ids: &[i64], error: &str, cfg: &ShipperConfig) {
    let r = sqlx::query(
        "UPDATE usage_outbox SET last_error = $2, \
                next_attempt_at = now() + make_interval(secs => \
                    LEAST($3::float8 * power(2, LEAST(attempts - 1, 20)), $4::float8)) \
         WHERE id = ANY($1)",
    )
    .bind(ids)
    .bind(error.chars().take(500).collect::<String>())
    .bind(cfg.base_backoff.as_secs_f64())
    .bind(cfg.max_backoff.as_secs_f64())
    .execute(db.pool())
    .await;
    if let Err(e) = r {
        // The claim lease still holds the rows back; this only delays a retry.
        tracing::warn!(error = %e, "could not record a usage shipping failure");
    }
}

/// Drain the outbox until `shutdown` flips, with a final pass on the way out.
pub async fn run(
    db: Db,
    platform: Arc<PlatformClient>,
    cfg: ShipperConfig,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut failures: u32 = 0;
    loop {
        let wait = match ship_once(&db, &platform, &cfg).await {
            Ok(n) => {
                failures = 0;
                if n >= cfg.batch {
                    Duration::ZERO
                } else {
                    cfg.poll
                }
            }
            Err(e) => {
                failures = failures.saturating_add(1);
                let wait = cfg
                    .poll
                    .saturating_mul(1u32 << failures.min(6))
                    .min(Duration::from_secs(60));
                match &e {
                    ShipError::Platform(otto_resource::Error::Unauthorized) => tracing::error!(
                        "the platform rejected this resource server's credential; usage is \
                         accumulating in the outbox. Check FLAGS_INTROSPECTION_SECRET"
                    ),
                    other => {
                        tracing::warn!(error = %other, retry_in = ?wait, "usage shipping failed")
                    }
                }
                wait
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = shutdown.changed() => {
                if let Err(e) = ship_once(&db, &platform, &cfg).await {
                    tracing::warn!(error = %e, "final usage flush failed; rows stay in the outbox");
                }
                return;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_are_billable_and_reads_are_not() {
        for t in [
            "create_flag",
            "set_flag_environment",
            "rollback_flag",
            "create_app",
        ] {
            assert!(is_billable(t), "{t}");
        }
        for t in [
            "whoami",
            "usage",
            "list_flags",
            "get_flag",
            "evaluate_flag",
            "flag_health",
        ] {
            assert!(!is_billable(t), "{t}");
        }
    }
}
