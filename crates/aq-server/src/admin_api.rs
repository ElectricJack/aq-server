//! JSON admin API: identity, status, workers, enrollment issuance and users.
//!
//! `build_admin_router` puts every route behind `csrf_guard`. GET handlers here never change state.

use std::time::Duration;

use aq_protocol::VersionInfo;
use axum::extract::{FromRequest, Path, Request, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::authz::{CurrentUser, RequireOwner};
use crate::config::AuthConfig;
use crate::error::AppError;
use crate::store::{Node, Role, StoreError};
use crate::tokens::{new_enrollment_code, verifier};
use crate::{AppState, version_info};

/// All admin routes, WITHOUT the CSRF guard. `build_admin_router` layers
/// `csrf_guard` over the fully merged router (a `layer`, not a `route_layer`
/// declared mid-chain), so every admin route and the fallback is covered no
/// matter where it is declared.
pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route("/api/me", get(me))
        .route("/api/status", get(status))
        .route("/api/workers", get(workers))
        .route("/api/enrollments", post(create_enrollment))
        .route("/api/workers/{id}/revoke", post(revoke_worker))
        .route("/api/users", get(list_users).post(create_user))
        .merge(crate::updates::update_routes())
}

/// `Json` extractor whose rejections are `ApiError` bodies with 400.
pub(crate) struct ApiJson<T>(pub(crate) T);

impl<T> FromRequest<AppState> for ApiJson<T>
where
    T: serde::de::DeserializeOwned,
{
    type Rejection = AppError;

    async fn from_request(req: Request, state: &AppState) -> Result<Self, Self::Rejection> {
        match Json::<T>::from_request(req, state).await {
            Ok(Json(v)) => Ok(ApiJson(v)),
            Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                Err(AppError::PayloadTooLarge(e.body_text()))
            }
            Err(e) => Err(AppError::BadRequest(e.body_text())),
        }
    }
}

/// Revoked wins; otherwise online while the last heartbeat is within
/// `offline_after` of `now`. A worker that never reported is offline.
pub fn worker_status(node: &Node, now: DateTime<Utc>, offline_after: Duration) -> &'static str {
    if node.revoked_at.is_some() {
        return "revoked";
    }
    let Some(seen) = node.last_seen_at else {
        return "offline";
    };
    match (now - seen).to_std() {
        Ok(age) if age <= offline_after => "online",
        // Negative age (clock skew): the heartbeat is in the future, so recent.
        Err(_) => "online",
        Ok(_) => "offline",
    }
}

#[derive(Serialize)]
struct MeResponse {
    email: String,
    role: Role,
    auth_mode: &'static str,
}

async fn me(State(state): State<AppState>, CurrentUser(user): CurrentUser) -> Json<MeResponse> {
    let auth_mode = match state.config.auth {
        AuthConfig::Dev { .. } => "dev",
        AuthConfig::CloudflareAccess { .. } => "cloudflare_access",
    };
    Json(MeResponse {
        email: user.email,
        role: user.role,
        auth_mode,
    })
}

#[derive(Serialize)]
struct StatusResponse {
    version: VersionInfo,
    health: &'static str,
    update: serde_json::Value,
}

async fn status(State(state): State<AppState>, _user: CurrentUser) -> Json<StatusResponse> {
    Json(StatusResponse {
        version: version_info(),
        health: "ok",
        update: crate::updates::status_summary(&state),
    })
}

#[derive(Serialize)]
struct WorkerView {
    id: Uuid,
    name: String,
    worker_version: String,
    protocol_version: u32,
    enrolled_at: DateTime<Utc>,
    last_seen_at: Option<DateTime<Utc>>,
    status: &'static str,
    /// X3/SBX25: the latest node storage report, flattened; absent when the
    /// node has never reported storage.
    #[serde(flatten)]
    storage: Option<crate::node_storage::StorageReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    storage_used_bps: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    storage_observed_at: Option<DateTime<Utc>>,
}

async fn workers(
    State(state): State<AppState>,
    _user: CurrentUser,
) -> Result<Json<Vec<WorkerView>>, AppError> {
    let now = (state.clock)();
    let offline_after = Duration::from_secs(state.config.workers.offline_after_secs);
    let nodes = state.store.list_nodes().await?;
    let ids: Vec<Uuid> = nodes.iter().map(|n| n.id).collect();
    let mut conn = state
        .store
        .pool
        .acquire()
        .await
        .map_err(|_| AppError::Internal)?;
    let mut storage = crate::node_storage::latest(&mut conn, &ids).await?;
    drop(conn);
    Ok(Json(
        nodes
            .into_iter()
            .map(|n| {
                let report = storage
                    .iter()
                    .position(|(id, ..)| *id == n.id)
                    .map(|i| storage.swap_remove(i));
                (n, report)
            })
            .map(|(n, report)| WorkerView {
                status: worker_status(&n, now, offline_after),
                storage_used_bps: report.as_ref().map(|r| r.2),
                storage_observed_at: report
                    .as_ref()
                    .and_then(|r| DateTime::from_timestamp_millis(r.3 as i64)),
                storage: report.map(|r| r.1),
                id: n.id,
                name: n.name,
                worker_version: n.worker_version,
                protocol_version: n.protocol_version,
                enrolled_at: n.enrolled_at,
                last_seen_at: n.last_seen_at,
            })
            .collect(),
    ))
}

#[derive(Serialize)]
struct EnrollmentResponse {
    code: String,
    expires_at: DateTime<Utc>,
}

async fn create_enrollment(
    State(state): State<AppState>,
    RequireOwner(owner): RequireOwner,
) -> Result<(StatusCode, Json<EnrollmentResponse>), AppError> {
    let now = (state.clock)();
    let ttl = i64::try_from(state.config.enrollment.code_ttl_secs)
        .ok()
        .and_then(chrono::Duration::try_seconds)
        .ok_or(AppError::Internal)?;
    let expires_at = now.checked_add_signed(ttl).ok_or(AppError::Internal)?;
    let code = new_enrollment_code();
    let id = state
        .store
        .create_enrollment(&verifier(&code), owner.id, now, expires_at)
        .await?;
    tracing::info!(created_by = %owner.id, enrollment_id = %id, "enrollment code created");
    Ok((
        StatusCode::CREATED,
        Json(EnrollmentResponse { code, expires_at }),
    ))
}

async fn revoke_worker(
    State(state): State<AppState>,
    _owner: RequireOwner,
    Path(id): Path<String>,
) -> Result<StatusCode, AppError> {
    let id: Uuid = id
        .parse()
        .map_err(|_| AppError::BadRequest("worker id must be a UUID".to_string()))?;
    let mut conn = state
        .store
        .pool
        .acquire()
        .await
        .map_err(|_| AppError::Internal)?;
    // Review I4: a slot of a service machine is revoked with its whole machine.
    if let Some(key) = aq_machine_control::live_machine_key(&mut conn, id)
        .await
        .ok()
        .flatten()
    {
        return Err(AppError::Conflict(format!(
            "{}: worker.revoke {key}",
            aq_machine_control::MACHINE_NODE
        )));
    }
    drop(conn);
    if state.store.revoke_node(id, (state.clock)()).await? {
        return Ok(StatusCode::NO_CONTENT);
    }
    // Either unknown or already revoked; revoking twice is idempotent.
    let known = state.store.list_nodes().await?.iter().any(|n| n.id == id);
    if known {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound("no such worker".to_string()))
    }
}

#[derive(Serialize)]
struct UserView {
    email: String,
    role: Role,
    bound: bool,
}

async fn list_users(
    State(state): State<AppState>,
    _owner: RequireOwner,
) -> Result<Json<Vec<UserView>>, AppError> {
    Ok(Json(
        state
            .store
            .list_users()
            .await?
            .into_iter()
            .map(|u| UserView {
                bound: u.subject.is_some(),
                email: u.email,
                role: u.role,
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewUserRequest {
    email: String,
    role: String,
}

async fn create_user(
    State(state): State<AppState>,
    _owner: RequireOwner,
    ApiJson(req): ApiJson<NewUserRequest>,
) -> Result<StatusCode, AppError> {
    let email = req.email.trim();
    if email.is_empty()
        || email.len() > 254
        || !email.contains('@')
        || email.contains(char::is_whitespace)
    {
        return Err(AppError::BadRequest("invalid email address".to_string()));
    }
    let role: Role = req.role.parse().map_err(|_: StoreError| {
        AppError::BadRequest("role must be owner or viewer".to_string())
    })?;
    state
        .store
        .invite_user(email, role, (state.clock)())
        .await?;
    Ok(StatusCode::CREATED)
}
