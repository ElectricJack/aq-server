//! Typed admission-observation bytes, retained under their actual Source role.
//! Context proposal receipts are never relabeled. This grants no execution.
use super::{Observation, refused};
use crate::{AppState, error::AppError};
use aq_admission::{ArtifactKind, RetainedArtifact, SourceAdmission};
use aq_delivery::source_checks::observation;
use uuid::Uuid;
<<<<<<< HEAD
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Body {
    schema_version: u32,
    admission: SourceAdmission,
    retention_handle: Uuid,
    evidence: Evidence,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Evidence {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reference_observation: Option<Value>,
    /// D1: the server-owned source head AQ published to the named ref.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    publication_head: Option<Value>,
    /// Spec 0007: the handoff whose saved commit this immutable source is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    handoff: Option<Value>,
    /// Spec 0016 §4: the synced, retained head of the followed named ref.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    main_sync: Option<Value>,
    commit_inspection: Value,
    retention_inspection: Value,
}
/// Structural owner check over the original actual public Git observations.
/// Fresh live authorization/retention remains a separate owner fence.
pub fn validate_body(bytes: &[u8], project: Uuid, now: u64) -> Result<SourceAdmission, AppError> {
    if bytes.is_empty() || bytes.len() > 1_048_576 || project.is_nil() || now == 0 {
        return Err(refused());
    }
    let body: Body = serde_json::from_slice(bytes).map_err(|_| refused())?;
    body.admission.validate().map_err(|_| refused())?;
    let a = &body.admission;
    if body.schema_version != 1
        || a.project_id != project
        || a.resolved_object.format != ObjectFormat::Sha1
        || body.retention_handle.is_nil()
        || now < a.observed_at_ms
        || now >= a.expires_at_ms
    {
        return Err(refused());
    }
    let inspected = &body.evidence.commit_inspection;
    if inspected["oid"] != serde_json::json!({"algorithm":"sha1","hex":a.resolved_object.hex})
        || inspected["tree"]["algorithm"] != "sha1"
        || !hex40(inspected["tree"]["hex"].as_str().unwrap_or(""))
    {
        return Err(refused());
    }
    if body.evidence.handoff.is_some() && !matches!(a.selection, SourceSelection::Immutable { .. })
    {
        return Err(refused());
    }
    if body.evidence.main_sync.is_some() && !matches!(a.selection, SourceSelection::NamedRef { .. })
    {
        return Err(refused());
    }
    match &a.selection {
        SourceSelection::Immutable { object } => {
            if *object != a.resolved_object
                || body.evidence.reference_observation.is_some()
                || body.evidence.publication_head.is_some()
            {
                return Err(refused());
            }
        }
        SourceSelection::NamedRef { reference } if body.evidence.main_sync.is_some() => {
            let (e, id, g) = (
                &body.evidence,
                a.repository_id,
                a.repository_binding_generation,
            );
            let scope = aq_delivery::main_sync::Scope::new(a.project_id, id, g, reference.as_str());
            let (hex, handle, at) = (
                &a.resolved_object.hex,
                body.retention_handle,
                a.observed_at_ms,
            );
            let proves =
                aq_delivery::main_sync::proves(e.main_sync.as_ref(), &scope, hex, handle, at);
            if e.reference_observation.is_some() || e.publication_head.is_some() || !proves {
                return Err(refused());
            }
        }
        SourceSelection::NamedRef { reference } if body.evidence.publication_head.is_some() => {
            let head: crate::profiled_execution::settlement::publication::SourceHead =
                serde_json::from_value(body.evidence.publication_head.clone().unwrap_or_default())
                    .map_err(|_| refused())?;
            if body.evidence.reference_observation.is_some()
                || !head.validate()
                || head.target != reference.as_str()
                || head.oid != a.resolved_object.hex
                || head.retention_handle != body.retention_handle
                || head.project_id != a.project_id
                || head.repository_id != a.repository_id
                || head.binding_generation != a.repository_binding_generation
            {
                return Err(refused());
            }
        }
        SourceSelection::NamedRef { reference } => {
            let record = body
                .evidence
                .reference_observation
                .as_ref()
                .ok_or_else(|| refused())?;
            if resolve_ref(
                record,
                reference.as_str(),
                a.repository_binding_generation,
                now,
            )? != a.resolved_object
            {
                return Err(refused());
            }
        }
    }
    verify_retention(
        &body.evidence.retention_inspection,
        body.retention_handle,
        a.repository_id,
        a.repository_binding_generation,
        &a.resolved_object,
    )?;
    Ok(body.admission)
=======
/// Structural owner check over the original actual public Git observations.
/// Fresh live authorization/retention remains a separate owner fence.
pub fn validate_body(bytes: &[u8], project: Uuid, now: u64) -> Result<SourceAdmission, AppError> {
    observation::verify(bytes, project, now)
        .map(|v| v.admission)
        .ok_or_else(refused)
>>>>>>> refs/remotes/central/main
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
