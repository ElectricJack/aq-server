use super::*;

/// D3's acceptance, re-checked under the report lock, the D3 settlement lock
/// and the common node/attempt/hot locks that cancellation and abandonment
/// take, so neither can interleave with the fence written under them.
async fn accepted_locked(
    state: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    report_id: Uuid,
) -> Result<Option<results::RecordedResult>> {
    let r = results::load_locked(state, tx, report_id).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,56))")
        .bind(format!("aq-auto-settlement:{report_id}"))
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    let settlement: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT disposition,reason FROM profiled_auto_settlements WHERE report_id=$1",
    )
    .bind(report_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?;
    let v = crate::execution::authority::load_locked(
        state,
        tx,
        &r.report.binding.session,
        crate::execution::authority::Policy::IdentityOnly,
    )
    .await?;
    let accepted = settlement == Some(("held".into(), Some(HELD.into())))
        && r.is_current_report_head
        && matches!(
            r.report.outcome,
            aq_admission::transport::ProcessOutcome::Success
        )
        && !v.revoked
        && v.cancellation_id.is_none()
        && v.holds_task
        && v.stopped;
    Ok(accepted.then_some(r))
}

pub(super) enum Fence {
    Fenced,
    NotAccepted,
    /// Final evidence already exists for this source.
    Recorded,
}

/// Durable publication fence: an `indeterminate` evidence row in phase
/// `publishing`, written only while D3's acceptance holds under the common
/// locks. Cancellation and abandonment refuse to act on an attempt with such a
/// row, and a crash after it leaves the result held (never completed, never
/// reworked) until the publication is reconciled.
pub(super) async fn fence(
    state: &AppState,
    report_id: Uuid,
    source_op: Uuid,
    evidence: &Value,
) -> Result<Fence> {
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    let Some(r) = accepted_locked(state, &mut tx, report_id).await? else {
        tx.rollback().await.map_err(db)?;
        return Ok(Fence::NotAccepted);
    };
    let attempt = r.report.binding.attempt_id;
    let existing: Option<(String, Option<String>)> = sqlx::query_as("SELECT state,evidence->>'phase' FROM profiled_delivery_evidence WHERE attempt_id=$1 AND source_operation_id=$2 FOR UPDATE")
        .bind(attempt).bind(source_op).fetch_optional(&mut *tx).await.map_err(db)?;
    let fenced = match existing {
        Some((s, Some(phase))) if s == "indeterminate" && phase == "publishing" => Fence::Fenced,
        Some(_) => Fence::Recorded,
        None => {
            sqlx::query("INSERT INTO profiled_delivery_evidence(attempt_id,source_operation_id,state,evidence,recorded_at_ms) VALUES($1,$2,'indeterminate',$3,$4)")
                .bind(attempt).bind(source_op).bind(evidence).bind(now(state)?)
                .execute(&mut *tx).await.map_err(db)?;
            Fence::Fenced
        }
    };
    tx.commit().await.map_err(db)?;
    Ok(fenced)
}

/// Removes this owner's fence when the publish operation provably sent
/// nothing (refused before dispatch), so the result returns to held/missing.
pub(super) async fn release_fence(state: &AppState, attempt: Uuid, source_op: Uuid) -> Result<()> {
    sqlx::query("DELETE FROM profiled_delivery_evidence WHERE attempt_id=$1 AND source_operation_id=$2 AND state='indeterminate' AND evidence->>'phase'='publishing'")
        .bind(attempt).bind(source_op).execute(&state.store.pool).await.map_err(db)?;
    Ok(())
}
