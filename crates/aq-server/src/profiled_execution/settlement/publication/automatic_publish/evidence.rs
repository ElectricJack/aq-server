use super::*;

pub(super) struct Prepared<'a> {
    pub(super) publication: &'a Publication,
    pub(super) base: ObjectId,
    pub(super) candidate: Candidate,
}
type EvidenceRow = (&'static str, Value, Option<SourceHead>);
impl Prepared<'_> {
    pub(super) fn evidence(
        &self,
        phase: Option<&str>,
        result: Option<&Value>,
        head: Option<&SourceHead>,
    ) -> Value {
        self.publication
            .target
            .evidence(&self.base, self.candidate.oid())
            .render(phase, result, head)
    }
    /// Replaces this owner's fence with the final evidence. A published head
    /// advances by compare-and-swap from the exact expected_old; if the recorded
    /// head moved meanwhile, the publication is recorded indeterminate (held).
    pub(super) async fn record(&self, state: &AppState, recorded: Recorded) -> Result<Step> {
        let publication = self.publication;
        let r = &publication.result;
        let admitted = &r.spec.source;
        let time = now(state)?;
        let mut tx = state.store.pool.begin().await.map_err(db)?;
        // Serializes head advancement per repository target.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,57))")
            .bind(format!(
                "aq-d1-source-head:{}:{}",
                admitted.repository_id, publication.target.policy.target
            ))
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        let (state_name, outcome, head) = self.evidence_row(&mut tx, recorded).await?;
        let evidence = self.evidence(None, Some(&outcome), head.as_ref());
        let written = sqlx::query("INSERT INTO profiled_delivery_evidence(attempt_id,source_operation_id,state,evidence,recorded_at_ms) VALUES($1,$2,$3,$4,$5) ON CONFLICT(attempt_id,source_operation_id) DO UPDATE SET state=EXCLUDED.state,evidence=EXCLUDED.evidence,recorded_at_ms=EXCLUDED.recorded_at_ms WHERE profiled_delivery_evidence.state='indeterminate' AND profiled_delivery_evidence.evidence->>'phase'='publishing'")
        .bind(r.report.binding.attempt_id).bind(publication.source_op).bind(state_name).bind(&evidence).bind(time)
        .execute(&mut *tx).await.map_err(db)?.rows_affected();
        tx.commit().await.map_err(db)?;
        if written == 1 {
            tracing::info!(report_id = %r.report.report_id, attempt = %r.report.binding.attempt_id, state = state_name, "delivery evidence recorded");
        }
        Ok(Step::Recorded)
    }

    async fn evidence_row(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        recorded: Recorded,
    ) -> Result<EvidenceRow> {
        let publication = self.publication;
        let r = &publication.result;
        let admitted = &r.spec.source;
        let row = match recorded {
            Recorded::Published { outcome, head } => {
                let current = current_head(
                    &mut **tx,
                    admitted.project_id,
                    admitted.repository_id,
                    admitted.repository_binding_generation,
                    &publication.target.policy.target,
                )
                .await?;
                if let Some(head) = publication
                    .target
                    .advance(current.as_ref(), &self.base, head)
                    .map_err(|_| conflict())?
                {
                    ("published", outcome, Some(head))
                } else {
                    tracing::warn!(report_id = %r.report.report_id, "source head moved since this publication's expected_old; recorded indeterminate");
                    (
                        "indeterminate",
                        json!({"reason":"source_head_moved","publication":outcome}),
                        None,
                    )
                }
            }
            Recorded::Unpublished(v) => ("unpublished", v, None),
            Recorded::Indeterminate(v) => ("indeterminate", v, None),
        };
        Ok(row)
    }
}
