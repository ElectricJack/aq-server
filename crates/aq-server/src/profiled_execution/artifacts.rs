//! Immutable nonsecret inline bytes with an actual database retention receipt.
//! Retain outside node/project transactions; linking never acquires quota locks.
use crate::{
    AppState,
    error::{AppError, db},
};
use aq_admission::{ArtifactKind, RetainedArtifact};
use aq_profiled_state::inline_artifact::storage;
pub use aq_profiled_state::inline_artifact::{LinkScope, OwnerKind, Receipt, Retain, Retained};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;
#[track_caller]
fn refused() -> AppError {
    tracing::debug!(target: "aq_refusal", at = %std::panic::Location::caller(), "profiled refusal");
    AppError::Conflict("inline artifact owner unavailable or inconsistent".into())
}
fn now(state: &AppState) -> Result<u64, AppError> {
    u64::try_from((state.clock)().timestamp_millis()).map_err(|_| refused())
}
/// Revalidate both exact stored bytes and the immutable receipt. Caller owns
/// assignment/authorization and final time checks; this read grants neither.
pub async fn load_locked(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> Result<Retained, AppError> {
    let row = storage::row_locked(tx, id)
        .await
        .map_err(db)?
        .ok_or_else(refused)?;
    let retained = Retained::decode(row)
        .map_err(|_| AppError::Internal)?
        .ok_or_else(refused)?;
    let descriptor = &retained.descriptor;
    if descriptor.kind == ArtifactKind::Source {
        super::source::validate_body(
            &retained.bytes,
            retained.project_id,
            descriptor.retained_at_ms,
        )?;
    }
    descriptor
        .verify(&retained.bytes, descriptor.retained_at_ms)
        .map_err(|_| refused())?;
    Ok(retained)
}
/// Independent transaction. No external I/O, eviction or fabricated source
/// bundle receipt. The caller must not hold node/project locks while retaining.
pub async fn retain(state: &AppState, request: Retain<'_>) -> Result<RetainedArtifact, AppError> {
    retain_for(state, request, None).await
}
pub(super) async fn retain_for(
    state: &AppState,
    request: Retain<'_>,
    stage: Option<Uuid>,
) -> Result<RetainedArtifact, AppError> {
    validate_request(&request)?;
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    let exists = storage::lock_retention(&mut tx, request.artifact_id)
        .await
        .map_err(db)?;
    if let Some(staged) = exists {
        let old = load_locked(&mut tx, request.artifact_id).await?;
        if !request.matches(&old) || (staged && stage.is_none()) {
            return Err(refused());
        }
        old.descriptor
            .verify(&old.bytes, now(state)?)
            .map_err(|_| refused())?;
        pin(state, &mut tx, stage, request.artifact_id).await?;
        tx.commit().await.map_err(db)?;
        return Ok(old.descriptor);
    }
    let time = now(state)?;
    if request.definition_conflict(&mut tx).await.map_err(db)? {
        return Err(AppError::DefinitionIdentityConflict);
    }
    if request.kind == ArtifactKind::Source {
        super::source::validate_body(request.bytes, request.project_id, time)?;
    }
    let record = storage::Record::prepare(&request, time)
        .map_err(|_| AppError::Internal)?
        .ok_or_else(refused)?;
    record
        .insert_body(&mut tx, &request, stage.is_some())
        .await
        .map_err(|_| refused())?;
    let descriptor = record
        .insert_receipt(&mut tx, request.artifact_id)
        .await
        .map_err(db)?;
    pin(state, &mut tx, stage, request.artifact_id).await?;
    tx.commit().await.map_err(|_| refused())?;
    Ok(descriptor)
}
async fn pin(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    stage: Option<Uuid>,
    id: Uuid,
) -> Result<(), AppError> {
    if let Some(stage) = stage
        && !aq_history_caps::offer_stage::pin(tx, stage, id, now(state)?)
            .await
            .map_err(db)?
    {
        return Err(refused());
    }
    Ok(())
}
fn validate_request(request: &Retain<'_>) -> Result<(), AppError> {
    if !request.valid() {
        return Err(refused());
    }
    if request.kind == ArtifactKind::Source {
        let source = super::source::validate_stored_body(request.bytes, request.project_id)?;
        let artifact =
            aq_delivery::source_checks::observation::artifact(request.bytes).ok_or_else(refused)?;
        let scope =
            request.owner == OwnerKind::SourceRetention && source.project_id == request.project_id;
        if !scope || artifact != request.artifact {
            return Err(refused());
        }
    }
    Ok(())
}
/// Caller holds the exact node/attempt admission locks. Existing artifacts are
/// immutable; final offer links only this project's actual retained bytes.
pub async fn link_locked(
    tx: &mut Transaction<'_, Postgres>,
    scope: LinkScope,
    artifacts: &[Uuid],
) -> Result<Vec<RetainedArtifact>, AppError> {
    let ids = scope.ordered_ids(artifacts).ok_or_else(refused)?;
    let mut descriptors = Vec::with_capacity(ids.len());
    for id in ids {
        let item = load_locked(tx, id).await?;
        if !scope.covers(&item) {
            return Err(refused());
        }
        item.descriptor
            .verify(&item.bytes, scope.required.start)
            .map_err(|_| refused())?;
        storage::link(tx, scope.attempt, id).await.map_err(db)?;
        descriptors.push(item.descriptor);
    }
    Ok(descriptors)
}
