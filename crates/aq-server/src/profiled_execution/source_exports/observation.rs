use super::{Request, database, refused, unavailable};
use crate::{
    error::AppError,
    profiled_execution::{artifacts, config::ProjectPolicyRecord, handoff},
};
use aq_admission::SourceAdmission;
use serde_json::Value;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;
type Tx<'a> = Transaction<'a, Postgres>;
pub(super) async fn retained_observation(
    tx: &mut Tx<'_>,
    request: &Request,
) -> Result<artifacts::Retained, AppError> {
    let observation_id:Uuid=sqlx::query_scalar("SELECT artifact_id FROM admission_artifacts WHERE owner_key=$1 AND owner_kind='source_retention' AND descriptor->>'kind'='context' AND descriptor->'artifact'=$2 ORDER BY artifact_id LIMIT 1").bind(request.project_id.to_string()).bind(serde_json::to_value(&request.source_observation).map_err(|_|unavailable())?).fetch_optional(&mut **tx).await.map_err(database)?.ok_or_else(unavailable)?;
    let retained = artifacts::load_locked(tx, observation_id).await?;
    let owner = retained.project_id == request.project_id
        && retained.owner == artifacts::OwnerKind::SourceRetention;
    let descriptor = retained.descriptor.artifact == request.source_observation
        && retained.descriptor.kind == aq_admission::ArtifactKind::Context;
    if owner && descriptor {
        Ok(retained)
    } else {
        Err(unavailable())
    }
}
pub(super) async fn is_handoff(
    tx: &mut Tx<'_>,
    request: &Request,
    value: &Value,
) -> Result<bool, AppError> {
    // Spec 0007 S7 (D5): a handoff successor exports its predecessor's recorded
    // handoff candidate, held by the candidate handle, as an immutable source.
    let predecessor = value["evidence"]["handoff"]["predecessor"].as_str();
    let scope = (
        request.project_id,
        request.repository_id,
        request.binding_generation,
    );
    Ok(match predecessor.and_then(|p| Uuid::parse_str(p).ok()) {
        Some(p) => {
            handoff::export_is_handoff(tx, p, request).await?
                && aq_machine_control::export_scope(tx, p).await? == Some(scope)
        }
        None => false,
    })
}
pub(super) fn validate_observation(
    request: &Request,
    configured: &ProjectPolicyRecord,
    value: &Value,
    handoff: bool,
) -> Result<SourceAdmission, AppError> {
    let observation: SourceAdmission =
        serde_json::from_value(value["admission"].clone()).map_err(|_| unavailable())?;
    observation.validate().map_err(|_| unavailable())?;
    let scope = request.matches_admission_scope(&observation)
        && observation.policy == configured.artifact().map_err(|_| unavailable())?;
    let correct_object = scope && request.matches_resolved_object(&observation);
    let selection_matches = if !correct_object {
        false
    } else if handoff {
        matches!(&observation.selection, aq_admission::SourceSelection::Immutable {object} if object.hex==request.base.hex)
    } else {
        serde_json::to_value(&observation.selection).map_err(|_| unavailable())?
            == serde_json::to_value(&configured.source).map_err(|_| unavailable())?
    };
    if !selection_matches
        || value["retention_handle"] != serde_json::json!(request.source_retention_handle)
    {
        return Err(refused("source_export_observation_mismatch"));
    }
    Ok(observation)
}
