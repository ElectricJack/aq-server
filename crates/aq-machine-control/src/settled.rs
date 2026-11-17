//! Nothing finished may hold its task. A failed attempt whose stop is proven, whose source
//! is retained and whose effects are quiescent used to release its task claim only when a
//! successor was offered. A task whose retry was not admitted (an exhausted retry budget, a
//! changed project binding, a node that was removed) therefore kept its last attempt holding
//! the task for good. The claim is now released as soon as the attempt is settled as failed:
//!
//! - its stop is `not_started`, or
//! - the default policy finally reworked its current result (`rework_required`), or
//! - it was released as lost, or
//! - it was cancelled and its stop settled (not a handoff, whose parked predecessor waits
//!   for its successor).
//!
//! The release record is the B4 replacement release ([`crate::lapse::release`] writes the
//! same one), so whether the task is retried stays the replacement owner's decision and the
//! playbook's budgets: a later offer is assessed by the normal rules, and a task whose retry
//! is not admitted stays held (`task_holds`) until an owner acts. This module only stops the
//! finished attempt from holding the task. Accepted, completing or publishing attempts are
//! never candidates; the release guard (migration 0052) re-checks the attempt's own facts.
use crate::Error;
use sqlx::PgPool;
use uuid::Uuid;

/// The rule over `execution_attempts a` and `profiled_authority_hot h`.
const SETTLED: &str = "a.kind='profiled_v1' AND a.holds_task AND h.state->>'status'='finished' \
 AND h.state#>>'{stop,kind}' IN ('stopped','not_started') \
 AND h.state#>>'{retention,kind}'='retained' AND h.state#>>'{effects,kind}'='quiescent' \
 AND (h.state#>>'{stop,kind}'='not_started' \
  OR EXISTS(SELECT 1 FROM profiled_auto_settlements s JOIN profiled_result_heads r USING(attempt_id,report_id) \
   WHERE s.attempt_id=a.attempt_id AND s.disposition='rework_required') \
  OR EXISTS(SELECT 1 FROM lost_attempt_releases x WHERE x.attempt_id=a.attempt_id) \
  OR (EXISTS(SELECT 1 FROM profiled_cancelled_settlements x WHERE x.attempt_id=a.attempt_id) \
   AND NOT EXISTS(SELECT 1 FROM attempt_handoffs x WHERE x.attempt_id=a.attempt_id))) \
 AND NOT EXISTS(SELECT 1 FROM profiled_settlement_evaluations x WHERE x.attempt_id=a.attempt_id \
  AND x.disposition IN ('accepted','needs_reconciliation','stale')) \
 AND NOT EXISTS(SELECT 1 FROM profiled_task_completions x WHERE x.attempt_id=a.attempt_id) \
 AND NOT EXISTS(SELECT 1 FROM profiled_auto_settlements x WHERE x.attempt_id=a.attempt_id \
  AND x.disposition IN ('prepared','possible','confirmed')) \
 AND NOT EXISTS(SELECT 1 FROM profiled_delivery_evidence x WHERE x.attempt_id=a.attempt_id \
  AND x.state='indeterminate')";

/// Releases the task claim of one settled failed attempt under the common node ->
/// attempt/hot locks. Idempotent; false when the attempt is not (or no longer) a candidate.
pub async fn release(pool: &PgPool, attempt: Uuid, now_ms: i64) -> Result<bool, Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT n.id FROM nodes n JOIN execution_attempts a ON a.node_id=n.id WHERE a.attempt_id=$1 FOR UPDATE OF n")
        .bind(attempt)
        .fetch_optional(&mut *tx)
        .await?;
    let held = sqlx::query(&format!(
        "SELECT 1 FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) \
         WHERE a.attempt_id=$1 AND {SETTLED} FOR UPDATE OF a,h"
    ))
    .bind(attempt)
    .fetch_optional(&mut *tx)
    .await?;
    if held.is_none() {
        return Ok(false);
    }
    sqlx::query(
        "INSERT INTO profiled_replacement_releases(attempt_id,task_id,reason,stop_receipt_digest,\
         retained_source_receipt_digest,effect_reconciliation_receipt_digest,attempts_started,recorded_at_ms) \
         SELECT a.attempt_id,a.task_id,'failed_attempt_replacement',h.state#>>'{stop,receipt,digest}',\
         h.state#>>'{retention,receipt,digest}',h.state#>>'{effects,receipt,digest}',\
         (SELECT count(*) FROM execution_attempts x WHERE x.task_id=a.task_id \
          AND NOT (NOT x.holds_task AND profiled_never_granted_valid(x.attempt_id))),$2 \
         FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) WHERE a.attempt_id=$1 \
         ON CONFLICT(attempt_id) DO NOTHING",
    )
    .bind(attempt)
    .bind(now_ms)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE execution_attempts SET holds_task=FALSE WHERE attempt_id=$1 AND holds_task",
    )
    .bind(attempt)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(true)
}

/// One bounded page of the sweep: releases the settled failed attempts after `cursor` and
/// advances it (reset at the end of the set). A refused attempt is logged and passed over, so
/// it never starves the others. Returns how many claims were released.
pub async fn sweep(pool: &PgPool, cursor: &mut Option<Uuid>, now_ms: i64) -> Result<usize, Error> {
    // Backstop for occupancy (migration 0136): a stop whose compute release was not recorded
    // with it (its proof was completed later) is validated again here, a few per page.
    let unrecorded = sqlx::query(
        "SELECT native_compute_release_record(x.attempt_id) FROM (SELECT a.attempt_id \
         FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) \
         WHERE a.kind='profiled_v1' AND a.holds_task AND h.state->>'status'='finished' \
         AND h.state#>>'{stop,kind}'='stopped' AND NOT EXISTS(SELECT 1 \
         FROM execution_attempt_compute_releases r WHERE r.attempt_id=a.attempt_id) \
         ORDER BY random() LIMIT 4) x",
    );
    if let Err(error) = unrecorded.execute(pool).await {
        tracing::debug!(?error, "unrecorded compute releases not validated");
    }
    let page: Vec<Uuid> = sqlx::query_scalar(&format!(
        "SELECT a.attempt_id FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) \
         WHERE ($1::UUID IS NULL OR a.attempt_id>$1) AND {SETTLED} ORDER BY a.attempt_id LIMIT 8"
    ))
    .bind(*cursor)
    .fetch_all(pool)
    .await?;
    *cursor = page.last().copied();
    let mut released = 0;
    for attempt in page {
        match release(pool, attempt, now_ms).await {
            Ok(true) => {
                tracing::info!(%attempt, "settled failed attempt released its task claim; a retry is the replacement owner's decision");
                released += 1;
            }
            Ok(false) => {}
            Err(error) => tracing::warn!(%attempt, ?error, "settled failed attempt not released"),
        }
    }
    Ok(released)
}
