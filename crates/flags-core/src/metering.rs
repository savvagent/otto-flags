//! Metering production evaluations (#140).
//!
//! The SDK evaluation surface is counted in memory and flushed to an hourly
//! table; a later pass closes finished hours into the usage outbox.
//!
//! Two sources feed one (org, app) pair:
//!
//! - **served**: successful answers from the server's evaluate endpoint.
//! - **reported**: evaluation events accepted from the SDKs' telemetry.
//!
//! The billed quantity for an hour is `GREATEST(served, reported)`, never the
//! sum. With telemetry on, `reported` already includes every served
//! evaluation (plus local and cached ones), so adding would double count; with
//! telemetry off, the bill falls back to the work the server did. See
//! `docs/specs/2026-10-20-meter-production-evaluations-design.md`, Decisions.
//!
//! [`EvalCounter`] does no I/O and holds its lock for one map operation, so
//! the SDK request path never waits on the database or the platform.
//! [`flush`] moves its contents into `usage_meter_hourly` in one set-based
//! statement inside a maintenance transaction. [`close_due`] then turns each
//! finished hour into usage outbox rows: metered while the org is billed,
//! unmetered while the platform blocks it.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use otto_tenant::ids::OrgId;
use otto_tenant::Db;
use sqlx::{Acquire, PgConnection, Postgres, Transaction};
use uuid::Uuid;

use crate::error::Result;
use crate::ids::FlagAppId;
use crate::usage::{BlockedLookup, Meter};
use std::sync::Arc;
use tokio::sync::watch;
use tokio::time::Instant;

/// How often the background task flushes the counter.
pub const FLUSH_EVERY: Duration = Duration::from_secs(10);
/// How often the background task looks for finished hours to close.
pub const CLOSE_EVERY: Duration = Duration::from_secs(60);
/// How long after an hour ends it is closed, so late flushes still land in it.
pub const CLOSE_GRACE: Duration = Duration::from_secs(5 * 60);
/// The most orgs one close pass takes.
pub const CLOSE_BATCH_ORGS: i64 = 100;
/// `statement_timeout` and `idle_in_transaction_session_timeout` for every
/// flush and close transaction. The server enforces both, so they hold even
/// when the machine is suspended mid-transaction.
pub const TX_TIMEOUT: &str = "30s";
/// The meter key declared to the platform.
pub const METER_EVALUATIONS: &str = "otto-flags/evaluations";
/// `(key, display name, unit)` for the platform's meter declaration.
pub const METER_DECLARATIONS: &[(&str, &str, &str)] =
    &[(METER_EVALUATIONS, "Flag evaluations", "evaluation")];
/// The outbox `tool` an hour's evaluations are recorded under.
pub const EVALUATIONS_TOOL: &str = "sdk_evaluations";

/// Evaluations counted for one (org, app) since the last flush.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    /// Successful answers from the evaluate endpoint.
    pub served: u64,
    /// Evaluation events accepted from SDK telemetry.
    pub reported: u64,
}

impl Counts {
    fn is_zero(&self) -> bool {
        self.served == 0 && self.reported == 0
    }
}

/// In-memory evaluation counts, shared as `Arc<EvalCounter>`.
///
/// Counts saturate at `u64::MAX`. No method does I/O. A poisoned lock is
/// recovered, never a panic on the request path.
///
/// ```
/// use flags_core::ids::FlagAppId;
/// use flags_core::metering::EvalCounter;
/// use otto_tenant::ids::OrgId;
///
/// let counter = EvalCounter::default();
/// let (org, app) = (OrgId::new(), FlagAppId::new());
/// counter.served(org, app, 2);
/// counter.reported(org, app, 5);
/// let taken = counter.take();
/// assert_eq!(taken[&(org, app)].served, 2);
/// assert_eq!(taken[&(org, app)].reported, 5);
/// assert!(counter.take().is_empty());
/// ```
#[derive(Debug, Default)]
pub struct EvalCounter {
    counts: Mutex<HashMap<(OrgId, FlagAppId), Counts>>,
}

impl EvalCounter {
    fn lock(&self) -> MutexGuard<'_, HashMap<(OrgId, FlagAppId), Counts>> {
        self.counts.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Count `n` successful answers from the evaluate endpoint.
    pub fn served(&self, org: OrgId, app: FlagAppId, n: u64) {
        let mut map = self.lock();
        let c = map.entry((org, app)).or_default();
        c.served = c.served.saturating_add(n);
    }

    /// Count `n` accepted telemetry evaluation events.
    pub fn reported(&self, org: OrgId, app: FlagAppId, n: u64) {
        let mut map = self.lock();
        let c = map.entry((org, app)).or_default();
        c.reported = c.reported.saturating_add(n);
    }

    /// The total of every count held, served and reported, for a log line.
    pub fn pending(&self) -> u64 {
        self.lock().values().fold(0u64, |t, c| {
            t.saturating_add(c.served).saturating_add(c.reported)
        })
    }

    /// Take every count, leaving the counter empty.
    pub fn take(&self) -> HashMap<(OrgId, FlagAppId), Counts> {
        std::mem::take(&mut *self.lock())
    }

    /// Add counts a failed flush took back, keeping whatever was counted since.
    pub fn restore(&self, taken: HashMap<(OrgId, FlagAppId), Counts>) {
        let mut map = self.lock();
        for (key, back) in taken {
            let c = map.entry(key).or_default();
            c.served = c.served.saturating_add(back.served);
            c.reported = c.reported.saturating_add(back.reported);
        }
    }
}

/// One set-based upsert. The `NOT EXISTS` drops tombstoned orgs. Existing rows
/// are added to with a numeric clamp so a saturated hour cannot overflow the
/// `bigint`.
///
/// `flag_apps` is row-level secured on the pinned org, so an unpinned
/// maintenance transaction cannot join it to drop pairs whose app is gone. The
/// composite foreign key does that check instead (foreign-key checks bypass
/// row-level security): see [`upsert_all`] for how a violation is handled.
const UPSERT: &str = "\
INSERT INTO usage_meter_hourly (org_id, app_id, hour, served, reported) \
SELECT u.org_id, u.app_id, date_trunc('hour', now(), 'UTC'), u.served, u.reported \
FROM unnest($1::uuid[], $2::uuid[], $3::bigint[], $4::bigint[]) AS u(org_id, app_id, served, reported) \
WHERE NOT EXISTS (SELECT 1 FROM deleted_orgs d WHERE d.org_id = u.org_id) \
ON CONFLICT (org_id, app_id, hour) DO UPDATE SET \
  served = LEAST(usage_meter_hourly.served::numeric + EXCLUDED.served, 9223372036854775807)::bigint, \
  reported = LEAST(usage_meter_hourly.reported::numeric + EXCLUDED.reported, 9223372036854775807)::bigint";

/// SQLSTATE `foreign_key_violation`.
const FK_VIOLATION: &str = "23503";

/// Write the counter's contents to the current hour's rows.
///
/// Returns the number of (org, app) rows written (inserted or added to);
/// pairs with both counts zero, tombstoned orgs and missing apps are not
/// written and not counted. On any error the taken counts are restored to
/// `counter` and the error is returned.
///
/// **Not cancel-safe.** The taken counts live only in this future until it
/// commits or restores them, so dropping it mid-flight (an outer
/// `tokio::time::timeout`, a `select!` branch losing) loses them silently.
/// Bound a flush with the server-side timeout of [`flush_with`] instead, and
/// always drive it to completion. A commit whose acknowledgement is lost is
/// restored and written again on the next flush: a rare, bounded overcount
/// that only idempotency keys could remove.
pub async fn flush(db: &Db, counter: &EvalCounter) -> Result<usize> {
    flush_with(db, counter, TX_TIMEOUT, Duration::ZERO).await
}

/// [`flush`] with the transaction timeout and a pause between the `SET LOCAL`s
/// and the upsert, so a test can leave the transaction idle past its timeout.
#[doc(hidden)]
pub async fn flush_with(
    db: &Db,
    counter: &EvalCounter,
    timeout: &str,
    pause: Duration,
) -> Result<usize> {
    let mut taken = counter.take();
    taken.retain(|_, c| !c.is_zero());
    if taken.is_empty() {
        return Ok(0);
    }
    match write(db, &taken, timeout, pause).await {
        Ok(n) => Ok(n),
        Err(e) => {
            counter.restore(taken);
            Err(e.into())
        }
    }
}

fn clamp(n: u64) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

async fn write(
    db: &Db,
    taken: &HashMap<(OrgId, FlagAppId), Counts>,
    timeout: &str,
    pause: Duration,
) -> sqlx::Result<usize> {
    let rows: Vec<Row> = taken
        .iter()
        .map(|(&(org, app), c)| Row {
            org: org.as_uuid(),
            app: app.as_uuid(),
            served: clamp(c.served),
            reported: clamp(c.reported),
        })
        .collect();
    let mut tx = begin_bounded(db, timeout).await?;
    if !pause.is_zero() {
        tokio::time::sleep(pause).await;
    }
    let written = upsert_all(&mut tx, &rows).await?;
    tx.commit().await?;
    Ok(written)
}

/// A maintenance transaction whose `statement_timeout` and
/// `idle_in_transaction_session_timeout` are `timeout` (`SET LOCAL`, so they
/// end with it).
async fn begin_bounded(db: &Db, timeout: &str) -> sqlx::Result<Transaction<'static, Postgres>> {
    let mut tx = crate::maintenance::begin(db).await?;
    for setting in ["statement_timeout", "idle_in_transaction_session_timeout"] {
        sqlx::query("SELECT set_config($1, $2, true)")
            .bind(setting)
            .bind(timeout)
            .execute(&mut *tx)
            .await?;
    }
    Ok(tx)
}

struct Row {
    org: Uuid,
    app: Uuid,
    served: i64,
    reported: i64,
}

async fn upsert(conn: &mut PgConnection, rows: &[Row]) -> sqlx::Result<usize> {
    let done = sqlx::query(UPSERT)
        .bind(rows.iter().map(|r| r.org).collect::<Vec<_>>())
        .bind(rows.iter().map(|r| r.app).collect::<Vec<_>>())
        .bind(rows.iter().map(|r| r.served).collect::<Vec<_>>())
        .bind(rows.iter().map(|r| r.reported).collect::<Vec<_>>())
        .execute(conn)
        .await?;
    Ok(usize::try_from(done.rows_affected()).unwrap_or(usize::MAX))
}

/// Write every row in one statement. If one names an app that no longer
/// exists, the foreign key refuses the whole statement; that is rare (an app
/// deleted between a count and its flush), so only then is the batch retried
/// one row at a time, each in a savepoint, dropping the rows the foreign key
/// refuses. Any other error fails the flush.
async fn upsert_all(tx: &mut Transaction<'static, Postgres>, rows: &[Row]) -> sqlx::Result<usize> {
    let mut sp = tx.begin().await?;
    match upsert(&mut sp, rows).await {
        Ok(n) => {
            sp.commit().await?;
            return Ok(n);
        }
        Err(e) if is_fk_violation(&e) => sp.rollback().await?,
        Err(e) => return Err(e),
    }
    let mut written = 0;
    for row in rows {
        let mut sp = tx.begin().await?;
        match upsert(&mut sp, std::slice::from_ref(row)).await {
            Ok(n) => {
                sp.commit().await?;
                written += n;
            }
            Err(e) if is_fk_violation(&e) => sp.rollback().await?,
            Err(e) => return Err(e),
        }
    }
    Ok(written)
}

fn is_fk_violation(e: &sqlx::Error) -> bool {
    matches!(e, sqlx::Error::Database(d) if d.code().as_deref() == Some(FK_VIOLATION))
}

// ------------------------------------------------------------------ hour close

/// The orgs with a due hour, oldest due hour first, at most `$2` of them. An
/// hour `H` is due once `H + 1 h + CLOSE_GRACE <= now()`; written as a bound
/// on `hour` so the `hour` index serves it.
const DUE_ORGS: &str = "\
SELECT org_id FROM usage_meter_hourly \
WHERE hour <= now() - interval '1 hour' - make_interval(secs => $1) \
GROUP BY org_id ORDER BY min(hour), org_id LIMIT $2";

/// Close the due rows of the orgs `$1` in one statement: the `DELETE …
/// RETURNING` takes each row once, so a concurrent pass waiting on the same
/// rows finds them gone and closes nothing twice.
///
/// - `$2`: the orgs (a subset of `$1`) whose hours ship unmetered, one
///   `billable = false` row per closed hour row.
/// - Every other org's row is billed `GREATEST(served, reported)` on
///   `$3` (the meter), split into rows of at most `$4` that sum to it.
/// - A row with nothing to bill (both counts zero) is deleted with no outbox
///   row.
///
/// Returns the rows closed and the billed and unbilled totals, for the log.
const CLOSE: &str = "\
WITH gone AS ( \
  DELETE FROM usage_meter_hourly \
  WHERE org_id = ANY($1::uuid[]) \
    AND hour <= now() - interval '1 hour' - make_interval(secs => $5) \
  RETURNING org_id, hour, GREATEST(served, reported) AS q, org_id = ANY($2::uuid[]) AS blocked \
), shipped AS ( \
  INSERT INTO usage_outbox (org_id, user_id, tool, billable, occurred_at, meter, quantity) \
  SELECT g.org_id, NULL::uuid, $6::text, false, g.hour, NULL::text, NULL::bigint \
  FROM gone g WHERE g.blocked AND g.q > 0 \
  UNION ALL \
  SELECT g.org_id, NULL::uuid, $6::text, true, g.hour, $3::text, LEAST($4::bigint, g.q - i * $4::bigint) \
  FROM gone g, generate_series(0::bigint, ceil(g.q::numeric / $4::bigint)::bigint - 1) AS i \
  WHERE NOT g.blocked AND g.q > 0 \
  RETURNING billable \
) \
SELECT \
  (SELECT count(*) FROM gone) AS closed, \
  (SELECT count(*) FROM gone WHERE NOT blocked AND q > 0) AS billed_hours, \
  (SELECT coalesce(sum(q), 0)::numeric::text FROM gone WHERE NOT blocked) AS billed_quantity, \
  (SELECT count(*) FROM shipped WHERE billable) AS billed_events, \
  (SELECT count(*) FROM gone WHERE blocked AND q > 0) AS unbilled_hours, \
  (SELECT coalesce(sum(q), 0)::numeric::text FROM gone WHERE blocked) AS unbilled_quantity";

/// Close every due hour of up to [`CLOSE_BATCH_ORGS`] orgs into the usage
/// outbox.
///
/// `lookup` says whether each org is blocked; it is called in sequence, with
/// no database connection held. Only [`BlockedLookup::Blocked`] ships an
/// org's hours unmetered (`billable = false`, no meter): both
/// [`BlockedLookup::NotBlocked`] and [`BlockedLookup::Failed`] bill (fail
/// open). After the first `Failed`, `lookup` is not called again in this
/// pass and the remaining orgs are billed, so a platform outage costs one
/// lookup timeout per pass.
///
/// A billed (org, app, hour) row becomes `GREATEST(served, reported)` on
/// [`METER_EVALUATIONS`] under the [`EVALUATIONS_TOOL`] tool, at
/// `occurred_at = hour`, split across several rows if it exceeds
/// [`otto_resource::MAX_METER_QUANTITY`]. A row with both counts zero is
/// deleted without an outbox row.
///
/// Returns the number of hourly rows closed (deleted), including the ones
/// with nothing to bill. A failure rolls the close back: the rows stay and
/// are closed by a later pass. Concurrent passes (two machines) close each
/// row exactly once.
///
/// In production `lookup` is `|org| meter.billable_blocked(org)`
/// ([`crate::usage::Meter::billable_blocked`]); never call this on a request
/// path.
pub async fn close_due<F, Fut>(db: &Db, lookup: F) -> Result<usize>
where
    F: FnMut(OrgId) -> Fut,
    Fut: Future<Output = BlockedLookup>,
{
    let cap = i64::try_from(otto_resource::MAX_METER_QUANTITY).unwrap_or(i64::MAX);
    close_due_with(db, lookup, cap).await
}

/// [`close_due`] with the largest quantity one outbox row may carry, so a
/// test can exercise the split with small numbers. `cap` below 1 is taken as 1.
#[doc(hidden)]
pub async fn close_due_with<F, Fut>(db: &Db, mut lookup: F, cap: i64) -> Result<usize>
where
    F: FnMut(OrgId) -> Fut,
    Fut: Future<Output = BlockedLookup>,
{
    let cap = cap.max(1);
    let grace = CLOSE_GRACE.as_secs_f64();

    let mut tx = begin_bounded(db, TX_TIMEOUT).await?;
    let orgs: Vec<Uuid> = sqlx::query_scalar(DUE_ORGS)
        .bind(grace)
        .bind(CLOSE_BATCH_ORGS)
        .fetch_all(&mut *tx)
        .await?;
    tx.commit().await?;
    if orgs.is_empty() {
        return Ok(0);
    }

    // No connection is held across the platform calls.
    let mut blocked = Vec::new();
    let mut skipped = 0usize;
    let mut failed = false;
    for &org in &orgs {
        if failed {
            skipped += 1;
            continue;
        }
        match lookup(OrgId(org)).await {
            BlockedLookup::Blocked => blocked.push(org),
            BlockedLookup::NotBlocked => {}
            BlockedLookup::Failed => failed = true,
        }
    }

    let mut tx = begin_bounded(db, TX_TIMEOUT).await?;
    let totals: CloseTotals = sqlx::query_as(CLOSE)
        .bind(&orgs)
        .bind(&blocked)
        .bind(METER_EVALUATIONS)
        .bind(cap)
        .bind(grace)
        .bind(EVALUATIONS_TOOL)
        .fetch_one(&mut *tx)
        .await?;
    tx.commit().await?;

    tracing::info!(
        orgs = orgs.len(),
        blocked_orgs = blocked.len(),
        lookups_skipped = skipped,
        hours_closed = totals.closed,
        billed_hours = totals.billed_hours,
        billed_evaluations = %totals.billed_quantity,
        billed_events = totals.billed_events,
        unbilled_hours = totals.unbilled_hours,
        unbilled_evaluations = %totals.unbilled_quantity,
        "closed evaluation hours into the usage outbox"
    );
    Ok(usize::try_from(totals.closed).unwrap_or(usize::MAX))
}

/// What one close statement did. The quantities are summed as `numeric`, so
/// a pass over saturated rows cannot overflow, and are read (and logged) as
/// their decimal text.
#[derive(sqlx::FromRow)]
struct CloseTotals {
    closed: i64,
    billed_hours: i64,
    billed_quantity: String,
    billed_events: i64,
    unbilled_hours: i64,
    unbilled_quantity: String,
}

/// The server-side bound on the final flush at shutdown.
const FINAL_FLUSH_TIMEOUT: &str = "5s";
/// The longest a failing pass waits before trying again.
const MAX_BACKOFF: Duration = Duration::from_secs(60);

/// How long to wait after `failures` failures in a row: the pass's own period,
/// doubled each time, capped at [`MAX_BACKOFF`].
fn backoff(every: Duration, failures: u32) -> Duration {
    if failures == 0 {
        return every;
    }
    every
        .saturating_mul(1u32 << failures.min(6))
        .min(MAX_BACKOFF.max(every))
}

/// Flush the counter every [`FLUSH_EVERY`] and close finished hours every
/// [`CLOSE_EVERY`] until `shutdown`, then flush once more.
///
/// Background only: it asks the platform (through `meter`) about orgs, which
/// the SDK request path never does. Failures are logged without their text
/// (the text only at `debug`) and retried with backoff; the counts of a failed
/// flush stay in `counter` ([`flush`] restores them).
///
/// `flush` is not cancel-safe, so no flush or close sits in a `select!`: only
/// the wait does, and the work runs after it, to completion. The final flush
/// is bounded by the server-side timeout of [`flush_with`], not by an outer
/// future timeout.
pub async fn run(
    db: Db,
    meter: Meter,
    counter: Arc<EvalCounter>,
    mut shutdown: watch::Receiver<bool>,
) {
    let (mut flush_failures, mut close_failures) = (0u32, 0u32);
    let now = Instant::now();
    let (mut next_flush, mut next_close) = (now + FLUSH_EVERY, now + CLOSE_EVERY);
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(next_flush.min(next_close)) => {}
            _ = shutdown.changed() => break,
        }
        let now = Instant::now();
        if now >= next_flush {
            match flush(&db, &counter).await {
                Ok(_) => flush_failures = 0,
                Err(e) => {
                    flush_failures = flush_failures.saturating_add(1);
                    let retry_in = backoff(FLUSH_EVERY, flush_failures);
                    let f = e.failure_reason();
                    tracing::warn!(
                        reason = f.reason,
                        sqlstate = f.sqlstate(),
                        transport = f.transport,
                        ?retry_in,
                        "flushing evaluation counts failed; they are kept and retried"
                    );
                    tracing::debug!(reason = f.reason, sqlstate = f.sqlstate(), transport = f.transport, ?retry_in, error = %e, "flushing evaluation counts failed");
                }
            }
            next_flush = Instant::now() + backoff(FLUSH_EVERY, flush_failures);
        }
        if now >= next_close {
            let lookup = |org| {
                let meter = meter.clone();
                async move { meter.billable_blocked(org).await }
            };
            match close_due(&db, lookup).await {
                Ok(_) => close_failures = 0,
                Err(e) => {
                    close_failures = close_failures.saturating_add(1);
                    let retry_in = backoff(CLOSE_EVERY, close_failures);
                    let f = e.failure_reason();
                    tracing::warn!(
                        reason = f.reason,
                        sqlstate = f.sqlstate(),
                        transport = f.transport,
                        ?retry_in,
                        "closing evaluation hours failed; they are retried"
                    );
                    tracing::debug!(reason = f.reason, sqlstate = f.sqlstate(), transport = f.transport, ?retry_in, error = %e, "closing evaluation hours failed");
                }
            }
            next_close = Instant::now() + backoff(CLOSE_EVERY, close_failures);
        }
    }
    if let Err(e) = flush_with(&db, &counter, FINAL_FLUSH_TIMEOUT, Duration::ZERO).await {
        let f = e.failure_reason();
        tracing::warn!(
            reason = f.reason,
            sqlstate = f.sqlstate(),
            transport = f.transport,
            "final flush of evaluation counts failed"
        );
        tracing::debug!(reason = f.reason, sqlstate = f.sqlstate(), transport = f.transport, error = %e, "final flush of evaluation counts failed");
    }
    let lost = counter.pending();
    if lost > 0 {
        tracing::warn!(
            evaluations = lost,
            "evaluation counts were not written before shutdown"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> (OrgId, FlagAppId) {
        (OrgId::new(), FlagAppId::new())
    }

    #[test]
    fn served_and_reported_accumulate_per_org_and_app() {
        let c = EvalCounter::default();
        let (o1, a1) = ids();
        let (o2, a2) = ids();
        c.served(o1, a1, 2);
        c.served(o1, a1, 3);
        c.reported(o1, a1, 7);
        c.reported(o2, a2, 1);
        let m = c.take();
        assert_eq!(m.len(), 2);
        assert_eq!(
            m[&(o1, a1)],
            Counts {
                served: 5,
                reported: 7
            }
        );
        assert_eq!(
            m[&(o2, a2)],
            Counts {
                served: 0,
                reported: 1
            }
        );
    }

    #[test]
    fn take_empties_the_counter() {
        let c = EvalCounter::default();
        let (o, a) = ids();
        c.served(o, a, 1);
        assert_eq!(c.take().len(), 1);
        assert!(c.take().is_empty());
    }

    #[test]
    fn restore_adds_back_without_losing_concurrent_adds() {
        let c = EvalCounter::default();
        let (o, a) = ids();
        c.served(o, a, 4);
        let taken = c.take();
        c.served(o, a, 1);
        c.reported(o, a, 2);
        c.restore(taken);
        assert_eq!(
            c.take()[&(o, a)],
            Counts {
                served: 5,
                reported: 2
            }
        );
    }

    #[test]
    fn counts_saturate_at_u64_max() {
        let c = EvalCounter::default();
        let (o, a) = ids();
        c.served(o, a, u64::MAX);
        c.served(o, a, 5);
        c.reported(o, a, u64::MAX - 1);
        c.reported(o, a, 9);
        let taken = c.take();
        assert_eq!(taken[&(o, a)].served, u64::MAX);
        assert_eq!(taken[&(o, a)].reported, u64::MAX);
        c.served(o, a, u64::MAX);
        c.restore(taken);
        assert_eq!(c.take()[&(o, a)].served, u64::MAX);
    }

    #[test]
    fn a_poisoned_lock_recovers() {
        let c = std::sync::Arc::new(EvalCounter::default());
        let c2 = c.clone();
        let _ = std::thread::spawn(move || {
            let _g = c2.counts.lock().unwrap();
            panic!("poison");
        })
        .join();
        let (o, a) = ids();
        c.served(o, a, 1);
        assert_eq!(c.take()[&(o, a)].served, 1);
    }
}
