//! Exact human decision owner. SQL026 remains candidate until native025 exists.
//! No public caller can submit a VerifiedSubject or a passed check. Actual
//! B1/source owners build subjects, and configured public Git confirms a fresh
//! retention observation outside authority locks. No B2 eligibility evaluation.
use crate::{
    AppState,
    error::AppError,
    protected_storage::{Keyring, Sealed},
};
use aq_review_protocol::{self as wire, *};
use aq_session_protocol::{ArtifactRef, Text};
use serde::Serialize;
use serde_json::json;
use sqlx::{Postgres, Row, Transaction};
use std::sync::Arc;
use uuid::Uuid;
pub(crate) mod action;
pub mod combined;
mod combined_http;
mod inbox;
mod subject_owner;
mod view;
pub use inbox::schema as inbox_contract_schema;
pub use inbox::{InboxItem, InboxPage, InboxQuery, InboxState};
use subject_owner::{Profile as SubjectProfile, Subject as OwnedSubject};
pub use view::{DecisionKind, Impact, InboxOrder, RequestedBy, ReviewPayload};

type Result<T> = std::result::Result<T, AppError>;
/// Load retained review keys before listeners start. Never invent ephemeral keys:
/// they must also decrypt decisions and settlement receipts after restart.
pub fn deserialize_config<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<Arc<Keyring>>, D::Error> {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Config {
        protection: crate::protected_storage::ProtectionConfig,
    }
    use serde::Deserialize;
    Option::<Config>::deserialize(deserializer)?
        .map(|config| {
            Keyring::load(&config.protection)
                .map(Arc::new)
                .map_err(serde::de::Error::custom)
        })
        .transpose()
}
use crate::error::db;
fn invalid() -> AppError {
    AppError::BadRequest("invalid review request".into())
}
fn conflict() -> AppError {
    AppError::Conflict("review identity, revision or owner pins changed".into())
}
fn forbidden() -> AppError {
    AppError::Forbidden("review is not available to this owner".into())
}
fn encode<T: Serialize>(v: &T) -> Result<Vec<u8>> {
    wire::canonical_bytes(v).map_err(|_| invalid())
}
fn hash<T: Serialize>(v: &T) -> Result<String> {
    Ok(wire::digest(v).map_err(|_| invalid())?.as_str().into())
}
fn now(state: &AppState) -> Result<u64> {
    u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|n| *n > 0)
        .ok_or(AppError::Internal)
}
fn reference(id: String, bytes: &impl Serialize) -> Result<ArtifactRef> {
    Ok(ArtifactRef {
        id: Text::new(id).map_err(|_| invalid())?,
        version: Text::new("1").map_err(|_| invalid())?,
        digest: wire::digest(bytes).map_err(|_| invalid())?,
    })
}
/// Ephemeral authenticated owner reads, not serializable proof or create permit.
pub struct PreparedReview {
    report_id: Uuid,
    source_operation: Uuid,
    subject: ReviewSubject,
    retention: crate::source_retention::result_retention::ResultRetentionObservation,
}
impl PreparedReview {
    pub fn subject(&self) -> &ReviewSubject {
        &self.subject
    }
}
#[derive(Clone)]
pub struct Service {
    state: AppState,
    keys: Arc<Keyring>,
}
impl Service {
    pub fn new(state: AppState, keys: Arc<Keyring>) -> Self {
        Self { state, keys }
    }
    async fn tx(&self) -> Result<Transaction<'_, Postgres>> {
        let mut tx = self.state.store.pool.begin().await.map_err(db)?;
        sqlx::query("SELECT set_config('statement_timeout','2000ms',true),set_config('lock_timeout','500ms',true)").execute(&mut *tx).await.map_err(db)?;
        Ok(tx)
    }
    async fn budget(tx: &mut Transaction<'_, Postgres>) -> Result<sqlx::postgres::PgRow> {
        sqlx::query("SELECT * FROM review_owner_budget WHERE singleton FOR UPDATE")
            .fetch_one(&mut **tx)
            .await
            .map_err(db)
    }
    async fn owner(tx: &mut Transaction<'_, Postgres>, actor: Uuid) -> Result<()> {
        let role: Option<String> =
            sqlx::query_scalar("SELECT role FROM users WHERE id=$1 FOR SHARE")
                .bind(actor)
                .fetch_optional(&mut **tx)
                .await
                .map_err(db)?;
        if role.as_deref() != Some("owner") {
            return Err(forbidden());
        }
        Ok(())
    }
    fn seal(&self, kind: &str, id: &str, bytes: &[u8]) -> Result<Sealed> {
        self.keys
            .seal(&Self::aad(kind, id), bytes)
            .map_err(|_| AppError::Internal)
    }
    fn aad(kind: &str, id: &str) -> Vec<u8> {
        format!("aq.review.v1/{kind}/{id}").into_bytes()
    }
    fn decode<T: serde::de::DeserializeOwned + Serialize>(
        &self,
        kind: &str,
        id: &str,
        row: &sqlx::postgres::PgRow,
    ) -> Result<T> {
        let s = Sealed {
            version: row.get::<i32, _>("sealed_version") as u32,
            key_id: row.get("key_id"),
            nonce: row.get("nonce"),
            ciphertext: row.get("ciphertext"),
        };
        let bytes = self
            .keys
            .open(&Self::aad(kind, id), &s)
            .map_err(|_| AppError::Internal)?;
        let value: T = serde_json::from_slice(&bytes).map_err(|_| AppError::Internal)?;
        if encode(&value)? != bytes.as_slice() {
            return Err(AppError::Internal);
        }
        Ok(value)
    }
    /// Internal admission preparation. Caller separately checks project scope;
    /// eventual creation derives/currently checks owner identity from auth.
    pub async fn prepare(&self, report_id: Uuid, source_operation: Uuid) -> Result<PreparedReview> {
        let mut tx = self.tx().await?;
        let result =
            crate::profiled_execution::results::load_locked(&self.state, &mut tx, report_id)
                .await?;
        if !result.is_current_report_head {
            return Err(conflict());
        }
        let source =
            crate::source_retention::load_confirmed_locked(&mut tx, source_operation, &result)
                .await?;
        let subject = subject(&result, &source)?;
        tx.commit().await.map_err(db)?;
        let retention =
            crate::source_retention::result_retention::inspect_current(&self.state, &source)
                .await?;
        Ok(PreparedReview {
            report_id,
            source_operation,
            subject,
            retention,
        })
    }
    /// Explicit requester is supplied only by authenticated internal adapter or
    /// RequireOwner. Stable requester/operation is immutable and retained.
    pub async fn admit(
        &self,
        actor: Uuid,
        prepared: &PreparedReview,
        request: &DecisionRequest,
    ) -> Result<()> {
        let mut tx = self.tx().await?;
        self.admit_in_tx(&mut tx, actor, prepared, request).await?;
        tx.commit().await.map_err(db)
    }
    /// Adapter takes this before bridge namespace/grant locks. Reacquiring on
    /// the same transaction is harmless; no permit exists outside that SQL tx.
    pub(crate) async fn lock_budget(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
        Self::budget(tx).await?;
        Ok(())
    }
    pub(crate) async fn admit_in_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        actor: Uuid,
        prepared: &PreparedReview,
        request: &DecisionRequest,
    ) -> Result<()> {
        self.admit_subject_in_tx(tx, actor, prepared, request, None, None)
            .await
    }
    async fn admit_subject_in_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        actor: Uuid,
        prepared: &PreparedReview,
        request: &DecisionRequest,
        combined: Option<&combined::CombinedSubject>,
        principal: Option<&crate::profiled_execution::settlement::actor::ActorPin>,
    ) -> Result<()> {
        request.validate().map_err(|_| invalid())?;
        let wanted = match combined {
            Some(s) => OwnedSubject::SourceAndEffects(Box::new(s.clone())),
            None => OwnedSubject::Code(Box::new(prepared.subject.clone())),
        };
        if request.subject != wanted.artifact()? {
            return Err(conflict());
        }
        let digest = hash(request)?;
        let budget = Self::budget(tx).await?;
        // Global budget lock serializes new admissions and duplicate IDs before
        // common authority locks. No decision row is locked before an attempt.
        let result =
            crate::profiled_execution::results::load_locked(&self.state, tx, prepared.report_id)
                .await?;
        let source =
            crate::source_retention::load_confirmed_locked(tx, prepared.source_operation, &result)
                .await?;
        let code = subject(&result, &source)?;
        if code != prepared.subject {
            return Err(conflict());
        }
        let exact = if let Some(combined) = combined {
            combined.validate()?;
            let ledger = crate::profiled_execution::settlement::effects::inspect_aq_effects_locked(
                &self.state,
                tx,
                &result,
            )
            .await?;
            if combined.code != code || !combined.matches_ledger(&result, &ledger) {
                return Err(conflict());
            }
            OwnedSubject::SourceAndEffects(Box::new(combined.clone()))
        } else {
            OwnedSubject::Code(Box::new(code))
        };
        let subject_digest = exact
            .artifact()
            .map_err(|_| invalid())?
            .digest
            .as_str()
            .to_owned();
        let existing =
            sqlx::query("SELECT * FROM review_subjects WHERE subject_digest=$1 FOR UPDATE")
                .bind(&subject_digest)
                .fetch_optional(&mut **tx)
                .await
                .map_err(db)?;
        let old=sqlx::query("SELECT * FROM human_decision_requests WHERE (requester_id=$1 AND operation_id=$2) OR decision_id=$3 FOR UPDATE").bind(actor).bind(request.operation_id).bind(request.decision_id).fetch_all(&mut **tx).await.map_err(db)?;
        if !old.is_empty() {
            if old.len() != 1
                || old[0].get::<Uuid, _>("requester_id") != actor
                || old[0].get::<String, _>("request_digest") != digest
                || self.decode::<DecisionRequest>(
                    "request",
                    &request.decision_id.to_string(),
                    &old[0],
                )? != *request
            {
                return Err(conflict());
            }
            Self::owner(tx, actor).await?;
            if let Some(pin) = principal {
                if pin.id() != actor {
                    return Err(forbidden());
                }
                pin.fence(tx).await?;
            }
            return Ok(());
        }
        if !result.is_current_report_head {
            return Err(conflict());
        }
        Self::owner(tx, actor).await?;
        if let Some(pin) = principal {
            if pin.id() != actor {
                return Err(forbidden());
            }
            pin.fence(tx).await?;
        }
        // Stable sorted order avoids inversions when eligible sets overlap.
        let mut owners = request.eligible_owners.clone();
        owners.sort();
        for owner in owners {
            Self::owner(tx, owner).await?;
        }
        self.project(tx, &exact, request.eligibility_project_revision)
            .await?;
        prepared.retention.validate_current(&self.state, &source)?;
        if request.expires_at_ms <= now(&self.state)?
            || request.review_policy != exact.validation_policy
        {
            return Err(conflict());
        }
        let body = encode(request)?;
        let subject_body = encode(&exact)?;
        sqlx::query("INSERT INTO review_lineage_budget VALUES($1,$2,$3,0) ON CONFLICT DO NOTHING")
            .bind(exact.project_id)
            .bind(exact.task.workspace_id)
            .bind(exact.task.task_id)
            .execute(&mut **tx)
            .await
            .map_err(db)?;
        let used:i64=sqlx::query_scalar("SELECT retained_operations FROM review_lineage_budget WHERE project_id=$1 AND workspace_id=$2 AND task_id=$3 FOR UPDATE").bind(exact.project_id).bind(exact.task.workspace_id).bind(exact.task.task_id).fetch_one(&mut **tx).await.map_err(db)?;
        if let Some(ref row) = existing
            && (encode(&self.decode_subject(&subject_digest, row)?)? != encode(&exact)?
                || row.get::<i64, _>("decision_count") >= 16)
        {
            return Err(conflict());
        }
        if exact.profile() == SubjectProfile::SourceAndEffects {
            let questions: i64 = sqlx::query_scalar("SELECT count(*) FROM human_decision_requests d JOIN review_subjects s USING(subject_digest) JOIN profiled_result_reports r ON r.report_id=s.report_id WHERE r.attempt_id=$1 AND s.subject_profile='source_and_effects_review_v1'")
                .bind(exact.binding.attempt_id).fetch_one(&mut **tx).await.map_err(db)?;
            if questions >= 16 {
                return Err(AppError::PayloadTooLarge(
                    "combined review attempt quota exhausted".into(),
                ));
            }
        }
        // Reserve final receipt+outbox envelope capacity at initial admission;
        // finality never needs to exceed the retained global payload budget.
        let charge = body.len()
            + if existing.is_none() {
                subject_body.len()
            } else {
                0
            }
            + 2 * MAX_BYTES
            + 256;
        if used >= 256
            || budget.get::<i64, _>("retained_operations") >= 10000
            || budget
                .get::<i64, _>("retained_bytes")
                .checked_add(charge as i64)
                .is_none_or(|n| n > 67108864)
        {
            return Err(AppError::PayloadTooLarge(
                "review retained budget exhausted".into(),
            ));
        }
        if exact.profile() == SubjectProfile::SourceAndEffects {
            sqlx::query("SELECT set_config('aq.settlement_owner','aq.settlement-owner.1',true)")
                .execute(&mut **tx)
                .await
                .map_err(db)?;
        }
        if existing.is_none() {
            let aad_kind = if exact.profile() == SubjectProfile::Code {
                "subject"
            } else {
                "combined-subject"
            };
            let s = self.seal(aad_kind, &subject_digest, &subject_body)?;
            sqlx::query("INSERT INTO review_subjects(subject_digest,report_id,project_id,workspace_id,task_id,sealed_version,key_id,nonce,ciphertext,subject_profile) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)").bind(&subject_digest).bind(prepared.report_id).bind(exact.project_id).bind(exact.task.workspace_id).bind(exact.task.task_id).bind(s.version as i32).bind(s.key_id).bind(s.nonce).bind(s.ciphertext).bind(exact.profile().name()).execute(&mut **tx).await.map_err(db)?;
        }
        sqlx::query(
            "UPDATE review_subjects SET decision_count=decision_count+1 WHERE subject_digest=$1",
        )
        .bind(&subject_digest)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
        let s = self.seal("request", &request.decision_id.to_string(), &body)?;
        sqlx::query("INSERT INTO human_decision_requests(decision_id,requester_id,operation_id,request_digest,subject_digest,decision_revision,state,expires_at_ms,sealed_version,key_id,nonce,ciphertext) VALUES($1,$2,$3,$4,$5,1,'pending',$6,$7,$8,$9,$10)").bind(request.decision_id).bind(actor).bind(request.operation_id).bind(digest).bind(subject_digest).bind(request.expires_at_ms as i64).bind(s.version as i32).bind(s.key_id).bind(s.nonce).bind(s.ciphertext).execute(&mut **tx).await.map_err(db)?;
        sqlx::query(
            "INSERT INTO human_decision_submissions(decision_id,submitted_at_ms) VALUES($1,$2)",
        )
        .bind(request.decision_id)
        .bind(now(&self.state)? as i64)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
        sqlx::query("UPDATE review_lineage_budget SET retained_operations=retained_operations+1 WHERE project_id=$1 AND workspace_id=$2 AND task_id=$3").bind(exact.project_id).bind(exact.task.workspace_id).bind(exact.task.task_id).execute(&mut **tx).await.map_err(db)?;
        sqlx::query("UPDATE review_owner_budget SET retained_operations=retained_operations+1,retained_bytes=retained_bytes+$1 WHERE singleton").bind(charge as i64).execute(&mut **tx).await.map_err(db)?;
        Ok(())
    }
    /// Authenticated human answer only. New answers check current B1 head,
    /// project revision and current eligible owner after lock waits. Exact old
    /// receipt replay never creates a new vote or restamps current authority.
    pub async fn resolve(&self, actor: Uuid, answer: &ResolveRequest) -> Result<DecisionReceipt> {
        self.resolve_subject(actor, answer, SubjectProfile::Code, None, None)
            .await
    }
    async fn resolve_subject(
        &self,
        actor: Uuid,
        answer: &ResolveRequest,
        profile: SubjectProfile,
        expected_subject: Option<&ArtifactRef>,
        principal: Option<&crate::profiled_execution::settlement::actor::ActorPin>,
    ) -> Result<DecisionReceipt> {
        answer.validate().map_err(|_| invalid())?;
        let answer_digest = hash(&json!({"actor":actor,"answer":answer}))?;
        let mut tx = self.tx().await?;
        Self::budget(&mut tx).await?;
        // Read immutable identities without a decision row lock. All operations
        // take common authority before subject/decision; correction takes only
        // common authority/head and cannot form a decision→attempt cycle.
        let ids=sqlx::query("SELECT s.report_id,s.subject_digest FROM human_decision_requests d JOIN review_subjects s USING(subject_digest) WHERE d.decision_id=$1").bind(answer.decision_id).fetch_optional(&mut *tx).await.map_err(db)?.ok_or_else(forbidden)?;
        let report_id: Uuid = ids.get("report_id");
        let subject_digest: String = ids.get("subject_digest");
        let result =
            crate::profiled_execution::results::load_locked(&self.state, &mut tx, report_id)
                .await?;
        let subject_row =
            sqlx::query("SELECT * FROM review_subjects WHERE subject_digest=$1 FOR SHARE")
                .bind(&subject_digest)
                .fetch_one(&mut *tx)
                .await
                .map_err(db)?;
        let subject = self.decode_subject(&subject_digest, &subject_row)?;
        if subject.profile() != profile
            || expected_subject
                .is_some_and(|expected| subject.artifact().as_ref().ok() != Some(expected))
        {
            return Err(forbidden());
        }
        if subject
            .artifact()
            .map_err(|_| AppError::Internal)?
            .digest
            .as_str()
            != subject_digest
            || subject.result.report_id != report_id
            || subject.result.record_receipt != result.receipt_artifact
        {
            return Err(AppError::Internal);
        }
        let row =
            sqlx::query("SELECT * FROM human_decision_requests WHERE decision_id=$1 FOR UPDATE")
                .bind(answer.decision_id)
                .fetch_one(&mut *tx)
                .await
                .map_err(db)?;
        let request: DecisionRequest =
            self.decode("request", &answer.decision_id.to_string(), &row)?;
        request.validate().map_err(|_| AppError::Internal)?;
        if request.subject != subject.artifact().map_err(|_| AppError::Internal)?
            || hash(&request)? != row.get::<String, _>("request_digest")
        {
            return Err(AppError::Internal);
        }
        let old=sqlx::query("SELECT * FROM human_decision_finality WHERE decision_id=$1 OR (operation_id=$2 AND actor_id=$3) FOR SHARE").bind(answer.decision_id).bind(answer.operation_id).bind(actor).fetch_all(&mut *tx).await.map_err(db)?;
        if !old.is_empty() {
            if old.len() != 1
                || old[0].get::<Uuid, _>("decision_id") != answer.decision_id
                || old[0].get::<Option<Uuid>, _>("actor_id") != Some(actor)
                || old[0].get::<Uuid, _>("operation_id") != answer.operation_id
                || old[0].get::<String, _>("request_digest") != answer_digest
            {
                return Err(conflict());
            }
            let receipt: DecisionReceipt =
                self.decode("finality", &answer.decision_id.to_string(), &old[0])?;
            receipt
                .validate_for(&request)
                .map_err(|_| AppError::Internal)?;
            if hash(&receipt)? != old[0].get::<String, _>("receipt_digest") {
                return Err(AppError::Internal);
            }
            Self::owner(&mut tx, actor).await?;
            if let Some(pin) = principal {
                if pin.id() != actor {
                    return Err(forbidden());
                }
                pin.fence(&mut tx).await?;
                if profile == SubjectProfile::SourceAndEffects
                    && (old[0]
                        .get::<Option<String>, _>("combined_actor_digest")
                        .as_deref()
                        != Some(pin.identity_digest()?.as_str())
                        || old[0]
                            .get::<Option<String>, _>("combined_actor_revision")
                            .as_deref()
                            != Some(pin.revision()))
                {
                    return Err(forbidden());
                }
            }
            tx.commit().await.map_err(db)?;
            return Ok(receipt);
        }
        if !result.is_current_report_head
            || answer.expected_decision_revision != 1
            || row.get::<i64, _>("decision_revision") != 1
            || row.get::<String, _>("state") != "pending"
        {
            return Err(conflict());
        }
        if !request.eligible_owners.contains(&actor) {
            return Err(forbidden());
        }
        if answer.choice == Choice::Approve && !subject.may_approve() {
            return Err(conflict());
        }
        Self::owner(&mut tx, actor).await?;
        if let Some(pin) = principal {
            if pin.id() != actor {
                return Err(forbidden());
            }
            pin.fence(&mut tx).await?;
        }
        self.project(&mut tx, &subject, request.eligibility_project_revision)
            .await?;
        let at = now(&self.state)?;
        if at >= request.expires_at_ms {
            return Err(conflict());
        }
        let receipt = DecisionReceipt {
            schema_version: 1,
            decision_id: answer.decision_id,
            decision_revision: 2,
            subject: request.subject.clone(),
            request_digest: wire::digest(&request).map_err(|_| AppError::Internal)?,
            eligibility_project_revision: request.eligibility_project_revision,
            review_policy: request.review_policy.clone(),
            recorded_at_ms: at,
            finality: DecisionFinality::HumanAnswer {
                owner_user_id: actor,
                operation_id: answer.operation_id,
                choice: answer.choice,
                comment_digest: answer
                    .comment
                    .as_ref()
                    .map(|s| aq_session_protocol::Digest::of(s.as_bytes())),
            },
        };
        receipt
            .validate_for(&request)
            .map_err(|_| AppError::Internal)?;
        let receipt_digest = hash(&receipt)?;
        let s = self.seal(
            "finality",
            &answer.decision_id.to_string(),
            &encode(&receipt)?,
        )?;
        if profile == SubjectProfile::SourceAndEffects {
            sqlx::query("SELECT set_config('aq.settlement_owner','aq.settlement-owner.1',true)")
                .execute(&mut *tx)
                .await
                .map_err(db)?;
        }
        let combined_actor = if profile == SubjectProfile::SourceAndEffects {
            Some(principal.ok_or_else(forbidden)?)
        } else {
            None
        };
        sqlx::query("INSERT INTO human_decision_finality(decision_id,actor_id,operation_id,request_digest,receipt_digest,sealed_version,key_id,nonce,ciphertext,combined_actor_digest,combined_actor_revision) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)").bind(answer.decision_id).bind(actor).bind(answer.operation_id).bind(answer_digest).bind(&receipt_digest).bind(s.version as i32).bind(s.key_id).bind(s.nonce).bind(s.ciphertext).bind(combined_actor.map(|p|p.identity_digest()).transpose()?).bind(combined_actor.map(|p|p.revision())).execute(&mut *tx).await.map_err(db)?;
        let event = json!({"decision":answer.decision_id.to_string(),"outcome":match answer.choice {Choice::Approve=>"approved",Choice::Reject=>"rejected"},"output":{"receipt":receipt.artifact().map_err(|_|AppError::Internal)?,"subject":request.subject}});
        let s = self.seal("outbox", &answer.decision_id.to_string(), &encode(&event)?)?;
        sqlx::query("INSERT INTO review_owner_outbox(event_id,decision_id,producer,receipt_digest,sealed_version,key_id,nonce,ciphertext) VALUES($1,$1,'aq.human.decisions@1',$2,$3,$4,$5,$6)").bind(answer.decision_id).bind(receipt_digest).bind(s.version as i32).bind(s.key_id).bind(s.nonce).bind(s.ciphertext).execute(&mut *tx).await.map_err(db)?;
        sqlx::query("UPDATE human_decision_requests SET decision_revision=2,state='answered' WHERE decision_id=$1 AND decision_revision=1").bind(answer.decision_id).execute(&mut *tx).await.map_err(db)?;
        tx.commit().await.map_err(db)?;
        Ok(receipt)
    }
    /// Current requester can cancel a pending question; cancellation cannot
    /// retract an answer. No worker or playbook may impersonate a human answer.
    pub async fn cancel(
        &self,
        actor: Uuid,
        operation: Uuid,
        decision: Uuid,
        expected: u64,
    ) -> Result<DecisionReceipt> {
        self.finish_unanswered(Some(actor), operation, decision, expected, false)
            .await
    }
    /// Internal maintenance only, never an expiry flag accepted from JSON.
    pub async fn expire(&self, decision: Uuid) -> Result<DecisionReceipt> {
        self.finish_unanswered(None, decision, decision, 1, true)
            .await
    }
    pub(crate) async fn cancel_in_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        actor: Uuid,
        operation: Uuid,
        decision: Uuid,
    ) -> Result<DecisionReceipt> {
        self.finish_unanswered_in_tx(tx, Some(actor), operation, decision, 1, false)
            .await
    }
    async fn finish_unanswered(
        &self,
        actor: Option<Uuid>,
        operation: Uuid,
        decision: Uuid,
        expected: u64,
        expiry: bool,
    ) -> Result<DecisionReceipt> {
        let mut tx = self.tx().await?;
        let receipt = self
            .finish_unanswered_in_tx(&mut tx, actor, operation, decision, expected, expiry)
            .await?;
        tx.commit().await.map_err(db)?;
        Ok(receipt)
    }
    async fn finish_unanswered_in_tx(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        actor: Option<Uuid>,
        operation: Uuid,
        decision: Uuid,
        expected: u64,
        expiry: bool,
    ) -> Result<DecisionReceipt> {
        if decision.is_nil() || operation.is_nil() || expected != 1 {
            return Err(invalid());
        }
        let digest = hash(
            &json!({"actor":actor,"operation":operation,"decision":decision,"expected":expected,"expiry":expiry}),
        )?;
        Self::budget(tx).await?;
        let ids=sqlx::query("SELECT s.report_id,s.subject_digest FROM human_decision_requests d JOIN review_subjects s USING(subject_digest) WHERE d.decision_id=$1").bind(decision).fetch_optional(&mut **tx).await.map_err(db)?.ok_or_else(forbidden)?;
        let report_id: Uuid = ids.get("report_id");
        let subject_digest: String = ids.get("subject_digest");
        let result =
            crate::profiled_execution::results::load_locked(&self.state, tx, report_id).await?;
        let sr = sqlx::query("SELECT * FROM review_subjects WHERE subject_digest=$1 FOR SHARE")
            .bind(&subject_digest)
            .fetch_one(&mut **tx)
            .await
            .map_err(db)?;
        let subject = self.decode_subject(&subject_digest, &sr)?;
        if subject.result.report_id != report_id
            || subject.result.record_receipt != result.receipt_artifact
            || subject
                .artifact()
                .map_err(|_| AppError::Internal)?
                .digest
                .as_str()
                != subject_digest
        {
            return Err(AppError::Internal);
        }
        let row =
            sqlx::query("SELECT * FROM human_decision_requests WHERE decision_id=$1 FOR UPDATE")
                .bind(decision)
                .fetch_one(&mut **tx)
                .await
                .map_err(db)?;
        let request: DecisionRequest = self.decode("request", &decision.to_string(), &row)?;
        request.validate().map_err(|_| AppError::Internal)?;
        if hash(&request)? != row.get::<String, _>("request_digest")
            || request.subject != subject.artifact().map_err(|_| AppError::Internal)?
        {
            return Err(AppError::Internal);
        }
        if let Some(actor) = actor {
            if row.get::<Uuid, _>("requester_id") != actor {
                return Err(forbidden());
            }
            Self::owner(tx, actor).await?;
        }
        if let Some(old) =
            sqlx::query("SELECT * FROM human_decision_finality WHERE decision_id=$1 FOR SHARE")
                .bind(decision)
                .fetch_optional(&mut **tx)
                .await
                .map_err(db)?
        {
            if old.get::<String, _>("request_digest") != digest
                || old.get::<Uuid, _>("operation_id") != operation
                || old.get::<Option<Uuid>, _>("actor_id") != actor
            {
                return Err(conflict());
            }
            let receipt: DecisionReceipt = self.decode("finality", &decision.to_string(), &old)?;
            receipt
                .validate_for(&request)
                .map_err(|_| AppError::Internal)?;
            if hash(&receipt)? != old.get::<String, _>("receipt_digest") {
                return Err(AppError::Internal);
            }
            return Ok(receipt);
        }
        if row.get::<i64, _>("decision_revision") != 1 || row.get::<String, _>("state") != "pending"
        {
            return Err(conflict());
        }
        let at = now(&self.state)?;
        if expiry != (at >= request.expires_at_ms) {
            return Err(conflict());
        }
        let finality = if expiry {
            DecisionFinality::Expired {}
        } else {
            DecisionFinality::Cancelled {
                operation_id: operation,
            }
        };
        let receipt = DecisionReceipt {
            schema_version: 1,
            decision_id: decision,
            decision_revision: 2,
            subject: request.subject.clone(),
            request_digest: wire::digest(&request).map_err(|_| AppError::Internal)?,
            eligibility_project_revision: request.eligibility_project_revision,
            review_policy: request.review_policy.clone(),
            recorded_at_ms: at,
            finality,
        };
        receipt
            .validate_for(&request)
            .map_err(|_| AppError::Internal)?;
        let receipt_digest = hash(&receipt)?;
        let s = self.seal("finality", &decision.to_string(), &encode(&receipt)?)?;
        sqlx::query("INSERT INTO human_decision_finality(decision_id,actor_id,operation_id,request_digest,receipt_digest,sealed_version,key_id,nonce,ciphertext) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9)").bind(decision).bind(actor).bind(operation).bind(digest).bind(&receipt_digest).bind(s.version as i32).bind(s.key_id).bind(s.nonce).bind(s.ciphertext).execute(&mut **tx).await.map_err(db)?;
        let outcome = if expiry { "expired" } else { "cancelled" };
        let event = json!({"decision":decision.to_string(),"outcome":outcome,"output":{"receipt":receipt.artifact().map_err(|_|AppError::Internal)?,"subject":request.subject}});
        let s = self.seal("outbox", &decision.to_string(), &encode(&event)?)?;
        sqlx::query("INSERT INTO review_owner_outbox(event_id,decision_id,producer,receipt_digest,sealed_version,key_id,nonce,ciphertext) VALUES($1,$1,'aq.human.decisions@1',$2,$3,$4,$5,$6)").bind(decision).bind(receipt_digest).bind(s.version as i32).bind(s.key_id).bind(s.nonce).bind(s.ciphertext).execute(&mut **tx).await.map_err(db)?;
        sqlx::query("UPDATE human_decision_requests SET decision_revision=2,state=$2 WHERE decision_id=$1 AND decision_revision=1").bind(decision).bind(if expiry {"expired"} else {"cancelled"}).execute(&mut **tx).await.map_err(db)?;
        Ok(receipt)
    }
    pub async fn inspect(&self, actor: Uuid, decision: Uuid) -> Result<serde_json::Value> {
        self.inspect_subject(actor, decision, SubjectProfile::Code, None)
            .await
    }
    async fn inspect_subject(
        &self,
        actor: Uuid,
        decision: Uuid,
        profile: SubjectProfile,
        principal: Option<&crate::profiled_execution::settlement::actor::ActorPin>,
    ) -> Result<serde_json::Value> {
        let mut tx = self.tx().await?;
        let ids=sqlx::query("SELECT s.report_id,s.subject_digest FROM human_decision_requests d JOIN review_subjects s USING(subject_digest) WHERE d.decision_id=$1").bind(decision).fetch_optional(&mut *tx).await.map_err(db)?.ok_or_else(forbidden)?;
        let report_id: Uuid = ids.get("report_id");
        let subject_digest: String = ids.get("subject_digest");
        let result =
            crate::profiled_execution::results::load_locked(&self.state, &mut tx, report_id)
                .await?;
        let sr = sqlx::query("SELECT * FROM review_subjects WHERE subject_digest=$1 FOR SHARE")
            .bind(&subject_digest)
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?;
        let subject = self.decode_subject(&subject_digest, &sr)?;
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
        if hash(&request)? != row.get::<String, _>("request_digest")
            || request.subject != subject.artifact().map_err(|_| AppError::Internal)?
            || subject.result.record_receipt != result.receipt_artifact
        {
            return Err(AppError::Internal);
        }
        if row.get::<Uuid, _>("requester_id") != actor && !request.eligible_owners.contains(&actor)
        {
            return Err(forbidden());
        }
        Self::owner(&mut tx, actor).await?;
        if let Some(pin) = principal {
            if pin.id() != actor {
                return Err(forbidden());
            }
            pin.fence(&mut tx).await?;
        }
        let finality =
            sqlx::query("SELECT * FROM human_decision_finality WHERE decision_id=$1 FOR SHARE")
                .bind(decision)
                .fetch_optional(&mut *tx)
                .await
                .map_err(db)?;
        let receipt = finality
            .as_ref()
            .map(|r| self.decode::<DecisionReceipt>("finality", &decision.to_string(), r))
            .transpose()?;
        if let Some(ref receipt) = receipt {
            receipt
                .validate_for(&request)
                .map_err(|_| AppError::Internal)?;
            if hash(receipt)?
                != finality
                    .as_ref()
                    .unwrap()
                    .get::<String, _>("receipt_digest")
            {
                return Err(AppError::Internal);
            }
        }
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
    async fn completion_subject_in_tx(
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
            || row.get::<i64, _>("decision_revision") != 2
        {
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
        let receipt: DecisionReceipt = self.decode("finality", &decision.to_string(), &finality)?;
        receipt
            .validate_for(&request)
            .map_err(|_| AppError::Internal)?;
        if hash(&receipt)? != finality.get::<String, _>("receipt_digest") {
            return Err(AppError::Internal);
        }
        let outbox =
            sqlx::query("SELECT * FROM review_owner_outbox WHERE decision_id=$1 FOR UPDATE")
                .bind(decision)
                .fetch_one(&mut **tx)
                .await
                .map_err(db)?;
        let event: serde_json::Value = self.decode("outbox", &decision.to_string(), &outbox)?;
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
        if outbox.get::<Uuid, _>("event_id") != decision
            || outbox.get::<String, _>("producer") != "aq.human.decisions@1"
            || outbox.get::<String, _>("receipt_digest") != hash(&receipt)?
            || event
                != json!({"decision":decision.to_string(),"outcome":outcome,"output":{"receipt":receipt.artifact().map_err(|_|AppError::Internal)?,"subject":request.subject}})
        {
            return Err(AppError::Internal);
        }
        Ok((subject, event, receipt))
    }
    async fn project(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        subject: &ReviewSubject,
        expected: u64,
    ) -> Result<()> {
        let row = sqlx::query(
            "SELECT revision,binding FROM project_execution_bindings WHERE project_id=$1 FOR SHARE",
        )
        .bind(subject.project_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?
        .ok_or_else(conflict)?;
        let binding: crate::project_bindings::domain::ProjectBinding =
            serde_json::from_value(row.get("binding")).map_err(|_| AppError::Internal)?;
        if row.get::<i64, _>("revision") as u64 != expected
            || !binding.enabled
            || binding.project_id != subject.project_id
            || binding.graph_workspace_id != subject.task.workspace_id
            || binding.repository_id != subject.source.repository_id
            || binding.repository_binding_generation != subject.source.binding_generation
        {
            return Err(conflict());
        }
        Ok(())
    }
}
pub(crate) fn subject(
    result: &crate::profiled_execution::results::RecordedResult,
    source: &crate::source_retention::RecordedSourceConfirmation,
) -> Result<ReviewSubject> {
    let spec = &result.spec;
    if result.outcome.review != aq_admission::outcome::ReviewRequirement::HumanOwner {
        return Err(conflict());
    }
    let object = |v: &aq_worker_protocol::ObjectId| ObjectId {
        format: ObjectFormat::Sha1,
        hex: v.hex.clone(),
    };
    let subject = ReviewSubject {
        schema_version: 1,
        project_id: spec.lifecycle.project_id,
        original_project_revision: spec.project_revision,
        task: TaskRevision {
            workspace_id: spec.task.workspace_id,
            task_id: spec.task.task_id,
            version: spec.task.version,
            content_revision: spec.task.content_revision,
        },
        binding: result.report.binding.clone(),
        execution_spec: spec.artifact().map_err(|_| AppError::Internal)?,
        expected_outcome: spec.outcome_artifact.clone(),
        task_observation: spec.task_observation.clone(),
        task_context: spec.task_context.clone(),
        validation_policy: result.outcome.validation_policy.clone(),
        result: ResultPin {
            report_id: result.report.report_id,
            report_digest: result.receipt.report_digest.clone(),
            record_receipt: result.receipt_artifact.clone(),
            stopped_receipt: result.receipt.stopped_receipt.clone(),
        },
        source: SourcePin {
            repository_id: source.receipt.repository_id,
            binding_generation: source.receipt.binding_generation,
            base: object(&source.manifest.source.base),
            resulting_object: object(&source.receipt.source),
            input_artifact: spec.source_artifact.clone(),
            resulting_artifact: ArtifactRef {
                id: Text::new(format!(
                    "aq.git-import-manifest/{}",
                    source.manifest.artifact_id
                ))
                .map_err(|_| invalid())?,
                version: Text::new("1").map_err(|_| invalid())?,
                digest: serde_json::from_value(json!(source.receipt.manifest_digest))
                    .map_err(|_| AppError::Internal)?,
            },
            source_receipt_id: source.receipt.receipt_id,
            retention_receipt: reference(
                format!("aq.git-import-receipt/{}", source.receipt.receipt_id),
                &source.acknowledgement,
            )?,
        },
        criteria: result
            .outcome
            .acceptance_criteria
            .iter()
            .map(|c| Text::new(c.id.as_str()))
            .collect::<std::result::Result<_, _>>()
            .map_err(|_| AppError::Internal)?,
    };
    subject.validate().map_err(|_| AppError::Internal)?;
    Ok(subject)
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelRequest {
    operation_id: Uuid,
    expected_decision_revision: u64,
}
/// Optional injection seam only. The ordinary production admin builder does
/// not mount these routes until its configured owner/protection is supplied.
/// Caller mounts below the existing identity/CSRF/maintenance layers.
pub fn admin_routes(service: Arc<Service>) -> axum::Router<AppState> {
    use axum::{
        extract::DefaultBodyLimit,
        routing::{get, post},
    };
    axum::Router::new()
        .route("/api/reviews/decisions", get(inbox::inbox_http))
        .route("/api/reviews/decisions/{id}", get(inspect_http))
        .route("/api/reviews/decisions/{id}/resolve", post(resolve_http))
        .route("/api/reviews/decisions/{id}/cancel", post(cancel_http))
        .route("/api/reviews/decisions/{id}/defer", post(view::defer_http))
        .route(
            "/api/reviews/decisions/{id}/evidence/{evidence}/viewed",
            post(view::viewed_http),
        )
        .merge(combined_http::routes())
        .layer(DefaultBodyLimit::max(16384))
        .layer(axum::Extension(service))
}
async fn inspect_http(
    crate::authz::RequireOwner(actor): crate::authz::RequireOwner,
    axum::Extension(service): axum::Extension<Arc<Service>>,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Result<axum::Json<serde_json::Value>> {
    Ok(axum::Json(service.decision_view(&actor, id).await?))
}
async fn resolve_http(
    crate::authz::RequireOwner(actor): crate::authz::RequireOwner,
    axum::Extension(service): axum::Extension<Arc<Service>>,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    crate::admin_api::ApiJson(answer): crate::admin_api::ApiJson<ResolveRequest>,
) -> Result<axum::Json<DecisionReceipt>> {
    if id != answer.decision_id {
        return Err(invalid());
    }
    answer.validate().map_err(|_| invalid())?;
    let pin =
        crate::profiled_execution::settlement::actor::ActorPin::observe(&service.state, &actor)
            .await?;
    let view = service
        .inspect_subject(actor.id, id, SubjectProfile::Code, Some(&pin))
        .await?;
    let subject = serde_json::from_value(view["request"]["subject"].clone())
        .map_err(|_| AppError::Internal)?;
    let payload = action::Resolve::from_answer(answer, subject);
    let mut registry = crate::dashboard_api::actions::Registry::default();
    registry
        .register(action::ResolveDecision(Some(service.clone())))
        .map_err(crate::dashboard_api::actions::existing::legacy_error)?;
    let operation = crate::dashboard_api::actions::submit_legacy(
        &service.state,
        &registry,
        &actor,
        payload.request(),
    )
    .await
    .map_err(crate::dashboard_api::actions::existing::legacy_error)?;
    if operation.disposition == crate::dashboard_api::actions::Disposition::Conflict {
        return Err(conflict());
    }
    let view = service
        .inspect_subject(actor.id, id, SubjectProfile::Code, Some(&pin))
        .await?;
    let receipt: DecisionReceipt =
        serde_json::from_value(view["receipt"].clone()).map_err(|_| {
            if operation.disposition == crate::dashboard_api::actions::Disposition::Conflict {
                conflict()
            } else {
                AppError::IdentityUnavailable("decision operation is unresolved".into())
            }
        })?;
    if !matches!(receipt.finality,DecisionFinality::HumanAnswer{owner_user_id,operation_id,..} if owner_user_id==actor.id && operation_id==operation.operation_id)
    {
        return Err(conflict());
    }
    Ok(axum::Json(receipt))
}
async fn cancel_http(
    crate::authz::RequireOwner(actor): crate::authz::RequireOwner,
    axum::Extension(service): axum::Extension<Arc<Service>>,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    crate::admin_api::ApiJson(request): crate::admin_api::ApiJson<CancelRequest>,
) -> Result<axum::Json<DecisionReceipt>> {
    Ok(axum::Json(
        service
            .cancel(
                actor.id,
                request.operation_id,
                id,
                request.expected_decision_revision,
            )
            .await?,
    ))
}
