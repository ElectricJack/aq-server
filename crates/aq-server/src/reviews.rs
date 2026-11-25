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
mod admission;
mod answer;
mod completion;
mod http;
mod inspection;
mod records;
use records::{LockedSubject, ReviewActor, ReviewScope, decision_event};
mod unanswered;
pub use http::admin_routes;
pub(crate) use unanswered::Cancellation;
pub mod combined;
mod combined_http;
mod inbox;
mod subject_owner;
mod view;
pub use aq_dashboard_contract::review_view::{
    DecisionKind, Impact, InboxItem, InboxOrder, InboxPage, InboxQuery, InboxState, RequestedBy,
    ReviewPayload,
};
pub use inbox::schema as inbox_contract_schema;
use subject_owner::{Profile as SubjectProfile, Subject as OwnedSubject};

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
        let valid_revision = row.get::<i64, _>("revision") as u64 == expected && binding.enabled;
        let same_task = binding.project_id == subject.project_id
            && binding.graph_workspace_id == subject.task.workspace_id;
        let same_source = binding.repository_id == subject.source.repository_id
            && binding.repository_binding_generation == subject.source.binding_generation;
        if !valid_revision {
            return Err(conflict());
        }
        if !same_task || !same_source {
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
