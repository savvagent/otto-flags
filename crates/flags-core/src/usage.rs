//! Recording MCP tool calls, and shipping usage to the platform. The same
//! design as otto-factory's `of-billing`, condensed:
//!
//! - **MCP tools are free.** [`Meter::charge`] writes a `usage_outbox` row in
//!   the tool's own transaction, always with `billable = false`, so a failed
//!   call rolls its record back and a successful one is recorded even while the
//!   platform is down. Nothing is looked up and nothing is refused: no tool is
//!   ever turned away for billing ([`BILLABLE`] is empty).
//! - **Billing is on production evaluations.** The SDKs' evaluations are
//!   counted and closed into hourly rows by [`crate::metering`], which ship
//!   here as metered rows (the `otto-flags/evaluations` meter).
//! - **Shipping is a background task** ([`run`]) that posts rows to the
//!   platform and deletes what it answered for. The platform dedupes on the
//!   row's `event_id`, so retries are free. Every statement the shipper issues
//!   runs in a maintenance transaction, and the writes after a claim are fenced
//!   on the `(id, attempts)` it set, so a lapsed claim cannot touch a newer
//!   one's rows.
//! - **Shipping declares first.** The platform refuses usage for a meter nobody
//!   declared, so [`run`] first declares [`crate::metering::METER_DECLARATIONS`]
//!   and ships nothing until the platform accepts them, retrying with backoff.
//!   Rows stay queued meanwhile and nothing is lost.
//! - **Refused events are kept, then swept.** An event the platform rejects
//!   outright moves to `usage_outbox_rejected` with its reason (and, for a
//!   metered row, its meter and quantity), is kept for
//!   [`REJECTED_RETENTION_DAYS`] days, and goes with its org on `org.deleted`.
//! - **A blocked org keeps being served.** [`Meter::billable_blocked`] tells
//!   the evaluation hour close whether the platform blocks billable usage; if
//!   so the hour ships unbilled. Evaluations are never refused for it either.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use crate::error::Failure;
use otto_resource::{MeterDeclaration, PlatformClient, UsageEvent, UsageStatus};
use otto_tenant::ids::{OrgId, UserId};
use otto_tenant::{Db, Tx};
use serde::Serialize;
use sqlx::FromRow;
use tokio::sync::watch;
use uuid::Uuid;

use crate::error::Result;

/// Tools that consume the plan's allowance: none. Every MCP tool is free, and
/// billing is on production evaluations ([`crate::metering`]). Kept so the
/// recorded `billable` flag and its tests have one place to say so.
pub const BILLABLE: &[&str] = &[];

/// Whether a tool call is billed. Always false today; see [`BILLABLE`].
pub fn is_billable(tool: &str) -> bool {
    BILLABLE.contains(&tool)
}

const QUOTA_LOOKUP_TIMEOUT: Duration = Duration::from_secs(2);
/// Past this share of the legacy allowance, the deprecated
/// [`Status::warning`] is true.
pub const WARN_AT: f64 = 0.8;
/// How long `usage_outbox_rejected` keeps an event the platform refused, for
/// operators reconciling usage with it (#44); [`sweep_rejected`] removes older
/// ones (#74).
pub const REJECTED_RETENTION_DAYS: i32 = 90;
/// The platform's rejection reason is its text, kept for operators: this many
/// characters are enough to read it, and bound the column.
const MAX_REJECTED_REASON: usize = 500;

/// Whether an org's evaluations are billed when its hour closes
/// ([`Meter::billable_blocked`]).
///
/// Only [`BlockedLookup::Blocked`] ships an hour unmetered; the hour close
/// bills on both [`BlockedLookup::NotBlocked`] and [`BlockedLookup::Failed`]
/// (fail open).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockedLookup {
    /// The platform says billable usage is blocked (budget exceeded or
    /// payment required).
    Blocked,
    /// The platform says billable usage is allowed.
    NotBlocked,
    /// The platform could not be asked, or did not answer in time.
    Failed,
}

#[derive(Clone)]
pub struct Meter {
    platform: Arc<PlatformClient>,
    pub upgrade_url: String,
}

impl std::fmt::Debug for Meter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Meter").finish_non_exhaustive()
    }
}

impl Meter {
    pub fn new(platform: Arc<PlatformClient>, upgrade_url: impl Into<String>) -> Self {
        Self {
            platform,
            upgrade_url: upgrade_url.into(),
        }
    }

    /// Record one tool call in `tx`, free. Never refuses, never asks the
    /// platform and never counts: the row is `billable = false`, whatever the
    /// tool ([`BILLABLE`]).
    pub async fn charge(&self, tx: &mut Tx<'_>, user: UserId, tool: &str) -> Result<()> {
        let org = tx.org();
        sqlx::query(
            "INSERT INTO usage_outbox (org_id, user_id, tool, billable) VALUES ($1,$2,$3,$4)",
        )
        .bind(org)
        .bind(user)
        .bind(tool)
        .bind(is_billable(tool))
        .execute(tx.conn())
        .await?;
        Ok(())
    }

    /// Whether the platform blocks the org's billable usage
    /// (`UsageStatus::billable_blocked`), for the evaluation meter's hour close
    /// ([`crate::metering::close_due`]). Background only: never called on a
    /// request path.
    ///
    /// The lookup is bounded by a 2 s timeout. Any
    /// failure is [`BlockedLookup::Failed`], logged at `warn` with the
    /// text-free [`Failure`] (the error text only at `debug`), and is not
    /// remembered.
    pub async fn billable_blocked(&self, org: OrgId) -> BlockedLookup {
        let failure = match tokio::time::timeout(
            QUOTA_LOOKUP_TIMEOUT,
            self.platform.usage_status(org.as_uuid()),
        )
        .await
        {
            Ok(Ok(status)) if status.billable_blocked() => return BlockedLookup::Blocked,
            Ok(Ok(_)) => return BlockedLookup::NotBlocked,
            Ok(Err(e)) => Some(e),
            Err(_) => None,
        };
        // A timeout has no text; it is named as entitlements names it.
        let f = failure.as_ref().map_or(
            Failure::reason("timeout"),
            crate::error::platform_failure_reason,
        );
        tracing::warn!(org = %org, reason = f.reason, status = f.status, transport = f.transport,
            "could not read usage from the platform; billing the org's closed evaluation hours");
        if let Some(e) = &failure {
            tracing::debug!(org = %org, reason = f.reason, status = f.status, transport = f.transport, error = %e,
                "could not read usage from the platform; billing the org's closed evaluation hours");
        }
        BlockedLookup::Failed
    }

    /// The org's standing, for the free `whoami` and `usage` tools.
    pub async fn report(&self, org: OrgId) -> Result<Status> {
        let usage = self.platform.usage_status(org.as_uuid()).await?;
        Ok(Status::new(usage))
    }
}

pub(crate) fn plan_name(plan: &str) -> String {
    let mut chars = plan.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Where an org stands: this month's meters, its budget for this service, and
/// whether the platform blocks billable usage (shared by every otto-* service
/// the org uses).
///
/// `meters`, `budget` and `blocked_reason` are the current picture. The older
/// bucket fields (`included_ops` through `enforced`) are kept so existing
/// readers keep working.
///
/// Blocked here means the platform's verdict on billable usage. It does not
/// refuse anything on this server: evaluations keep serving and are not billed
/// while it holds, and MCP tools are never refused for billing.
#[derive(Debug, Clone, PartialEq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub plan: String,
    /// This month's quantity for each meter this server bills on, such as
    /// `otto-flags/evaluations`. A meter with no usage yet shows zero.
    pub meters: Vec<MeterStatus>,
    /// The monthly budget the org set for this service and where its spend
    /// stands, or null when it has set none.
    pub budget: Option<BudgetReport>,
    /// Why billable usage is blocked right now, such as `budget_exceeded` or
    /// `payment_required`, or null when it is not. While it is set, production
    /// evaluations keep serving and are not billed; MCP tools are never
    /// refused for billing.
    pub blocked_reason: Option<String>,
    /// Deprecated: the legacy bucket. Billable operations included this month,
    /// after any negotiated override.
    pub included_ops: i64,
    /// Deprecated: see `meters`.
    pub billable_used: i64,
    /// Deprecated. Never negative: an org past its bucket has none left, not a
    /// debt.
    pub remaining: i64,
    /// Deprecated. Every recorded call this month, billable or not.
    pub total_calls: i64,
    pub period_start: chrono::NaiveDate,
    /// Deprecated. True past [`WARN_AT`] of the bucket.
    pub warning: bool,
    /// Deprecated. Whether exceeding the legacy bucket stops billable work
    /// rather than metering overage.
    pub hard_stop: bool,
    /// Deprecated. Always `true`; the platform's blocked reason is always
    /// reported and no longer switchable.
    pub enforced: bool,
}

/// One meter's usage this month.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct MeterStatus {
    /// The meter's key, `<service>/<meter>`.
    pub meter: String,
    pub display_name: String,
    pub unit: String,
    /// The sum of the quantities billed on this meter this month.
    pub quantity: u64,
}

/// The org's monthly budget for this service, in US cents.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct BudgetReport {
    pub monthly_budget_cents: i64,
    /// Spend so far this month, or null when the platform could not compute it.
    pub spend_cents: Option<i64>,
    /// Whole percent of the budget spent (can exceed 100), or null when spend
    /// is unknown.
    pub percent_used: Option<i64>,
    /// The alert thresholds the platform reports as reached this month.
    pub thresholds_reached: Vec<u8>,
    /// Whether the org opted in to blocking billable usage once spend reaches
    /// the budget.
    pub hard_stop: bool,
    /// Whether billable usage is blocked now because of the budget.
    pub blocked: bool,
    /// Why `blocked` is true (`budget_exceeded`), or why spend is unknown
    /// (`spend_unavailable`, never blocked); null otherwise.
    pub reason: Option<String>,
}

/// The code a block is reported with: `payment_required` as the platform says,
/// anything else (no reason, or one this build does not know) is
/// `budget_exceeded`, so a reader always gets a code it can branch on.
fn blocked_code(reason: Option<&str>) -> &'static str {
    match reason {
        Some(otto_resource::PAYMENT_REQUIRED) => otto_resource::PAYMENT_REQUIRED,
        _ => otto_resource::BUDGET_EXCEEDED,
    }
}

impl Status {
    fn new(u: UsageStatus) -> Self {
        let blocked_reason = if u.billable_blocked() {
            Some(blocked_code(u.blocked_reason.as_deref()).to_string())
        } else {
            None
        };
        let remaining = u.included_ops.saturating_sub(u.billable_count).max(0);
        let warning =
            u.included_ops <= 0 || u.billable_count as f64 >= u.included_ops as f64 * WARN_AT;
        Self {
            plan: plan_name(&u.plan),
            meters: u
                .meters
                .into_iter()
                .map(|m| MeterStatus {
                    meter: m.meter,
                    display_name: m.display_name,
                    unit: m.unit,
                    quantity: m.quantity,
                })
                .collect(),
            budget: u.budget.map(|b| BudgetReport {
                monthly_budget_cents: b.monthly_budget_cents,
                spend_cents: b.spend_cents,
                percent_used: b.percent_used,
                thresholds_reached: b.thresholds_reached,
                hard_stop: b.hard_stop,
                blocked: b.blocked,
                reason: b.reason,
            }),
            blocked_reason,
            included_ops: u.included_ops,
            billable_used: u.billable_count,
            remaining,
            total_calls: u.total_count,
            period_start: u.period_start,
            warning,
            hard_stop: u.hard_stop,
            enforced: true,
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
    meter: Option<String>,
    quantity: Option<i64>,
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
         RETURNING id, event_id, org_id, user_id, tool, billable, occurred_at, attempts, \
                   meter, quantity",
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
            service: None,
            meter: r.meter.clone(),
            // The CHECK keeps quantity positive; a value that still fails the
            // conversion is sent as 0, which the platform refuses per event
            // (`invalid_quantity`), so the row is dead-lettered, not lost.
            quantity: r.quantity.map(|q| u64::try_from(q).unwrap_or(0)),
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
           RETURNING o.event_id, o.org_id, o.user_id, o.tool, o.billable, o.occurred_at, \
                      o.meter, o.quantity), \
         moved AS ( \
           INSERT INTO usage_outbox_rejected \
             (event_id, org_id, user_id, tool, billable, occurred_at, meter, quantity, reason) \
           SELECT g.event_id, g.org_id, g.user_id, g.tool, g.billable, g.occurred_at, \
                  g.meter, g.quantity, r.reason \
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
/// which the table's unpinned policies require. Returns the number of rows
/// deleted.
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

/// How often a stuck meter declaration is logged.
const DECLARE_LOG_EVERY: Duration = Duration::from_secs(60);

/// Why a declaration attempt did not finish the job, for the log: a text-free
/// [`Failure`] always, and the platform's own words (its error text, or each
/// rejected meter's reason) only for the `debug` line.
fn declaration_failure(
    result: std::result::Result<otto_resource::MeterDeclarationReceipt, otto_resource::Error>,
) -> Option<(Failure, String)> {
    match result {
        Ok(receipt) if receipt.is_complete() => None,
        Ok(receipt) => {
            let text = receipt
                .rejected
                .iter()
                .map(|r| format!("{} refused: {}", r.key, r.reason))
                .collect::<Vec<_>>()
                .join("; ");
            Some((Failure::reason("meter_rejected"), text))
        }
        Err(e) => Some((crate::error::platform_failure_reason(&e), e.to_string())),
    }
}

/// Rows waiting in `usage_outbox`, for the declaration log.
async fn waiting(db: &Db) -> std::result::Result<i64, sqlx::Error> {
    let mut tx = crate::maintenance::begin(db).await?;
    let n = sqlx::query_scalar("SELECT count(*) FROM usage_outbox")
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(n)
}

/// Declare [`crate::metering::METER_DECLARATIONS`] with the platform, retrying
/// with capped exponential backoff ([`ShipperConfig::base_backoff`] to
/// [`ShipperConfig::max_backoff`]) until the receipt is complete. Returns
/// `false` if `shutdown` came first.
///
/// A transport error, a 5xx, a `409 service_not_set`, or a meter in `rejected`
/// all retry. Nothing ships meanwhile and the outbox keeps every row, so a
/// platform that cannot take the declaration delays billing and loses none. On
/// the first failure, then once a minute, it logs at `error` with a text-free
/// reason and the number of rows waiting; the platform's text is `debug` only.
async fn declare_meters(
    db: &Db,
    platform: &PlatformClient,
    cfg: &ShipperConfig,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    let declarations: Vec<MeterDeclaration> = crate::metering::METER_DECLARATIONS
        .iter()
        .map(|(key, display, unit)| MeterDeclaration::new(*key, *display, *unit))
        .collect();
    let mut attempt: u32 = 0;
    let mut last_logged: Option<tokio::time::Instant> = None;
    loop {
        if *shutdown.borrow() {
            return false;
        }
        let result = tokio::select! {
            r = platform.declare_meters(&declarations) => r,
            _ = shutdown.changed() => return false,
        };
        let Some((f, text)) = declaration_failure(result) else {
            tracing::info!(meters = declarations.len(), "declared usage meters");
            return true;
        };
        if last_logged.is_none_or(|t| t.elapsed() >= DECLARE_LOG_EVERY) {
            last_logged = Some(tokio::time::Instant::now());
            let rows_waiting = match waiting(db).await {
                Ok(n) => Some(n),
                Err(e) => {
                    let f = crate::error::db_failure_reason(&e);
                    tracing::debug!(reason = f.reason, sqlstate = f.sqlstate(), error = %e, "could not count the usage rows waiting");
                    None
                }
            };
            tracing::error!(
                reason = f.reason,
                status = f.status,
                transport = f.transport,
                ?rows_waiting,
                "could not declare the usage meters; no usage ships until they are declared \
                 (rows stay queued and nothing is lost). Retrying"
            );
            tracing::debug!(
                reason = f.reason, status = f.status, transport = f.transport, platform_text = %text,
                "could not declare the usage meters"
            );
        }
        let wait = cfg
            .base_backoff
            .saturating_mul(1u32 << attempt.min(20))
            .min(cfg.max_backoff);
        attempt = attempt.saturating_add(1);
        tokio::select! {
            _ = tokio::time::sleep(wait) => {}
            _ = shutdown.changed() => return false,
        }
    }
}

/// Declare the meters, then drain the outbox until `shutdown` flips, with a
/// final pass on the way out.
///
/// Nothing ships until the meters are declared ([`declare_meters`]): a metered
/// event for an undeclared meter would be rejected and dead-lettered. If
/// shutdown arrives while declaring, this returns at once, without the final
/// pass; the rows stay queued for the next process.
pub async fn run(
    db: Db,
    platform: Arc<PlatformClient>,
    cfg: ShipperConfig,
    mut shutdown: watch::Receiver<bool>,
) {
    if !declare_meters(&db, &platform, &cfg, &mut shutdown).await {
        return;
    }
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
    fn nothing_is_billable() {
        assert!(BILLABLE.is_empty());
        for t in [
            "create_flag",
            "set_flag_environment",
            "rollback_flag",
            "create_app",
            "set_rollback_policy",
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

    fn usage_status(json: serde_json::Value) -> UsageStatus {
        let mut base = serde_json::json!({
            "org_id": Uuid::nil(),
            "plan": "team",
            "period_start": "2026-10-01",
            "billable_count": 30,
            "total_count": 40,
            "included_ops": 100,
            "hard_stop": false,
        });
        base.as_object_mut()
            .expect("object")
            .extend(json.as_object().expect("object").clone());
        serde_json::from_value(base).expect("usage status")
    }

    #[test]
    fn status_carries_the_meters() {
        let s = Status::new(usage_status(serde_json::json!({
            "meters": [{
                "meter": "otto-flags/evaluations",
                "display_name": "Evaluations",
                "unit": "evaluation",
                "quantity": 1200,
            }],
        })));
        assert_eq!(s.plan, "Team");
        assert_eq!(
            s.meters,
            vec![MeterStatus {
                meter: "otto-flags/evaluations".into(),
                display_name: "Evaluations".into(),
                unit: "evaluation".into(),
                quantity: 1200,
            }]
        );
        assert_eq!(s.budget, None);
        assert_eq!(s.blocked_reason, None);
        assert!(s.enforced);
        // The legacy fields keep their meaning.
        assert_eq!(
            (s.included_ops, s.billable_used, s.remaining, s.total_calls),
            (100, 30, 70, 40)
        );
        assert!(!s.warning && !s.hard_stop);
    }

    #[test]
    fn status_carries_the_budget_with_its_reason() {
        let s = Status::new(usage_status(serde_json::json!({
            "budget": {
                "monthly_budget_cents": 5000,
                "spend_cents": null,
                "percent_used": null,
                "thresholds_reached": [50],
                "hard_stop": true,
                "blocked": false,
                "reason": "spend_unavailable",
            },
        })));
        assert_eq!(
            s.budget,
            Some(BudgetReport {
                monthly_budget_cents: 5000,
                spend_cents: None,
                percent_used: None,
                thresholds_reached: vec![50],
                hard_stop: true,
                blocked: false,
                reason: Some("spend_unavailable".into()),
            })
        );
        assert_eq!(s.blocked_reason, None);
    }

    #[test]
    fn status_blocked_reason_follows_the_platform_or_falls_back() {
        let platform = Status::new(usage_status(
            serde_json::json!({"blocked_reason": "payment_required"}),
        ));
        assert_eq!(platform.blocked_reason.as_deref(), Some("payment_required"));
        // Blocked by the budget with no reason given: still a code.
        let fallback = Status::new(usage_status(serde_json::json!({
            "budget": {
                "monthly_budget_cents": 100,
                "spend_cents": 100,
                "percent_used": 100,
                "thresholds_reached": [50, 80, 100],
                "hard_stop": true,
                "blocked": true,
            },
        })));
        assert_eq!(fallback.blocked_reason.as_deref(), Some("budget_exceeded"));
        // An unknown reason from a newer platform is still blocked, and still
        // reported as a code the reader knows.
        let unknown = Status::new(usage_status(serde_json::json!({"blocked_reason": "x"})));
        assert_eq!(unknown.blocked_reason.as_deref(), Some("budget_exceeded"));
    }

    /// A platform that answers every request with `status` and `body`.
    async fn platform_answering(status: u16, body: String) -> Arc<PlatformClient> {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = sock.read(&mut buf).await;
                let res = format!(
                    "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\n\
                     content-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(res.as_bytes()).await;
            }
        });
        let cfg = otto_resource::ClientConfig::new(
            format!("http://{addr}"),
            "https://flags.test/mcp",
            "secret",
        );
        Arc::new(PlatformClient::new(cfg).expect("client"))
    }

    fn usage_body(org: OrgId, blocked_reason: Option<&str>) -> String {
        serde_json::json!({
            "org_id": org.as_uuid(),
            "plan": "team",
            "period_start": "2026-10-01",
            "billable_count": 0,
            "total_count": 0,
            "included_ops": 0,
            "hard_stop": false,
            "blocked_reason": blocked_reason,
        })
        .to_string()
    }

    #[tokio::test]
    async fn billable_blocked_follows_the_platform() {
        let org = OrgId::new();
        let blocked = Meter::new(
            platform_answering(200, usage_body(org, Some("payment_required"))).await,
            "",
        );
        assert_eq!(blocked.billable_blocked(org).await, BlockedLookup::Blocked);
        let open = Meter::new(platform_answering(200, usage_body(org, None)).await, "");
        assert_eq!(open.billable_blocked(org).await, BlockedLookup::NotBlocked);
    }

    #[tokio::test]
    async fn billable_blocked_is_failed_when_the_platform_cannot_answer() {
        let org = OrgId::new();
        let erroring = Meter::new(platform_answering(502, "{}".into()).await, "");
        assert_eq!(erroring.billable_blocked(org).await, BlockedLookup::Failed);
        // Nothing listens on the port.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("bind")
            .local_addr()
            .expect("addr")
            .port();
        let cfg = otto_resource::ClientConfig::new(
            format!("http://127.0.0.1:{port}"),
            "https://flags.test/mcp",
            "secret",
        );
        let unreachable = Meter::new(Arc::new(PlatformClient::new(cfg).expect("client")), "");
        assert_eq!(
            unreachable.billable_blocked(org).await,
            BlockedLookup::Failed
        );
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
