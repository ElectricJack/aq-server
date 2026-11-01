//! Actual post-Started stop evidence. This ends execution authority but keeps
//! the task, source retention and effect claims until their owners settle them.
use super::{journal, registration};
use crate::{
    AppState,
    error::AppError,
    execution::{
        authority::{self, Kind, Policy, hot::HotState},
        profiled::{Status, Stop},
    },
    worker_api::AuthenticatedNode,
};
use aq_admission::{
    Digest, SessionIdentity,
    stopped::{Receipt, Request},
    transport::Feature,
};
use sqlx::Row;
fn conflict() -> AppError {
    AppError::Conflict("stopped evidence conflicts with original launch owner".into())
}
fn db(_: sqlx::Error) -> AppError {
    AppError::Internal
}
fn canonical(value: &impl serde::Serialize) -> Result<Vec<u8>, AppError> {
    let mut value = serde_json::to_value(value).map_err(|_| AppError::Internal)?;
    value.sort_all_objects();
    serde_json::to_vec(&value).map_err(|_| AppError::Internal)
}
pub async fn record(
    state: &AppState,
    node: &AuthenticatedNode,
    request: &Request,
) -> Result<Receipt, AppError> {
    request
        .validate()
        .map_err(|_| AppError::BadRequest("invalid stopped report".into()))?;
    let binding = &request.evidence.binding;
    let SessionIdentity::Remote {
        node_id, boot_id, ..
    } = binding.session
    else {
        return Err(conflict());
    };
    if node.id() != node_id {
        return Err(AppError::Forbidden("stopped node mismatch".into()));
    }
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("SET LOCAL statement_timeout='3s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    crate::worker_api::lock_current_node(&mut tx, node).await?;
    let admitted = registration::admitted_runtime_locked(&mut tx, node_id, boot_id).await?;
    if admitted.request.runtime != request.runtime
        || admitted.qualification != request.qualification
        || !admitted
            .request
            .features
            .contains(&Feature::DurableHostJournalV1)
        || !admitted
            .request
            .features
            .contains(&Feature::HostStopEvidenceV1)
    {
        return Err(conflict());
    }
    let view =
        authority::load_locked(state, &mut tx, &binding.session, Policy::IdentityOnly).await?;
    if view.kind != Kind::ProfiledV1
        || view.offer_id != binding.offer_id
        || view.launch_manifest_digest.as_ref() != Some(&binding.launch_manifest_digest)
        || view.execution_spec_digest != binding.execution_spec_digest.as_str()
    {
        return Err(conflict());
    }
    let row=sqlx::query("SELECT state,record_revision,authority_revision,grant_revision FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE")
        .bind(binding.attempt_id).fetch_optional(&mut *tx).await.map_err(db)?.ok_or_else(conflict)?;
    let mut hot: HotState =
        serde_json::from_value(row.get("state")).map_err(|_| AppError::Internal)?;
    hot.validate_against(binding).map_err(|_| conflict())?;
    let owner = hot.owner.as_ref().ok_or_else(conflict)?;
    let grant_id = request.grant_id().map_err(|_| conflict())?;
    if owner.guardian_id != request.evidence.guardian_id
        || owner.launch_id != request.evidence.launch_id
        || owner.reservation_id != request.reservation_id
        || hot
            .grant
            .as_ref()
            .is_none_or(|g| g.grant_id != grant_id || &g.binding != binding)
    {
        return Err(conflict());
    }
    super::initial_authority::require_legacy_origin_locked(&mut tx, binding).await?;
    let original=sqlx::query("SELECT request_bytes,request_digest,receipt_bytes FROM profiled_started_receipts WHERE attempt_id=$1 FOR SHARE")
        .bind(binding.attempt_id).fetch_optional(&mut *tx).await.map_err(db)?.ok_or_else(conflict)?;
    let original_bytes: Vec<u8> = original.get("request_bytes");
    if Digest::of(&original_bytes).as_str() != original.get::<String, _>("request_digest") {
        return Err(AppError::Internal);
    }
    let started =
        aq_admission::started::decode_request(&original_bytes).map_err(|_| AppError::Internal)?;
    aq_admission::started::decode_receipt(&original.get::<Vec<u8>, _>("receipt_bytes"), &started)
        .map_err(|_| AppError::Internal)?;
    request.validate_after(&started).map_err(|_| conflict())?;
    let digest = request.digest().map_err(|_| AppError::Internal)?;
    let operation = request.evidence.stop_operation_id;
    let existing=sqlx::query("SELECT attempt_id,operation_id,request_digest,receipt_bytes FROM profiled_stopped_receipts WHERE attempt_id=$1 OR(node_id=$2 AND operation_id=$3) FOR SHARE")
        .bind(binding.attempt_id).bind(node_id).bind(operation).fetch_all(&mut *tx).await.map_err(db)?;
    if !existing.is_empty() {
        if existing.len() != 1 {
            return Err(conflict());
        }
        let old = &existing[0];
        if old.get::<uuid::Uuid, _>("attempt_id") != binding.attempt_id
            || old.get::<uuid::Uuid, _>("operation_id") != operation
            || old.get::<String, _>("request_digest") != digest.as_str()
        {
            return Err(conflict());
        }
        let receipt =
            aq_admission::stopped::decode_receipt(&old.get::<Vec<u8>, _>("receipt_bytes"), request)
                .map_err(|_| AppError::Internal)?;
        // An exact historical acknowledgement grants no new execution or release.
        tx.commit().await.map_err(db)?;
        return Ok(receipt);
    }
    if !view.holds_task
        || !matches!(
            hot.status,
            Status::Granted | Status::Cancelling | Status::Unknown
        )
        || !matches!(hot.stop, Stop::Running | Stop::Unknown)
    {
        return Err(conflict());
    }
    let head = sqlx::query(
        "SELECT phase,revision FROM profiled_owner_journals WHERE attempt_id=$1 FOR UPDATE",
    )
    .bind(binding.attempt_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?
    .ok_or_else(conflict)?;
    if head.get::<String, _>("phase") != "started"
        || request.journal().map_err(|_| conflict())?.0 as i64 <= head.get::<i64, _>("revision")
    {
        return Err(conflict());
    }
    // Sample only after the final lock. Expired/cancelled execution still needs
    // to settle; neither grant expiry nor a retired launch allowlist blocks stop.
    let now = u64::try_from((state.clock)().timestamp_millis()).map_err(|_| conflict())?;
    let authority_revision = row
        .get::<i64, _>("authority_revision")
        .checked_add(1)
        .ok_or(AppError::Internal)?;
    let receipt = Receipt {
        schema_version: 1,
        operation_id: operation,
        binding: binding.clone(),
        grant_id,
        request_digest: digest.clone(),
        recorded_at_ms: now,
        authority_revision_at_record: authority_revision as u64,
    };
    receipt
        .validate_for(request)
        .map_err(|_| AppError::Internal)?;
    let proof = canonical(&request.proof)?;
    sqlx::query("INSERT INTO profiled_stopped_receipts(attempt_id,node_id,operation_id,request_bytes,request_digest,proof_bytes,proof_digest,receipt_bytes,before_record_revision,before_grant_revision) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
        .bind(binding.attempt_id).bind(node_id).bind(operation).bind(canonical(request)?).bind(digest.as_str())
        .bind(&proof).bind(Digest::of(&proof).as_str()).bind(canonical(&receipt)?).bind(row.get::<i64,_>("record_revision"))
        .bind(row.get::<i64,_>("grant_revision")).execute(&mut *tx).await.map_err(db)?;
    journal::record_stopped(&mut tx, request).await?;
    hot.status = Status::Finished;
    hot.stop = Stop::Stopped {
        receipt: receipt.artifact().map_err(|_| AppError::Internal)?,
    };
    hot.validate_against(binding)
        .map_err(|_| AppError::Internal)?;
    let updated=sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1 RETURNING record_revision,authority_revision,grant_revision")
        .bind(binding.attempt_id).bind(serde_json::to_value(hot).map_err(|_|AppError::Internal)?).fetch_one(&mut *tx).await.map_err(db)?;
    if updated.get::<i64, _>("authority_revision") != authority_revision
        || updated.get::<i64, _>("record_revision") != row.get::<i64, _>("record_revision") + 1
        || updated.get::<i64, _>("grant_revision") != row.get::<i64, _>("grant_revision")
    {
        return Err(AppError::Internal);
    }
    tx.commit().await.map_err(db)?;
    Ok(receipt)
}
pub fn routes(state: AppState) -> axum::Router<AppState> {
    axum::Router::new()
        .route("/v1/worker/profiled/stopped", axum::routing::post(report))
        .layer(axum::extract::DefaultBodyLimit::max(
            aq_admission::stopped::MAX_BYTES,
        ))
        .with_state(state.control_view())
}
async fn report(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: axum::http::HeaderMap,
    crate::admin_api::ApiJson(request): crate::admin_api::ApiJson<Request>,
) -> Result<axum::Json<Receipt>, AppError> {
    let node = crate::worker_api::authenticate_context(&state, &headers).await?;
    Ok(axum::Json(record(&state, &node, &request).await?))
}
