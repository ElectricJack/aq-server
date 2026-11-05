//! Authenticated qualified-owner report of an actual durable host start.
//! It changes only the last-known host phase, never grant identity or deadlines.
use super::{journal, registration};
use crate::{
    AppState,
    error::AppError,
    execution::{
        authority::{self, Kind, Policy, hot::HotState},
        profiled::Stop,
    },
    worker_api::AuthenticatedNode,
};
use aq_admission::{
    SessionIdentity,
    started::{Receipt, Request},
};
use sqlx::Row;
fn conflict() -> AppError {
    AppError::Conflict("started evidence conflicts with current launch owner".into())
}
fn db(_: sqlx::Error) -> AppError {
    AppError::Internal
}
fn canonical(value: &impl serde::Serialize) -> Result<Vec<u8>, AppError> {
    let mut value = serde_json::to_value(value).map_err(|_| AppError::Internal)?;
    value.sort_all_objects();
    serde_json::to_vec(&value).map_err(|_| AppError::Internal)
}
/// Production attempt path (H2): best-effort enrollment of the attempt's trusted
/// catalog observation source once the attempt has started. A failure here
/// never fails `started`; the batch route retries enrollment before ingest.
async fn enroll_observation_source(state: &AppState, attempt: uuid::Uuid, node: uuid::Uuid) {
    if let Err(error) =
        crate::observations::catalog::register_attempt(state, attempt, Some(node)).await
    {
        tracing::debug!(%attempt, ?error, "observation source enrollment deferred");
    }
}
pub async fn record(
    state: &AppState,
    node: &AuthenticatedNode,
    request: &Request,
) -> Result<Receipt, AppError> {
    request
        .validate()
        .map_err(|_| AppError::BadRequest("invalid started report".into()))?;
    let SessionIdentity::Remote {
        node_id, boot_id, ..
    } = request.binding.session
    else {
        return Err(conflict());
    };
    if node.id() != node_id {
        return Err(AppError::Forbidden("started node mismatch".into()));
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
    let runtime = registration::current_runtime_locked(state, &mut tx, node_id, boot_id).await?;
    if runtime.runtime != request.runtime || runtime.qualification != request.qualification {
        return Err(conflict());
    }
    let view = authority::load_locked(
        state,
        &mut tx,
        &request.binding.session,
        Policy::IdentityOnly,
    )
    .await?;
    if view.kind != Kind::ProfiledV1
        || view.offer_id != request.binding.offer_id
        || view.launch_manifest_digest.as_ref() != Some(&request.binding.launch_manifest_digest)
        || view.execution_spec_digest != request.binding.execution_spec_digest.as_str()
    {
        return Err(conflict());
    }
    let row=sqlx::query("SELECT state,record_revision,authority_revision,grant_revision FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE").bind(request.binding.attempt_id).fetch_optional(&mut *tx).await.map_err(db)?.ok_or_else(conflict)?;
    let mut hot: HotState =
        serde_json::from_value(row.get("state")).map_err(|_| AppError::Internal)?;
    hot.validate_against(&request.binding)
        .map_err(|_| conflict())?;
    let owner = hot.owner.as_ref().ok_or_else(conflict)?;
    if owner.guardian_id != request.guardian_id
        || owner.launch_id != request.launch_id
        || owner.reservation_id != request.reservation_id
        || hot
            .grant
            .as_ref()
            .is_none_or(|g| g.grant_id != request.grant_id || g.binding != request.binding)
    {
        return Err(conflict());
    }
    // Phase execution needs its own qualified Started adapter. A hot Grant
    // alone cannot turn an accepted no-launch origin into legacy authority.
    super::initial_authority::require_legacy_origin_locked(&mut tx, &request.binding).await?;
    super::closure::ensure_launch_open_locked(&mut tx, &request.binding).await?;
    let digest = request.digest().map_err(|_| AppError::Internal)?;
    let existing=sqlx::query("SELECT attempt_id,operation_id,request_digest,receipt_bytes FROM profiled_started_receipts WHERE attempt_id=$1 OR(node_id=$2 AND operation_id=$3) FOR SHARE").bind(request.binding.attempt_id).bind(node_id).bind(request.operation_id).fetch_all(&mut *tx).await.map_err(db)?;
    if !existing.is_empty() {
        if existing.len() != 1 {
            return Err(conflict());
        }
        let old = &existing[0];
        if old.get::<uuid::Uuid, _>("attempt_id") != request.binding.attempt_id
            || old.get::<uuid::Uuid, _>("operation_id") != request.operation_id
            || old.get::<String, _>("request_digest") != digest.as_str()
        {
            return Err(conflict());
        }
        let receipt =
            aq_admission::started::decode_receipt(&old.get::<Vec<u8>, _>("receipt_bytes"), request)
                .map_err(|_| AppError::Internal)?;
        // Exact historical ack: cancellation/expiry/stop are not undone.
        tx.commit().await.map_err(db)?;
        enroll_observation_source(state, request.binding.attempt_id, node_id).await;
        return Ok(receipt);
    }
    if !matches!(hot.stop, Stop::Unknown) {
        return Err(conflict());
    }
    let head = sqlx::query(
        "SELECT phase,revision,digest FROM profiled_owner_journals WHERE attempt_id=$1 FOR UPDATE",
    )
    .bind(request.binding.attempt_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?
    .ok_or_else(conflict)?;
    if head.get::<String, _>("phase") != "prepared"
        || request.journal_revision as i64 <= head.get::<i64, _>("revision")
    {
        return Err(conflict());
    }
    // Sample wall time only after the final owner lock. Boot-clock evidence is
    // never compared to wall time or used to extend a central execution lease.
    let now = u64::try_from((state.clock)().timestamp_millis()).map_err(|_| conflict())?;
    view.active_at(now)?;
    let authority_revision = row
        .get::<i64, _>("authority_revision")
        .checked_add(1)
        .ok_or(AppError::Internal)?;
    let receipt = Receipt {
        schema_version: 1,
        operation_id: request.operation_id,
        binding: request.binding.clone(),
        grant_id: request.grant_id,
        request_digest: digest.clone(),
        recorded_at_ms: now,
        authority_revision_at_record: authority_revision as u64,
    };
    receipt
        .validate_for(request)
        .map_err(|_| AppError::Internal)?;
    sqlx::query("INSERT INTO profiled_started_receipts(attempt_id,node_id,operation_id,request_bytes,request_digest,receipt_bytes,before_record_revision) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(request.binding.attempt_id).bind(node_id).bind(request.operation_id).bind(canonical(request)?).bind(digest.as_str()).bind(canonical(&receipt)?).bind(row.get::<i64,_>("record_revision")).execute(&mut *tx).await.map_err(db)?;
    journal::record_started(&mut tx, request).await?;
    hot.stop = Stop::Running;
    hot.validate_against(&request.binding)
        .map_err(|_| AppError::Internal)?;
    let updated=sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1 RETURNING record_revision,authority_revision,grant_revision").bind(request.binding.attempt_id).bind(serde_json::to_value(hot).map_err(|_|AppError::Internal)?).fetch_one(&mut *tx).await.map_err(db)?;
    if updated.get::<i64, _>("authority_revision") != authority_revision
        || updated.get::<i64, _>("record_revision") != row.get::<i64, _>("record_revision") + 1
        || updated.get::<i64, _>("grant_revision") != row.get::<i64, _>("grant_revision")
    {
        return Err(AppError::Internal);
    }
    tx.commit().await.map_err(db)?;
    enroll_observation_source(state, request.binding.attempt_id, node_id).await;
    Ok(receipt)
}
pub fn routes(state: AppState) -> axum::Router<AppState> {
    axum::Router::new()
        .route("/v1/worker/profiled/started", axum::routing::post(report))
        .layer(axum::extract::DefaultBodyLimit::max(
            aq_admission::started::MAX_BYTES,
        ))
        .with_state(state.control_view())
}
async fn report(
    axum::extract::State(state): axum::extract::State<AppState>,
    headers: axum::http::HeaderMap,
    crate::admin_api::ApiJson(request): crate::admin_api::ApiJson<Request>,
) -> Result<axum::Json<Receipt>, AppError> {
    let node = crate::delegated_permits::runtime::authenticate_runner(
        &state,
        &headers,
        aq_admission_sandbox::sandbox::RunnerOperation::Started,
        None,
    )
    .await?;
    crate::delegated_permits::runtime::require_binding(&node, &request.binding)?;
    Ok(axum::Json(record(&state, &node, &request).await?))
}
