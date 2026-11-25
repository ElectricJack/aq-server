use super::*;

impl Service {
    pub(crate) async fn completion_in_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        decision: Uuid,
    ) -> Result<(ReviewSubject, serde_json::Value)> {
        let (subject, event, _) = self.completion_subject_in_tx(tx, decision).await?;
        match subject {
            OwnedSubject::Code(subject) => Ok((*subject, event)),
            _ => Err(conflict()),
        }
    }
    pub(super) async fn completion_subject_in_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        decision: Uuid,
    ) -> Result<(OwnedSubject, serde_json::Value, DecisionReceipt)> {
        let row =
            sqlx::query("SELECT * FROM human_decision_requests WHERE decision_id=$1 FOR SHARE")
                .bind(decision)
                .fetch_one(&mut **tx)
                .await
                .map_err(db)?;
        let request: DecisionRequest = self.decode("request", &decision.to_string(), &row)?;
        request.validate().map_err(|_| AppError::Internal)?;
        if request.decision_id != decision
            || hash(&request)? != row.get::<String, _>("request_digest")
        {
            return Err(AppError::Internal);
        }
        if row.get::<i64, _>("decision_revision") != 2 {
            return Err(AppError::Internal);
        }
        let sr = sqlx::query("SELECT * FROM review_subjects WHERE subject_digest=$1 FOR SHARE")
            .bind(request.subject.digest.as_str())
            .fetch_one(&mut **tx)
            .await
            .map_err(db)?;
        let subject = self.decode_subject(request.subject.digest.as_str(), &sr)?;
        if subject.artifact().map_err(|_| AppError::Internal)? != request.subject {
            return Err(AppError::Internal);
        }
        let finality =
            sqlx::query("SELECT * FROM human_decision_finality WHERE decision_id=$1 FOR SHARE")
                .bind(decision)
                .fetch_one(&mut **tx)
                .await
                .map_err(db)?;
        let receipt = self.read_receipt(decision, &finality, &request)?;
        let event = self.completion_event(tx, &receipt).await?;
        Ok((subject, event, receipt))
    }
    async fn completion_event(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        receipt: &DecisionReceipt,
    ) -> Result<serde_json::Value> {
        let decision = receipt.decision_id;
        let outbox =
            sqlx::query("SELECT * FROM review_owner_outbox WHERE decision_id=$1 FOR UPDATE")
                .bind(decision)
                .fetch_one(&mut **tx)
                .await
                .map_err(db)?;
        let event: serde_json::Value = self.decode("outbox", &decision.to_string(), &outbox)?;
        if outbox.get::<Uuid, _>("event_id") != decision
            || outbox.get::<String, _>("producer") != "aq.human.decisions@1"
        {
            return Err(AppError::Internal);
        }
        if outbox.get::<String, _>("receipt_digest") != hash(receipt)?
            || event != decision_event(receipt)?
        {
            return Err(AppError::Internal);
        }
        Ok(event)
    }
}
