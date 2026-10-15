//! Metering MCP tool calls against the org's plan, and shipping the record to
//! the platform. The same design as otto-factory's `of-billing`, condensed:
//!
//! - **Recording is local and atomic.** [`Meter::charge`] writes a
//!   `usage_outbox` row in the tool's own transaction, so a failed call rolls
//!   its record back and a successful one is recorded even while the platform
//!   is down.
//! - **Shipping is a background task** ([`run`]) that posts rows to the
//!   platform and deletes what it answered for. The platform dedupes on the
//!   row's `event_id`, so retries are free. Every statement the shipper issues
//!   runs in a maintenance transaction, and the writes after a claim are fenced
//!   on the `(id, attempts)` it set, so a lapsed claim cannot touch a newer
//!   one's rows.
//! - **Refused events are kept, then swept.** An event the platform rejects
//!   outright moves to `usage_outbox_rejected` with its reason, is kept
//!   [`REJECTED_RETENTION_DAYS`], and goes with its org on `org.deleted`.
//! - **Enforcing** reads the platform's cached usage status (and counts this
//!   org's unshipped rows on top). Off by default. If the platform cannot
//!   answer, the call is allowed and still recorded.
//!
//! Writes are billable; reads, dry-run evaluation, and the identity tools are
//! recorded but free. The SDK evaluation surface is not metered.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::error::Failure;
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
    "set_rollback_policy",
];

pub fn is_billable(tool: &str) -> bool {
    BILLABLE.contains(&tool)
}

const QUOTA_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);
const FAIL_OPEN_FOR: Duration = Duration::from_secs(5);
pub const WARN_AT: f64 = 0.8;
/// How long `usage_outbox_rejected` keeps an event the platform refused, for
/// operators reconciling usage with it (#44); [`sweep_rejected`] removes older
/// ones (#74).
pub const REJECTED_RETENTION_DAYS: i32 = 90;
/// The platform's rejection reason is its text, kept for operators: this many
/// characters are enough to read it, and bound the column.
const MAX_REJECTED_REASON: usize = 500;

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
            Ok(Err(e)) => Some(e),
            Err(_) => None,
        };
        // A timeout has no text; it is named as entitlements names it.
        let f = failure.as_ref().map_or(
            Failure::reason("timeout"),
            crate::error::platform_failure_reason,
        );
        tracing::warn!(org = %org, reason = f.reason, status = f.status, transport = f.transport,
            "could not read usage from the platform; allowing billable calls for {FAIL_OPEN_FOR:?}");
        if let Some(e) = &failure {
            tracing::debug!(org = %org, reason = f.reason, status = f.status, transport = f.transport, error = %e,
                "could not read usage from the platform; allowing billable calls for {FAIL_OPEN_FOR:?}");
        }
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

pub(crate) fn plan_name(plan: &str) -> String {
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
    attempts: i32,
}

#[derive(Debug, thiserror::Error)]
pub enum ShipError {
    #[error("the platform did not accept the batch: {0}")]
    Platform(#[from] otto_resource::Error),
    #[error("outbox database error: {0}")]
    Db(#[from] sqlx::Error),
}

impl ShipError {
    /// A short, text-free [`Failure`] (reason, HTTP status, SQLSTATE, transport
    /// sub-kind) that is safe to log above debug: the error text can carry SQL,
    /// schema names, response bodies, and URLs.
    pub fn failure_reason(&self) -> Failure {
        match self {
            ShipError::Platform(p) => crate::error::platform_failure_reason(p),
            ShipError::Db(e) => crate::error::db_failure_reason(e),
        }
    }
}

/// Claim a batch of due rows, ship them, delete what the platform answered
/// for. Returns how many rows were claimed. Never loses a row: on any failure
/// nothing is deleted and the rows are rescheduled with backoff. Every
/// statement runs in a maintenance transaction ([`crate::maintenance::begin`]),
/// the claim commits before the platform call, and the writes after it are
/// fenced on the `(id, attempts)` the claim set, so a shipper whose claim
/// lapsed leaves the rows to their newer holder.
pub async fn ship_once(
    db: &Db,
    platform: &PlatformClient,
    cfg: &ShipperConfig,
) -> std::result::Result<usize, ShipError> {
    let mut tx = crate::maintenance::begin(db).await?;
    let mut rows: Vec<Row> = sqlx::query_as(
        "UPDATE usage_outbox SET attempts = attempts + 1, \
                next_attempt_at = now() + make_interval(secs => $2) \
         WHERE id IN ( \
             SELECT id FROM usage_outbox WHERE next_attempt_at <= now() \
             ORDER BY id LIMIT $1 FOR UPDATE SKIP LOCKED) \
         RETURNING id, event_id, org_id, user_id, tool, billable, occurred_at, attempts",
    )
    .bind(cfg.batch as i64)
    .bind(cfg.claim_lease.as_secs_f64())
    .fetch_all(&mut *tx)
    .await?;
    tx.commit().await?;
    if rows.is_empty() {
        return Ok(0);
    }
    rows.sort_by_key(|r| r.id);
    let ids: Vec<i64> = rows.iter().map(|r| r.id).collect();
    let attempts: Vec<i32> = rows.iter().map(|r| r.attempts).collect();
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
            reschedule(db, &ids, &attempts, &stored_reason(&e), cfg).await;
            return Err(e.into());
        }
    };
    let answered = receipt.accepted as usize + receipt.duplicates as usize + receipt.rejected.len();
    if answered != ids.len() {
        let msg = format!(
            "the platform accounted for {answered} of {} usage events",
            ids.len()
        );
        return Err(refuse_receipt(db, &ids, &attempts, msg, cfg).await);
    }
    // A rejected id outside this batch would be recorded as rejected while its
    // row still ships later, and a repeated id would insert two rejected rows.
    // Either receipt can pass the count check above, so the ids need their own
    // check (#63). No platform-supplied id goes in the message.
    let mut not_yet_rejected: HashSet<Uuid> = rows.iter().map(|r| r.event_id).collect();
    let mut stray = 0;
    for r in &receipt.rejected {
        if !not_yet_rejected.remove(&r.event_id) {
            stray += 1;
        }
    }
    if stray > 0 {
        let msg = format!(
            "the platform rejected {} usage events, {stray} of them not in this batch or repeated",
            receipt.rejected.len()
        );
        return Err(refuse_receipt(db, &ids, &attempts, msg, cfg).await);
    }

    let mut rejected_ids: Vec<Uuid> = Vec::with_capacity(receipt.rejected.len());
    let mut reasons: Vec<String> = Vec::with_capacity(receipt.rejected.len());
    for r in &receipt.rejected {
        // The platform's reason is its text, not ours: debug only, and kept in
        // usage_outbox_rejected for operators (#44), capped and with NUL
        // replaced so it is bounded and cannot fail the batch (#51).
        let reason = crate::telemetry::stored(&r.reason, MAX_REJECTED_REASON);
        tracing::error!(event_id = %r.event_id, reason = "rejected",
            "the platform refused a usage event; recording it in usage_outbox_rejected if this claim still holds it");
        tracing::debug!(event_id = %r.event_id, reason = "rejected", platform_reason = ?reason,
            "the platform refused a usage event; recording it in usage_outbox_rejected if this claim still holds it");
        rejected_ids.push(r.event_id);
        reasons.push(reason);
    }

    let mut tx = crate::maintenance::begin(db).await?;
    // Fenced on the attempts value this claim set: a row whose claim lapsed and
    // was taken by another shipper has a newer value, so it is neither deleted
    // nor recorded here; the newer holder does both (#67). A row is recorded as
    // rejected only if this statement deleted it, and a row can be deleted
    // once, so a rejection is recorded at most once even when two shippers
    // finish together: the later DELETE waits on the row lock, then re-checks
    // the fence against the row as the newer claim left it. That re-check is
    // how read committed, Postgres's default isolation and the one these
    // transactions use, handles a row changed under it. The join on this
    // batch's deleted rows is also a second guard on the batch boundary (#63).
    let (deleted, moved): (i64, i64) = sqlx::query_as(
        "WITH gone AS ( \
           DELETE FROM usage_outbox o \
            USING unnest($1::int8[], $2::int4[]) AS c(id, attempts) \
            WHERE o.id = c.id AND o.attempts = c.attempts \
           RETURNING o.event_id, o.org_id, o.user_id, o.tool, o.billable, o.occurred_at), \
         moved AS ( \
           INSERT INTO usage_outbox_rejected \
             (event_id, org_id, user_id, tool, billable, occurred_at, reason) \
           SELECT g.event_id, g.org_id, g.user_id, g.tool, g.billable, g.occurred_at, r.reason \
             FROM gone g JOIN unnest($3::uuid[], $4::text[]) AS r(event_id, reason) \
               ON r.event_id = g.event_id \
           RETURNING 1) \
         SELECT (SELECT count(*) FROM gone), (SELECT count(*) FROM moved)",
    )
    .bind(&ids)
    .bind(&attempts)
    .bind(&rejected_ids)
    .bind(&reasons)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    // Counts from count(*), never negative.
    let deleted = usize::try_from(deleted).unwrap_or(0);
    let moved = usize::try_from(moved).unwrap_or(0);
    if deleted < ids.len() {
        tracing::warn!(claimed = ids.len(), lost = ids.len() - deleted,
            "usage rows this shipper claimed are no longer held by its claim (reclaimed after it lapsed, or purged); left as they are");
    }
    if moved < rejected_ids.len() {
        tracing::warn!(
            rejected = rejected_ids.len(),
            recorded = moved,
            "rejected usage events are no longer held by this claim; left as they are"
        );
    }
    Ok(ids.len())
}

/// Delete rejected usage events older than [`REJECTED_RETENTION_DAYS`],
/// across every org, in a maintenance transaction ([`crate::maintenance`]),
/// which the table's unpinned policies require. Returns how many went.
pub async fn sweep_rejected(db: &Db) -> Result<u64> {
    let mut tx = crate::maintenance::begin(db).await?;
    let swept = sqlx::query(
        "DELETE FROM usage_outbox_rejected \
         WHERE rejected_at < now() - make_interval(days => $1)",
    )
    .bind(REJECTED_RETENTION_DAYS)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(swept)
}

/// Keep every row of a batch whose receipt cannot be trusted and retry it
/// with backoff; the error carries counts only, never platform text.
async fn refuse_receipt(
    db: &Db,
    ids: &[i64],
    attempts: &[i32],
    msg: String,
    cfg: &ShipperConfig,
) -> ShipError {
    tracing::error!("{msg}; keeping every row and retrying");
    let err = otto_resource::Error::Decode(msg);
    reschedule(db, ids, attempts, &stored_reason(&err), cfg).await;
    err.into()
}

async fn reschedule(db: &Db, ids: &[i64], attempts: &[i32], reason: &str, cfg: &ShipperConfig) {
    let r = async {
        let mut tx = crate::maintenance::begin(db).await?;
        // Fenced like the finish step: a lapsed claim must not overwrite the
        // schedule or last_error of a newer one (#67).
        sqlx::query(
            "UPDATE usage_outbox o SET last_error = $3, \
                    next_attempt_at = now() + make_interval(secs => \
                        LEAST($4::float8 * power(2, LEAST(o.attempts - 1, 20)), $5::float8)) \
               FROM unnest($1::int8[], $2::int4[]) AS c(id, attempts) \
              WHERE o.id = c.id AND o.attempts = c.attempts",
        )
        .bind(ids)
        .bind(attempts)
        .bind(reason)
        .bind(cfg.base_backoff.as_secs_f64())
        .bind(cfg.max_backoff.as_secs_f64())
        .execute(&mut *tx)
        .await?;
        tx.commit().await
    }
    .await;
    if let Err(e) = r {
        // The claim lease still holds the rows back; this only delays a retry.
        let f = crate::error::db_failure_reason(&e);
        tracing::warn!(
            reason = f.reason,
            sqlstate = f.sqlstate(),
            rows = ids.len(),
            "could not record a usage shipping failure"
        );
        tracing::debug!(reason = f.reason, sqlstate = f.sqlstate(), rows = ids.len(), error = %e, "could not record a usage shipping failure");
    }
}

/// What `usage_outbox.last_error` records for a failed shipment: the
/// failure's name and HTTP status (`http_status 502`), never its text,
/// which can carry a platform response body or URL (#44).
fn stored_reason(e: &otto_resource::Error) -> String {
    let f = crate::error::platform_failure_reason(e);
    match (f.reason, f.status) {
        (reason, Some(status)) => format!("{reason} {status}"),
        (reason, None) => reason.to_string(),
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
                        let f = other.failure_reason();
                        tracing::warn!(reason = f.reason, status = f.status, sqlstate = f.sqlstate(), transport = f.transport, retry_in = ?wait, "usage shipping failed");
                        tracing::debug!(reason = f.reason, status = f.status, sqlstate = f.sqlstate(), transport = f.transport, retry_in = ?wait, error = %other, "usage shipping failed");
                    }
                }
                wait
            }
        };
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = shutdown.changed() => {
                if let Err(e) = ship_once(&db, &platform, &cfg).await {
                    let f = e.failure_reason();
                    tracing::warn!(reason = f.reason, status = f.status, sqlstate = f.sqlstate(), transport = f.transport, "final usage flush failed; rows stay in the outbox");
                    tracing::debug!(reason = f.reason, status = f.status, sqlstate = f.sqlstate(), transport = f.transport, error = %e, "final usage flush failed; rows stay in the outbox");
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
    fn the_stored_reason_names_the_failure_without_its_text() {
        let status = otto_resource::Error::Status {
            status: 502,
            body: "secret".into(),
        };
        assert_eq!(stored_reason(&status), "http_status 502");
        assert_eq!(
            stored_reason(&otto_resource::Error::Unauthorized),
            "unauthorized"
        );
        assert_eq!(
            stored_reason(&otto_resource::Error::Decode("secret".into())),
            "decode"
        );
    }

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

    #[test]
    fn ship_errors_name_their_kind_without_the_text() {
        let platform = ShipError::Platform(otto_resource::Error::Status {
            status: 502,
            body: "secret".into(),
        });
        let f = platform.failure_reason();
        assert_eq!((f.reason, f.status), ("http_status", Some(502)));
        let db = ShipError::Db(sqlx::Error::Protocol("secret".into()));
        let f = db.failure_reason();
        assert_eq!((f.reason, f.status), ("protocol", None));
    }
}
