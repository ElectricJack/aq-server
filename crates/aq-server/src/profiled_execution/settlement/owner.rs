//! Actual stopped-result settlement. No caller-supplied proof or agent approval.
use super::super::results;
use super::{
    actor::ActorPin,
    applicability, effects,
    eligibility::{self, Disposition, Evidence, VerifiedEffectDisposition, VerifiedReviewEvidence},
};
use crate::{AppState, error::AppError, reviews, source_retention, store::User};
use aq_admission::{ArtifactRef, Digest, Text};
use aq_session_protocol::Validate;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

pub(super) type Result<T> = std::result::Result<T, AppError>;
pub(super) use crate::error::db;
pub(super) fn conflict() -> AppError {
    AppError::Conflict("settlement identity, accepted evidence or owner changed".into())
}
pub(super) fn unavailable() -> AppError {
    AppError::UpdatesUnavailable("settlement owner unavailable".into())
}
pub(super) fn bytes(v: &impl Serialize) -> Result<Vec<u8>> {
    let mut v = serde_json::to_value(v).map_err(|_| AppError::Internal)?;
    v.sort_all_objects();
    serde_json::to_vec(&v).map_err(|_| AppError::Internal)
}
pub(super) fn hash(v: &impl Serialize) -> Result<String> {
    Ok(Digest::of(&bytes(v)?).as_str().into())
}
pub(super) fn reference(id: String, v: &impl Serialize) -> Result<ArtifactRef> {
    Ok(ArtifactRef {
        id: Text::new(id).map_err(|_| AppError::Internal)?,
        version: Text::new("1").map_err(|_| AppError::Internal)?,
        digest: Digest::of(&bytes(v)?),
    })
}
pub(super) fn now(state: &AppState) -> Result<i64> {
    let n = (state.clock)().timestamp_millis();
    if n <= 0 { Err(unavailable()) } else { Ok(n) }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluateRequest {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub report_id: Uuid,
    pub source_operation_id: Uuid,
    pub decision_id: Uuid,
    pub subject: ArtifactRef,
}
impl EvaluateRequest {
    fn validate(&self) -> Result<()> {
        if self.schema_version != 1
            || [
                self.operation_id,
                self.report_id,
                self.source_operation_id,
                self.decision_id,
            ]
            .iter()
            .any(Uuid::is_nil)
            || bytes(self)?.len() > 8192
        {
            return Err(AppError::BadRequest(
                "invalid settlement evaluation request".into(),
            ));
        }
        self.subject.id.validate().map_err(|_| conflict())?;
        self.subject.version.validate().map_err(|_| conflict())?;
        Ok(())
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationDisposition {
    Accepted,
    Rejected,
    Stale,
    NeedsReconciliation,
}
impl EvaluationDisposition {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Accepted => "accepted",
            Self::Rejected => "rejected",
            Self::Stale => "stale",
            Self::NeedsReconciliation => "needs_reconciliation",
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluationReceipt {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub attempt_id: Uuid,
    pub report: ArtifactRef,
    pub source_digest: Digest,
    pub review_receipt: ArtifactRef,
    pub subject: ArtifactRef,
    pub ledger_digest: Digest,
    pub disposition: EvaluationDisposition,
    pub reason: Option<eligibility::Reason>,
    pub recorded_at_ms: u64,
    pub release_id: Option<Uuid>,
    pub completion_operation_id: Option<Uuid>,
}
#[derive(Clone)]
pub struct Service {
    pub(super) state: AppState,
    pub(super) reviews: reviews::Service,
}
impl Service {
    pub fn new(state: AppState, reviews: reviews::Service) -> Self {
        Self { state, reviews }
    }
    pub(super) async fn tx(&self) -> Result<Transaction<'_, Postgres>> {
        let mut tx = self.state.store.pool.begin().await.map_err(db)?;
        sqlx::query("SELECT set_config('lock_timeout','500ms',true),set_config('statement_timeout','2000ms',true)").execute(&mut *tx).await.map_err(db)?;
        Ok(tx)
    }
    pub(super) async fn lock_budget(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
        sqlx::query(
            "SELECT pg_advisory_xact_lock(hashtextextended('aq-settlement39-budget-v1',0))",
        )
        .execute(&mut **tx)
        .await
        .map_err(db)?;
        Ok(())
    }
    fn replay(
        row: &sqlx::postgres::PgRow,
        actor: &ActorPin,
        request: &EvaluateRequest,
    ) -> Result<EvaluationReceipt> {
        if row.get::<Uuid, _>("actor_id") != actor.id()
            || row.get::<String, _>("actor_identity_digest") != actor.identity_digest()?
            || row.get::<Value, _>("request")
                != serde_json::to_value(request).map_err(|_| AppError::Internal)?
            || row.get::<String, _>("request_digest") != hash(request)?
        {
            return Err(conflict());
        }
        let receipt: EvaluationReceipt =
            serde_json::from_value(row.get("receipt")).map_err(|_| AppError::Internal)?;
        if hash(&receipt)? != row.get::<String, _>("receipt_digest") {
            return Err(AppError::Internal);
        }
        Ok(receipt)
    }
    /// Exact-ID lookup does not rerun observations or claim renewed eligibility.
    pub async fn lookup(&self, actor: &User, operation: Uuid) -> Result<EvaluationReceipt> {
        let actor = ActorPin::observe(&self.state, actor).await?;
        self.lookup_pinned(&actor, operation, None).await
    }
    async fn lookup_pinned(
        &self,
        actor: &ActorPin,
        operation: Uuid,
        expected: Option<&EvaluateRequest>,
    ) -> Result<EvaluationReceipt> {
        let mut tx = self.tx().await?;
        let report: Uuid = sqlx::query_scalar(
            "SELECT report_id FROM profiled_settlement_evaluations WHERE operation_id=$1",
        )
        .bind(operation)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?
        .ok_or_else(|| AppError::NotFound("settlement evaluation not found".into()))?;
        results::load_locked(&self.state, &mut tx, report).await?;
        let row = sqlx::query(
            "SELECT * FROM profiled_settlement_evaluations WHERE operation_id=$1 FOR SHARE",
        )
        .bind(operation)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
        actor.fence(&mut tx).await?;
        let request = serde_json::from_value(row.get("request")).map_err(|_| AppError::Internal)?;
        let r = Self::replay(&row, actor, expected.unwrap_or(&request))?;
        tx.commit().await.map_err(db)?;
        Ok(r)
    }
    pub async fn evaluate(
        &self,
        actor: &User,
        request: &EvaluateRequest,
    ) -> Result<EvaluationReceipt> {
        request.validate()?;
        let actor = ActorPin::observe(&self.state, actor).await?;
        // Replay before remote reads: an accepted historical result remains exact.
        let exists: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM profiled_settlement_evaluations WHERE operation_id=$1)",
        )
        .bind(request.operation_id)
        .fetch_one(&self.state.store.pool)
        .await
        .map_err(db)?;
        if exists {
            return self
                .lookup_pinned(&actor, request.operation_id, Some(request))
                .await;
        }
        let observation = applicability::Observation::observe(
            &self.state,
            request.report_id,
            request.source_operation_id,
        )
        .await?;
        let mut tx = self.tx().await?;
        Self::lock_budget(&mut tx).await?;
        let result = results::load_locked(&self.state, &mut tx, request.report_id).await?;
        if let Some(row) = sqlx::query(
            "SELECT * FROM profiled_settlement_evaluations WHERE operation_id=$1 FOR SHARE",
        )
        .bind(request.operation_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?
        {
            actor.fence(&mut tx).await?;
            let r = Self::replay(&row, &actor, request)?;
            tx.commit().await.map_err(db)?;
            return Ok(r);
        }
        let source =
            source_retention::load_confirmed_locked(&mut tx, request.source_operation_id, &result)
                .await?;
        let ledger = effects::inspect_aq_effects_locked(&self.state, &mut tx, &result).await?;
        let decision = match self
            .reviews
            .combined_completion_in_tx(&mut tx, request.decision_id)
            .await
        {
            Ok(decision) => decision,
            Err(error) => {
                // Current authorization still wins after external observation
                // or a pending-review lock wait, even on a normal held result.
                actor.fence(&mut tx).await?;
                return Err(error);
            }
        };
        if decision.subject().artifact()? != request.subject {
            return Err(conflict());
        }
        let review = VerifiedReviewEvidence::from_combined(&decision)?;
        let external = VerifiedEffectDisposition::from_combined(&decision, &source)?;
        let policy = applicability::policy(&self.state, &result)?;
        let current = observation
            .verify_locked(&self.state, &mut tx, &result, &source)
            .await?;
        actor.fence(&mut tx).await?;
        let plan = eligibility::evaluate_pilot(
            &result,
            &source,
            &Evidence {
                policy: Some(&policy),
                review: Some(&review),
                aq_effects: Some(&ledger),
                external_effects: Some(&external),
                applicability: Some(&current),
            },
        )?;
        let eligible = plan.disposition == Disposition::Eligible;
        let count: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM profiled_settlement_evaluations WHERE attempt_id=$1",
        )
        .bind(result.report.binding.attempt_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
        let used: i64 = sqlx::query_scalar(
            "SELECT COALESCE(sum(reserved_bytes),0)::BIGINT FROM profiled_settlement_evaluations",
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
        if count >= 16 || used > 67108864 - 196608 {
            return Err(AppError::PayloadTooLarge(
                "settlement retained quota exhausted".into(),
            ));
        }
        let receipt = EvaluationReceipt {
            schema_version: 1,
            operation_id: request.operation_id,
            attempt_id: result.report.binding.attempt_id,
            report: result.receipt_artifact.clone(),
            source_digest: applicability::source_digest(&source)?,
            review_receipt: decision
                .receipt()
                .artifact()
                .map_err(|_| AppError::Internal)?,
            subject: request.subject.clone(),
            ledger_digest: ledger.digest().clone(),
            disposition: match plan.disposition {
                Disposition::Eligible => EvaluationDisposition::Accepted,
                Disposition::Rejected => EvaluationDisposition::Rejected,
                Disposition::Stale => EvaluationDisposition::Stale,
                Disposition::NeedsReconciliation => EvaluationDisposition::NeedsReconciliation,
            },
            reason: plan.reason,
            recorded_at_ms: now(&self.state)? as u64,
            release_id: eligible.then(Uuid::new_v4),
            completion_operation_id: eligible.then(Uuid::new_v4),
        };
        let evidence = json!({"applicability":observation.evidence()?,"source_operation_id":request.source_operation_id,"aq_profile":result.spec.graph_access_profile,"ledger_count":ledger.operation_count(),"ledger_bytes":ledger.inspected_bytes(),"combined_subject":decision.subject(),"review_receipt":decision.receipt()});
        if bytes(request)?.len() + bytes(&receipt)?.len() + bytes(&evidence)?.len() > 131072 {
            return Err(AppError::PayloadTooLarge(
                "settlement evaluation evidence envelope".into(),
            ));
        }
        sqlx::query("SELECT set_config('aq.settlement_owner','aq.settlement-owner.1',true)")
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query("INSERT INTO profiled_settlement_evaluations(operation_id,attempt_id,report_id,source_operation_id,decision_id,actor_id,actor_identity_digest,request,request_digest,receipt,receipt_digest,evidence,disposition,created_at_ms,release_id,reserved_bytes) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,196608)")
            .bind(request.operation_id).bind(receipt.attempt_id).bind(request.report_id).bind(request.source_operation_id).bind(request.decision_id).bind(actor.id()).bind(actor.identity_digest()?).bind(serde_json::to_value(request).map_err(|_|AppError::Internal)?).bind(hash(request)?).bind(serde_json::to_value(&receipt).map_err(|_|AppError::Internal)?).bind(hash(&receipt)?).bind(evidence).bind(receipt.disposition.name()).bind(receipt.recorded_at_ms as i64).bind(receipt.release_id).execute(&mut *tx).await.map_err(db)?;
        if eligible {
            self.accept_locked(&mut tx, &result, &source, &receipt)
                .await?;
        }
        observation.validate_fresh(&self.state)?;
        tx.commit().await.map_err(db)?;
        Ok(receipt)
    }
    async fn accept_locked(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        r: &results::RecordedResult,
        source: &source_retention::RecordedSourceConfirmation,
        e: &EvaluationReceipt,
    ) -> Result<()> {
        let attempt = e.attempt_id;
        let row=sqlx::query("SELECT operation_id,request_bytes,phase_terminal_qualification FROM profiled_stopped_receipts WHERE attempt_id=$1 FOR SHARE").bind(attempt).fetch_one(&mut **tx).await.map_err(db)?;
        let q: Value = serde_json::from_slice(&row.get::<Vec<u8>, _>("request_bytes"))
            .map_err(|_| AppError::Internal)?;
        let qualification = row
            .get::<Option<Value>, _>("phase_terminal_qualification")
            .unwrap_or_else(|| q["qualification"].clone());
        if !qualification.is_object() || bytes(&qualification)?.len() > 4096 {
            return Err(unavailable());
        }
        let source_ref = crate::reviews::subject(r, source)?.source.retention_receipt;
        let effects_ref = reference(
            format!("aq.settlement-effects/{}", e.operation_id),
            &json!({"report":e.report,"source":e.source_digest,"ledger":e.ledger_digest,"subject":e.subject,"decision":e.review_receipt}),
        )?;
        let hot: Value = sqlx::query_scalar(
            "SELECT state FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE",
        )
        .bind(attempt)
        .fetch_one(&mut **tx)
        .await
        .map_err(db)?;
        let mut hot = hot;
        hot["retention"] = json!({"kind":"retained","receipt":source_ref});
        hot["effects"] = json!({"kind":"quiescent","receipt":effects_ref});
        sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
            .bind(attempt)
            .bind(hot)
            .execute(&mut **tx)
            .await
            .map_err(db)?;
        sqlx::query("INSERT INTO profiled_verified_releases(attempt_id,release_id,stop_operation_id,stop_evidence_digest,host_owner_qualification,source_receipt_digest,effect_receipt_digest,verified_at_ms,settlement_operation_id) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)").bind(attempt).bind(e.release_id).bind(row.get::<Uuid,_>("operation_id")).bind(r.receipt.stopped_receipt.digest.as_str()).bind(qualification).bind(source_ref.digest.as_str()).bind(effects_ref.digest.as_str()).bind(e.recorded_at_ms as i64).bind(e.operation_id).execute(&mut **tx).await.map_err(db)?;
        self.prepare_completion_locked(tx, r, e).await
    }
}
