use super::*;
use crate::profiled_execution::{results::RecordedResult, settlement::actor::ActorPin};
use sqlx::postgres::PgRow;

pub(super) struct LockedSubject {
    pub(super) report_id: Uuid,
    pub(super) subject_digest: String,
    pub(super) result: RecordedResult,
    pub(super) subject: OwnedSubject,
}

#[derive(Clone, Copy)]
pub(super) struct ReviewActor<'a> {
    pub(super) id: Uuid,
    pub(super) pin: Option<&'a ActorPin>,
}

pub(super) struct ReviewScope<'a> {
    pub(super) profile: SubjectProfile,
    pub(super) expected: Option<&'a ArtifactRef>,
}

impl<'a> ReviewScope<'a> {
    pub(super) fn new(profile: SubjectProfile, expected: Option<&'a ArtifactRef>) -> Self {
        Self { profile, expected }
    }
}

impl<'a> ReviewActor<'a> {
    pub(super) fn new(id: Uuid, pin: Option<&'a ActorPin>) -> Self {
        Self { id, pin }
    }

    pub(super) async fn authorize(self, tx: &mut Transaction<'_, Postgres>) -> Result<()> {
        Service::owner(tx, self.id).await?;
        if let Some(pin) = self.pin {
            if pin.id() != self.id {
                return Err(forbidden());
            }
            pin.fence(tx).await?;
        }
        Ok(())
    }
}

impl Service {
    pub(super) async fn lock_subject(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        decision: Uuid,
    ) -> Result<LockedSubject> {
        // Read immutable identities before taking the attempt and subject locks.
        let ids=sqlx::query("SELECT s.report_id,s.subject_digest FROM human_decision_requests d JOIN review_subjects s USING(subject_digest) WHERE d.decision_id=$1").bind(decision).fetch_optional(&mut **tx).await.map_err(db)?.ok_or_else(forbidden)?;
        let report_id: Uuid = ids.get("report_id");
        let subject_digest: String = ids.get("subject_digest");
        let result =
            crate::profiled_execution::results::load_locked(&self.state, tx, report_id).await?;
        let subject_row =
            sqlx::query("SELECT * FROM review_subjects WHERE subject_digest=$1 FOR SHARE")
                .bind(&subject_digest)
                .fetch_one(&mut **tx)
                .await
                .map_err(db)?;
        let subject = self.decode_subject(&subject_digest, &subject_row)?;
        Ok(LockedSubject {
            report_id,
            subject_digest,
            result,
            subject,
        })
    }

    pub(super) fn read_receipt(
        &self,
        decision: Uuid,
        row: &PgRow,
        request: &DecisionRequest,
    ) -> Result<DecisionReceipt> {
        let receipt: DecisionReceipt = self.decode("finality", &decision.to_string(), row)?;
        receipt
            .validate_for(request)
            .map_err(|_| AppError::Internal)?;
        if hash(&receipt)? != row.get::<String, _>("receipt_digest") {
            return Err(AppError::Internal);
        }
        Ok(receipt)
    }
    pub(super) async fn emit_decision(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        receipt: &DecisionReceipt,
        digest: &str,
    ) -> Result<()> {
        let decision = receipt.decision_id;
        let event = decision_event(receipt)?;
        let s = self.seal("outbox", &decision.to_string(), &encode(&event)?)?;
        sqlx::query("INSERT INTO review_owner_outbox(event_id,decision_id,producer,receipt_digest,sealed_version,key_id,nonce,ciphertext) VALUES($1,$1,'aq.human.decisions@1',$2,$3,$4,$5,$6)").bind(decision).bind(digest).bind(s.version as i32).bind(s.key_id).bind(s.nonce).bind(s.ciphertext).execute(&mut **tx).await.map_err(db)?;
        Ok(())
    }
}

pub(super) fn decision_event(receipt: &DecisionReceipt) -> Result<serde_json::Value> {
    let outcome = match receipt.finality {
        DecisionFinality::HumanAnswer {
            choice: Choice::Approve,
            ..
        } => "approved",
        DecisionFinality::HumanAnswer {
            choice: Choice::Reject,
            ..
        } => "rejected",
        DecisionFinality::Expired {} => "expired",
        DecisionFinality::Cancelled { .. } => "cancelled",
    };
    Ok(
        json!({"decision":receipt.decision_id.to_string(),"outcome":outcome,"output":{"receipt":receipt.artifact().map_err(|_|AppError::Internal)?,"subject":receipt.subject}}),
    )
}

impl LockedSubject {
    pub(super) fn validate_binding(&self) -> Result<()> {
        let expected = self.subject.artifact().map_err(|_| AppError::Internal)?;
        if expected.digest.as_str() != self.subject_digest {
            return Err(AppError::Internal);
        }
        if self.subject.result.report_id != self.report_id
            || self.subject.result.record_receipt != self.result.receipt_artifact
        {
            return Err(AppError::Internal);
        }
        Ok(())
    }
}

pub(super) fn new_receipt(
    request: &DecisionRequest,
    decision: Uuid,
    at: u64,
    finality: DecisionFinality,
) -> Result<DecisionReceipt> {
    let receipt = DecisionReceipt {
        schema_version: 1,
        decision_id: decision,
        decision_revision: 2,
        subject: request.subject.clone(),
        request_digest: wire::digest(request).map_err(|_| AppError::Internal)?,
        eligibility_project_revision: request.eligibility_project_revision,
        review_policy: request.review_policy.clone(),
        recorded_at_ms: at,
        finality,
    };
    receipt
        .validate_for(request)
        .map_err(|_| AppError::Internal)?;
    Ok(receipt)
}

pub(super) fn pending(row: &PgRow) -> bool {
    row.get::<i64, _>("decision_revision") == 1 && row.get::<String, _>("state") == "pending"
}

pub(super) fn validate_actor_pin(row: &PgRow, pin: &ActorPin) -> Result<()> {
    if row
        .get::<Option<String>, _>("combined_actor_digest")
        .as_deref()
        != Some(pin.identity_digest()?.as_str())
        || row
            .get::<Option<String>, _>("combined_actor_revision")
            .as_deref()
            != Some(pin.revision())
    {
        return Err(forbidden());
    }
    Ok(())
}
