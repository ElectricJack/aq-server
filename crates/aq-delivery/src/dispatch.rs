use super::*;

impl DeliveryService {
    pub(super) async fn execute<T: WireResult>(
        &self,
        op: &IntentRequest<'_>,
        intent: Intent,
    ) -> Result<Receipt<T>> {
        let mut tx = self.pool.begin().await?;
        let publish = op.name == "publish";
        if publish {
            sqlx::query(
                "SELECT operation_id FROM delivery_intents WHERE operation_id=$1 FOR UPDATE",
            )
            .bind(op.context.operation_id)
            .fetch_one(&mut *tx)
            .await?;
        }
        if let Some(raw) = sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT response FROM delivery_receipts WHERE operation_id=$1",
        )
        .bind(op.context.operation_id)
        .fetch_optional(&mut *tx)
        .await?
        {
            tx.commit().await?;
            return op.decode(raw, &intent.params);
        }
        if publish {
            self.dispatch_publication(op, intent, tx).await
        } else {
            tx.commit().await?;
            self.dispatch_retryable(op, intent).await
        }
    }
    async fn dispatch_publication<T: WireResult>(
        &self,
        op: &IntentRequest<'_>,
        intent: Intent,
        mut tx: Transaction<'_, Postgres>,
    ) -> Result<Receipt<T>> {
        let added = sqlx::query("INSERT INTO delivery_publish_dispatches(operation_id) VALUES($1) ON CONFLICT DO NOTHING").bind(op.context.operation_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        if added != 1 {
            return Err(DeliveryError::Indeterminate);
        }
        // Marker is committed before any bytes can be sent. Cancellation,
        // database failure, or process death leaves a durable destination hold.
        let raw = protocol::exchange(&self.socket, &intent.request)
            .await
            .map_err(|_| DeliveryError::Indeterminate)?;
        let parsed = op.decode::<T>(raw.clone(), &intent.params);
        let mut tx = self.pool.begin().await?;
        Self::save_receipt(&mut tx, op.context.operation_id, &raw).await?;
        // After dispatch, missing uncertainty flags are not proof of refusal:
        // aq-git may have pushed before failing to write/read its journal.
        // Only validated success or an explicitly failed, non-uncertain
        // operation releases the hold. All other replies need reconciliation.
        let conclusive = parsed.as_ref().is_ok_and(|r| match &r.outcome {
            Ok(_) => true,
            Err(e) => !e.uncertain() && r.operation_state == Some(OperationStatus::Failed),
        });
        if conclusive {
            sqlx::query("DELETE FROM delivery_destinations WHERE operation_id=$1")
                .bind(op.context.operation_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        parsed
    }
    async fn dispatch_retryable<T: WireResult>(
        &self,
        op: &IntentRequest<'_>,
        intent: Intent,
    ) -> Result<Receipt<T>> {
        // Concurrent calls use aq-git's same-operation idempotency. Release
        // every row lock and pooled connection before bounded socket I/O;
        // the first persisted terminal reply becomes the immutable receipt.
        let raw = protocol::exchange(&self.socket, &intent.request).await?;
        let parsed = op.decode::<T>(raw.clone(), &intent.params);
        if op.name == "candidate.create"
            && parsed
                .as_ref()
                .is_ok_and(|r| r.outcome.as_ref().is_err_and(GitError::retryable_candidate))
        {
            // A safe transient reply is an attempt outcome, not a terminal
            // receipt. The frozen intent survives for exact same-ID replay.
            return parsed;
        }
        let mut tx = self.pool.begin().await?;
        Self::save_receipt(&mut tx, op.context.operation_id, &raw).await?;
        let raw = sqlx::query_scalar::<_, Vec<u8>>(
            "SELECT response FROM delivery_receipts WHERE operation_id=$1",
        )
        .bind(op.context.operation_id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        op.decode(raw, &intent.params)
    }
    async fn save_receipt(
        tx: &mut Transaction<'_, Postgres>,
        operation: Uuid,
        raw: &[u8],
    ) -> Result<()> {
        sqlx::query("INSERT INTO delivery_receipts(operation_id,response) VALUES($1,$2) ON CONFLICT DO NOTHING")
            .bind(operation)
            .bind(raw)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }
}
