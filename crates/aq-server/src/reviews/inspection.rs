use super::*;

impl Service {
    pub async fn inspect(&self, actor: Uuid, decision: Uuid) -> Result<serde_json::Value> {
        self.inspect_subject(
            ReviewActor::new(actor, None),
            decision,
            SubjectProfile::Code,
        )
        .await
    }
    pub(super) async fn inspect_subject(
        &self,
        owner: ReviewActor<'_>,
        decision: Uuid,
        profile: SubjectProfile,
    ) -> Result<serde_json::Value> {
        let actor = owner.id;
        let mut tx = self.tx().await?;
        let LockedSubject {
            result, subject, ..
        } = self.lock_subject(&mut tx, decision).await?;
        if subject.profile() != profile {
            return Err(forbidden());
        }
        let row =
            sqlx::query("SELECT * FROM human_decision_requests WHERE decision_id=$1 FOR SHARE")
                .bind(decision)
                .fetch_one(&mut *tx)
                .await
                .map_err(db)?;
        let request: DecisionRequest = self.decode("request", &decision.to_string(), &row)?;
        request.validate().map_err(|_| AppError::Internal)?;
        if hash(&request)? != row.get::<String, _>("request_digest") {
            return Err(AppError::Internal);
        }
        if request.subject != subject.artifact().map_err(|_| AppError::Internal)?
            || subject.result.record_receipt != result.receipt_artifact
        {
            return Err(AppError::Internal);
        }
        if row.get::<Uuid, _>("requester_id") != actor && !request.eligible_owners.contains(&actor)
        {
            return Err(forbidden());
        }
        owner.authorize(&mut tx).await?;
        let finality =
            sqlx::query("SELECT * FROM human_decision_finality WHERE decision_id=$1 FOR SHARE")
                .bind(decision)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db)?;
        let receipt = finality
            .as_ref()
            .map(|row| self.read_receipt(decision, row, &request))
            .transpose()?;
        let view = json!({"request":request,"subject":subject,"decision_revision":row.get::<i64,_>("decision_revision"),"state":row.get::<String,_>("state"),"receipt":receipt,"current_report_head":result.is_current_report_head,"acceptance_criteria":result.outcome.acceptance_criteria,"recorded_result":{"report_id":result.report.report_id,"conclusion":result.report.conclusion,"process_outcome":result.report.outcome,"recorded_at_ms":result.receipt.recorded_at_ms}});
        if serde_json::to_vec(&view)
            .map_err(|_| AppError::Internal)?
            .len()
            > 4 * MAX_BYTES + 2048
        {
            return Err(AppError::Internal);
        }
        tx.commit().await.map_err(db)?;
        Ok(view)
    }
}
