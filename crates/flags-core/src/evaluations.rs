//! Flag-evaluation ingestion — the raw evidence base correlation, health
//! monitoring, and risk scoring get computed from later (design doc §5).
//! Deliberately just the write path plus enough of a read path to confirm
//! ingestion worked: aggregation/rollup tables are out of scope here per
//! `migrations/0003_flag_evaluations.sql`'s comment.

use crate::error::Result;
use crate::ids::{EvaluationId, FlagAppId, FlagId};
use otto_tenant::ids::OrgId;
use otto_tenant::Tx;
use serde::Serialize;
use sqlx::FromRow;

#[derive(Debug, Clone, PartialEq, Serialize, FromRow, schemars::JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct FlagEvaluation {
    pub id: EvaluationId,
    pub org_id: OrgId,
    pub app_id: FlagAppId,
    pub flag_id: FlagId,
    pub environment: String,
    pub variation_key: String,
    pub context: serde_json::Value,
    pub evaluated_at: chrono::DateTime<chrono::Utc>,
}

const EVAL_COLS: &str = "id, org_id, app_id, flag_id, environment, variation_key, context, \
                          evaluated_at";

/// Extension methods on [`otto_tenant::Tx`] for evaluation ingestion. This is
/// the write path the evaluation server (not built yet — see `VISION.md`)
/// will eventually call on every `isEnabled()`; nothing here assumes it runs
/// on the same hot path as production evaluation itself, since that path is
/// SDK/REST, not this crate, by design.
pub trait EvaluationsExt {
    fn record_evaluation(
        &mut self,
        app_id: FlagAppId,
        flag_id: FlagId,
        environment: &str,
        variation_key: &str,
        context: serde_json::Value,
    ) -> impl std::future::Future<Output = Result<FlagEvaluation>> + Send;

    /// The most recent evaluations for one flag, newest first. Exists to make
    /// ingestion observable and testable, not as the shape a future
    /// correlate_errors/assess_risk tool would actually query.
    fn recent_evaluations(
        &mut self,
        flag_id: FlagId,
        limit: i64,
    ) -> impl std::future::Future<Output = Result<Vec<FlagEvaluation>>> + Send;
}

impl EvaluationsExt for Tx<'_> {
    async fn record_evaluation(
        &mut self,
        app_id: FlagAppId,
        flag_id: FlagId,
        environment: &str,
        variation_key: &str,
        context: serde_json::Value,
    ) -> Result<FlagEvaluation> {
        let org = self.org();
        let eval = sqlx::query_as(&format!(
            "INSERT INTO flag_evaluations (org_id, app_id, flag_id, environment, variation_key, context) \
             VALUES ($1,$2,$3,$4,$5,$6) RETURNING {EVAL_COLS}"
        ))
        .bind(org)
        .bind(app_id)
        .bind(flag_id)
        .bind(environment)
        .bind(variation_key)
        .bind(context)
        .fetch_one(self.conn())
        .await?;
        Ok(eval)
    }

    async fn recent_evaluations(
        &mut self,
        flag_id: FlagId,
        limit: i64,
    ) -> Result<Vec<FlagEvaluation>> {
        let org = self.org();
        let evals = sqlx::query_as(&format!(
            "SELECT {EVAL_COLS} FROM flag_evaluations \
             WHERE org_id = $1 AND flag_id = $2 \
             ORDER BY evaluated_at DESC LIMIT $3"
        ))
        .bind(org)
        .bind(flag_id)
        .bind(limit)
        .fetch_all(self.conn())
        .await?;
        Ok(evals)
    }
}
