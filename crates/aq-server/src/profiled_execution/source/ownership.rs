//! Typed admission-observation bytes, retained under their actual Source role.
//! Context proposal receipts are never relabeled. This grants no execution.
use super::{Observation, hex40, refused, resolve_ref, verify_retention};
use crate::{AppState, error::AppError};
use aq_admission::{
    ArtifactKind, ArtifactRef, Digest, ObjectFormat, RetainedArtifact, SourceAdmission,
    SourceSelection, Text,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
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
    match &a.selection {
        SourceSelection::Immutable { object } => {
            if *object != a.resolved_object
                || body.evidence.reference_observation.is_some()
                || body.evidence.publication_head.is_some()
            {
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
}
pub fn validate_stored_body(bytes: &[u8], project: Uuid) -> Result<SourceAdmission, AppError> {
    if bytes.len() > 1_048_576 {
        return Err(refused());
    }
    let body: Body = serde_json::from_slice(bytes).map_err(|_| refused())?;
    let as_of = body
        .admission
        .expires_at_ms
        .checked_sub(1)
        .ok_or_else(|| refused())?;
    validate_body(bytes, project, as_of)
}
pub async fn retain_admission(
    state: &AppState,
    operation: Uuid,
    observation: &Observation,
    retain_until_ms: u64,
) -> Result<RetainedArtifact, AppError> {
    let verified = validate_stored_body(&observation.bytes, observation.admission.project_id)?;
    let artifact = ArtifactRef {
        id: Text::new(format!(
            "aq.source-admission-observation/{}",
            Digest::of(&observation.bytes).as_str()
        ))
        .map_err(|_| refused())?,
        version: Text::new("1").map_err(|_| refused())?,
        digest: Digest::of(&observation.bytes),
    };
    let value: Body = serde_json::from_slice(&observation.bytes).map_err(|_| refused())?;
    if operation.is_nil()
        || artifact != observation.artifact
        || value.retention_handle != observation.retention_handle
        || serde_json::to_value(&verified).map_err(|_| refused())?
            != serde_json::to_value(&observation.admission).map_err(|_| refused())?
    {
        return Err(refused());
    }
    super::super::artifacts::retain(
        state,
        super::super::artifacts::Retain {
            artifact_id: operation,
            project_id: verified.project_id,
            owner: super::super::artifacts::OwnerKind::SourceRetention,
            artifact,
            kind: ArtifactKind::Source,
            bytes: &observation.bytes,
            retain_until_ms,
        },
    )
    .await
}
