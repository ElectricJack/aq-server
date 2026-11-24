//! `POST /v1/worker/profiled/material/refresh`: a lease-scoped,
//! compare-and-swap write-back of a refreshed subscription login. The node
//! uploads "version N+1 based on N"; the server accepts only while its current
//! version is N, and otherwise keeps the current one and answers `409` with
//! `{accepted:false, version}`. Authenticated like the material fetch (runner
//! permit, admitted attempt). The body is the octet-stream login file; the
//! `aq-material-refresh` header carries base64url JSON `MaterialRefresh`.
//! Other refusals are one uniform Conflict; no response echoes bytes.
use super::{Origin, append_locked};
use crate::{
    AppState,
    error::AppError,
    execution::authority::{self, Kind, Policy, hot::HotState},
    worker_api::AuthenticatedNode,
};
use aq_admission::{LaunchBinding, MaterialLease};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;
use std::sync::Arc;
use zeroize::Zeroizing;

pub const PATH: &str = "/v1/worker/profiled/material/refresh";
pub const HEADER: &str = "aq-material-refresh";
pub const MAX_HEADER_BYTES: usize = 8192;

use aq_login_store::refresh::RefreshDecision;
pub use aq_login_store::refresh::{MaterialRefresh, RefreshOutcome};

#[track_caller]
fn refused() -> AppError {
    tracing::debug!(target: "aq_refusal", at = %std::panic::Location::caller(), "profiled refusal");
    AppError::Conflict("material refresh is not currently authorized".into())
}
use crate::error::db;

pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route(PATH, post(report))
        .layer(DefaultBodyLimit::max(super::store::MAX_BYTES))
        .with_state(state)
}

fn parse(headers: &HeaderMap) -> Result<MaterialRefresh, AppError> {
    let bad = || AppError::BadRequest("invalid material refresh".into());
    let raw = headers.get(HEADER).ok_or_else(bad)?.as_bytes();
    if raw.len() > MAX_HEADER_BYTES {
        return Err(bad());
    }
    let doc: MaterialRefresh =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(raw).map_err(|_| bad())?)
            .map_err(|_| bad())?;
    if doc.schema_version != 1 {
        return Err(bad());
    }
    Ok(doc)
}

async fn report(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, AppError> {
    let request = parse(&headers)?;
    let auth = crate::delegated_permits::runtime::authenticate_runner(
        &state,
        &headers,
        aq_admission_sandbox::sandbox::RunnerOperation::FetchPreparationMaterial,
        None,
    )
    .await?;
    crate::delegated_permits::runtime::require_binding(&auth, &request.binding)?;
    let bytes = Arc::new(Zeroizing::new(body.to_vec()));
    drop(body);
    let outcome = write_back(&state, &auth, &request, bytes).await?;
    let status = if outcome.accepted {
        StatusCode::OK
    } else {
        StatusCode::CONFLICT
    };
    Ok((status, Json(outcome)).into_response())
}

pub async fn write_back(
    state: &AppState,
    auth: &AuthenticatedNode,
    request: &MaterialRefresh,
    bytes: Arc<Zeroizing<Vec<u8>>>,
) -> Result<RefreshOutcome, AppError> {
    if auth.id() != request.identity.node_id {
        return Err(AppError::Forbidden("material node mismatch".into()));
    }
    if !request.matches_content(&bytes) {
        return Err(refused());
    }
    let registry = state.config.intelligence.clone().ok_or_else(|| refused())?;
    let dir = registry
        .config
        .material_store
        .clone()
        .ok_or_else(|| refused())?;
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    crate::worker_api::lock_current_node(&mut tx, auth).await?;
    let lease = admitted_lease(state, &mut tx, request, auth).await?;
    let (index, lineage) =
        aq_login_store::refresh::writable_lineage(&mut tx, &registry, &lease, auth.id())
            .await?
            .ok_or_else(refused)?;
    match request.evaluate(&lineage, &lease) {
        RefreshDecision::Complete(outcome) => return Ok(outcome),
        RefreshDecision::Refused => return Err(refused()),
        RefreshDecision::Append => {}
    }
    // A Codex login must be the stored one refreshed: same account, later.
    if !super::refresh_of_head(
        registry.clone(),
        index,
        lineage.head_source(&dir),
        bytes.clone(),
    )
    .await?
    {
        return Err(refused());
    }
    let now = (state.clock)().timestamp_millis();
    let (version, _) = append_locked(
        &mut tx,
        &dir,
        &lineage,
        aq_login_store::NewRevision {
            origin: Origin::Refresh,
            lease_id: Some(lease.lease_id),
            bytes,
            now_ms: now,
        },
    )
    .await?;
    tx.commit().await.map_err(db)?;
    Ok(RefreshOutcome {
        accepted: true,
        version,
        replayed: false,
    })
}
fn bound_authority(view: &authority::AuthorityView, binding: &LaunchBinding) -> bool {
    view.kind == Kind::ProfiledV1
        && view.holds_task
        && view.offer_id == binding.offer_id
        && view.launch_manifest_digest.as_ref() == Some(&binding.launch_manifest_digest)
        && view.execution_spec_digest == binding.execution_spec_digest.as_str()
}
async fn admitted_lease(
    state: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    request: &MaterialRefresh,
    auth: &AuthenticatedNode,
) -> Result<MaterialLease, AppError> {
    let view =
        authority::load_locked(state, tx, &request.binding.session, Policy::IdentityOnly).await?;
    if !bound_authority(&view, &request.binding) {
        return Err(refused());
    }
    let hot: Value = sqlx::query_scalar(
        "SELECT state FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE",
    )
    .bind(request.binding.attempt_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?
    .ok_or_else(|| refused())?;
    let hot: HotState = serde_json::from_value(hot).map_err(|_| refused())?;
    hot.validate_against(&request.binding)
        .map_err(|_| refused())?;
    // Live, or cancelled after its grant (`Status::login_write_back`); the lease is checked below.
    let cancelled = hot.cancellation_id.is_some();
    if !hot.status.login_write_back(hot.revoked, cancelled) {
        return Err(refused());
    }
    crate::intelligence::fence_current_registry(state, tx).await?;
    // Revocation (attempt closure, preparation revoke) moves the lease out of
    // `admitted`; lease expiry bounds delivery, not the running login.
    let lease: Value = sqlx::query_scalar(
        "SELECT m.lease FROM admission_material_leases m JOIN profiled_launch_materials l ON l.lease_id=m.lease_id AND l.attempt_id=$2 WHERE m.lease_id=$1 AND m.node_id=$3 AND m.boot_id=$4 AND m.admitted_attempt_id=$2 AND m.state='admitted' FOR UPDATE OF m",
    )
    .bind(request.lease_id)
    .bind(request.binding.attempt_id)
    .bind(auth.id())
    .bind(request.identity.boot_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?
    .ok_or_else(|| refused())?;
    let lease: MaterialLease = serde_json::from_value(lease).map_err(|_| refused())?;
    if lease.lease_id != request.lease_id || lease.binding != request.binding {
        return Err(refused());
    }
    Ok(lease)
}
