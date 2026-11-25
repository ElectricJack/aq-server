//! Authorize lookups and publish a decision under the same operation and retention fences.
use super::{Result, *};
pub(super) struct Publication<'a> {
    state: &'a AppState,
    subject: principal::Principal,
    operation_id: Uuid,
    record_refused: bool,
    payload: Digest,
}
impl<'a> Publication<'a> {
    pub(super) fn new(
        state: &'a AppState,
        subject: principal::Principal,
        request: &ResolveRequest,
    ) -> Result<Self> {
        Ok(Self {
            state,
            subject,
            operation_id: request.operation_id,
            record_refused: request.record_refused,
            payload: digest(request).map_err(|_| bad("resolver digest"))?,
        })
    }
    pub(super) async fn replay(&self) -> Result<Option<Decision>> {
        let mut early = self.state.store.pool.begin().await.map_err(db)?;
        self.subject.authorize(self.state, &mut early).await?;
        if let Some(decision) = self
            .subject
            .lookup(&mut early, self.operation_id, &self.payload)
            .await?
        {
            return Ok(Some(decision));
        }
        early.rollback().await.map_err(db)?;
        Ok(None)
    }
    pub(super) async fn publish(&self, decision: Decision) -> Result<Decision> {
        let encoded = serde_json::to_value(&decision).map_err(|_| AppError::Internal)?;
        if serde_json::to_vec(&encoded)
            .map_err(|_| AppError::Internal)?
            .len()
            > 8 * 1024 * 1024
        {
            return Err(AppError::PayloadTooLarge("decision artifact bound".into()));
        }
        let mut tx = self.state.store.pool.begin().await.map_err(db)?;
        // Metadata publication uses the project/node/catalog fence, then registry.
        fence_snapshot(self.state, &mut tx, &decision, false).await?;
        sqlx::query("SELECT pg_advisory_xact_lock(712010)")
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        self.subject.authorize(self.state, &mut tx).await?;
        if let Some(decision) = self
            .subject
            .lookup(&mut tx, self.operation_id, &self.payload)
            .await?
        {
            return Ok(decision);
        }
        if !self.record_refused && decision.outcome.resolved.is_none() {
            tx.rollback().await.map_err(db)?;
            return Ok(decision);
        }
        let kind = aq_history_caps::Kind::IntelligenceDecisions;
        let row = aq_history_caps::usage(&mut tx, kind, None, decision.inputs.now_ms)
            .await
            .map_err(db)?;
        let incoming: i64 = sqlx::query_scalar("SELECT pg_column_size($1::jsonb)::BIGINT")
            .bind(&encoded)
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?;
        if row.total_rows >= 1024
            || row
                .total_bytes
                .checked_add(incoming)
                .is_none_or(|n| n > 64 * 1024 * 1024)
        {
            return Err(conflict("decision artifact retention limit"));
        }
        self.subject
            .insert(
                &mut tx,
                (self.operation_id, &self.payload),
                (&decision, encoded),
            )
            .await?;
        check_time(self.state, &decision)?;
        tx.commit().await.map_err(db)?;
        Ok(decision)
    }
}
