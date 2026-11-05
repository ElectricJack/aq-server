//! `POST /v1/worker/profiled/material/refresh`: a lease-scoped,
//! compare-and-swap write-back of a refreshed subscription login. The node
//! uploads "version N+1 based on N"; the server accepts only while its current
//! version is N, and otherwise keeps the current one and answers `409` with
//! `{accepted:false, version}`. Authenticated like the material fetch (runner
//! permit, admitted attempt). The body is the octet-stream login file; the
//! `aq-material-refresh` header carries base64url JSON `MaterialRefresh`.
//! Other refusals are one uniform Conflict; no response echoes bytes.
use super::{
    Origin, append_locked, head_digest, head_version, identity, lineage_locked, renewable,
};
use crate::{
    AppState,
    error::AppError,
    execution::{
        authority::{self, Kind, Policy, hot::HotState},
        profiled::Status,
    },
    worker_api::AuthenticatedNode,
};
use aq_admission::{Digest, LaunchBinding, MaterialLease, transport::WorkerIdentity};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;
use uuid::Uuid;
use zeroize::Zeroizing;

pub const PATH: &str = "/v1/worker/profiled/material/refresh";
pub const HEADER: &str = "aq-material-refresh";
pub const MAX_HEADER_BYTES: usize = 8192;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct MaterialRefresh {
    pub schema_version: u32,
    pub operation_id: Uuid,
    pub identity: WorkerIdentity,
    pub binding: LaunchBinding,
    pub lease_id: Uuid,
    /// The version these bytes were refreshed from (N in "N+1 based on N").
    pub base_version: i64,
    pub content_digest: Digest,
}
/// Write-back outcome. `accepted: false` means a newer version won; `version`
/// is then the server's current version (fetched by the next attempt).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RefreshOutcome {
    pub accepted: bool,
    pub version: i64,
    pub replayed: bool,
}

fn refused() -> AppError {
    AppError::Conflict("material refresh is not currently authorized".into())
}
fn db(_: sqlx::Error) -> AppError {
    AppError::Internal
}

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
    if bytes.is_empty()
        || bytes.len() > super::store::MAX_BYTES
        || Digest::of(&bytes) != request.content_digest
    {
        return Err(refused());
    }
    let registry = state.config.intelligence.clone().ok_or_else(refused)?;
    let dir = registry.config.material_store.clone().ok_or_else(refused)?;
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    crate::worker_api::lock_current_node(&mut tx, auth).await?;
    let view = authority::load_locked(
        state,
        &mut tx,
        &request.binding.session,
        Policy::IdentityOnly,
    )
    .await?;
    if view.kind != Kind::ProfiledV1
        || !view.holds_task
        || view.offer_id != request.binding.offer_id
        || view.launch_manifest_digest.as_ref() != Some(&request.binding.launch_manifest_digest)
        || view.execution_spec_digest != request.binding.execution_spec_digest.as_str()
    {
        return Err(refused());
    }
    let hot: Value = sqlx::query_scalar(
        "SELECT state FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE",
    )
    .bind(request.binding.attempt_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?
    .ok_or_else(refused)?;
    let hot: HotState = serde_json::from_value(hot).map_err(|_| refused())?;
    hot.validate_against(&request.binding)
        .map_err(|_| refused())?;
    if !matches!(hot.status, Status::Preparing | Status::Granted)
        || hot.revoked
        || hot.cancellation_id.is_some()
    {
        return Err(refused());
    }
    crate::intelligence::fence_current_registry(state, &mut tx).await?;
    // Revocation (attempt closure, preparation revoke) moves the lease out of
    // `admitted`; lease expiry bounds delivery, not the running login.
    let lease: Value = sqlx::query_scalar(
        "SELECT m.lease FROM admission_material_leases m JOIN profiled_launch_materials l ON l.lease_id=m.lease_id AND l.attempt_id=$2 WHERE m.lease_id=$1 AND m.node_id=$3 AND m.boot_id=$4 AND m.admitted_attempt_id=$2 AND m.state='admitted' FOR UPDATE OF m",
    )
    .bind(request.lease_id)
    .bind(request.binding.attempt_id)
    .bind(auth.id())
    .bind(request.identity.boot_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(db)?
    .ok_or_else(refused)?;
    let lease: MaterialLease = serde_json::from_value(lease).map_err(|_| refused())?;
    if lease.lease_id != request.lease_id || lease.binding != request.binding {
        return Err(refused());
    }
    let index = super::configured_index(&registry, &lease).ok_or_else(refused)?;
    let (credential, configured) = (
        &registry.credentials[index],
        &registry.config.credentials[index],
    );
    let base = registry
        .fingerprints
        .get(index)
        .cloned()
        .flatten()
        .ok_or_else(refused)?;
    if configured.kind != aq_selection::CredentialKind::SubscriptionLogin
        || !credential.enabled
        || !configured.allowed_node_ids.contains(&auth.id())
    {
        return Err(refused());
    }
    let lineage = lineage_locked(
        &mut tx,
        &identity(&credential.profile),
        credential.revision,
        &base,
    )
    .await?;
    if let Some(prior) = lineage.entries.iter().find(|e| {
        e.lease_id == Some(lease.lease_id) && e.content_digest == request.content_digest.as_str()
    }) {
        return Ok(RefreshOutcome {
            accepted: true,
            version: prior.sequence,
            replayed: true,
        });
    }
    // An operator replace (or revocation, above) ends this lease's login.
    if !renewable(&lineage, lease.material_digest.as_str()) {
        return Err(refused());
    }
    let current = head_version(&lineage);
    if request.base_version != current {
        // Someone newer won: keep the current version.
        return Ok(RefreshOutcome {
            accepted: false,
            version: current,
            replayed: false,
        });
    }
    if head_digest(&lineage) == request.content_digest.as_str() {
        // Unchanged bytes: nothing to append.
        return Ok(RefreshOutcome {
            accepted: true,
            version: current,
            replayed: true,
        });
    }
    let now = (state.clock)().timestamp_millis();
    let (version, _) = append_locked(
        &mut tx,
        &dir,
        &lineage,
        Origin::Refresh,
        Some(lease.lease_id),
        bytes,
        now,
    )
    .await?;
    tx.commit().await.map_err(db)?;
    Ok(RefreshOutcome {
        accepted: true,
        version,
        replayed: false,
    })
}
