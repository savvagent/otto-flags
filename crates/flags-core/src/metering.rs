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
//! statement inside a maintenance transaction.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use otto_tenant::ids::OrgId;
use otto_tenant::Db;
use sqlx::{Acquire, PgConnection, Postgres, Transaction};
use uuid::Uuid;

use crate::error::Result;
use crate::ids::FlagAppId;

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
    let mut tx = crate::maintenance::begin(db).await?;
    for setting in ["statement_timeout", "idle_in_transaction_session_timeout"] {
        sqlx::query("SELECT set_config($1, $2, true)")
            .bind(setting)
            .bind(timeout)
            .execute(&mut *tx)
            .await?;
    }
    if !pause.is_zero() {
        tokio::time::sleep(pause).await;
    }
    let written = upsert_all(&mut tx, &rows).await?;
    tx.commit().await?;
    Ok(written)
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
