//! Typed admission-observation bytes, retained under their actual Source role.
//! Context proposal receipts are never relabeled. This grants no execution.
use super::{Observation, refused};
use crate::{AppState, error::AppError};
use aq_admission::{ArtifactKind, RetainedArtifact, SourceAdmission};
use aq_delivery::source_checks::observation;
use uuid::Uuid;
/// Structural owner check over the original actual public Git observations.
/// Fresh live authorization/retention remains a separate owner fence.
pub fn validate_body(bytes: &[u8], project: Uuid, now: u64) -> Result<SourceAdmission, AppError> {
    observation::verify(bytes, project, now)
        .map(|v| v.admission)
        .ok_or_else(refused)
}
pub fn validate_stored_body(bytes: &[u8], project: Uuid) -> Result<SourceAdmission, AppError> {
    observation::verify_stored(bytes, project)
        .map(|v| v.admission)
        .ok_or_else(refused)
}
pub async fn retain_admission(
    state: &AppState,
    operation: Uuid,
    observation: &Observation,
    retain_until_ms: u64,
) -> Result<RetainedArtifact, AppError> {
    retain_for(state, operation, observation, retain_until_ms, None).await
}
pub(in crate::profiled_execution) async fn retain_for(
    state: &AppState,
    operation: Uuid,
    observation: &Observation,
    retain_until_ms: u64,
    stage: Option<Uuid>,
) -> Result<RetainedArtifact, AppError> {
    let verified = observation::verify_stored(&observation.bytes, observation.admission.project_id)
        .ok_or_else(refused)?;
    let artifact = observation::artifact(&observation.bytes).ok_or_else(refused)?;
    let identity = !operation.is_nil()
        && artifact == observation.artifact
        && verified.retention_handle == observation.retention_handle;
    if !identity
        || serde_json::to_value(&verified.admission).map_err(|_| refused())?
            != serde_json::to_value(&observation.admission).map_err(|_| refused())?
    {
        return Err(refused());
    }
    super::super::artifacts::retain_for(
        state,
        super::super::artifacts::Retain {
            artifact_id: operation,
            project_id: verified.admission.project_id,
            owner: super::super::artifacts::OwnerKind::SourceRetention,
            artifact,
            kind: ArtifactKind::Source,
            bytes: &observation.bytes,
            retain_until_ms,
        },
        stage,
    )
    .await
}
