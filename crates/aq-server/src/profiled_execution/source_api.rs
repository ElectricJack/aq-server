//! Current-credential native source reads. No request creates an export, grant,
//! source write or receipt. All Git/file I/O occurs outside authority locks.
use super::{
    registration,
    source_exports::{self, LinkedExport},
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
use aq_admission::{
    LaunchBinding,
    control::{ControlReply, Failure, FailureCode},
    transport::Feature,
};
use aq_session_protocol::Validate;
use axum::{
    Json, Router,
    extract::{
        Path, Query, State,
        rejection::{PathRejection, QueryRejection},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;
pub const VERSION_HEADER: &str = "aq-source-api-version";
pub const TRANSFER_HEADER: &str = "aq-source-transfer";
pub const OFFSET_HEADER: &str = "aq-source-offset";
#[derive(Debug)]
struct Error(AppError);
impl From<AppError> for Error {
    fn from(e: AppError) -> Self {
        Self(e)
    }
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let status = self.0.status();
        let code = match status {
            StatusCode::UNAUTHORIZED => FailureCode::AuthenticationRequired,
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => FailureCode::UnknownOrNotOwned,
            StatusCode::BAD_REQUEST => FailureCode::InvalidRequest,
            StatusCode::PAYLOAD_TOO_LARGE => FailureCode::Capacity,
            StatusCode::CONFLICT => FailureCode::Conflict,
            _ => FailureCode::Unavailable,
        };
        let mut response = (
            status,
            Json(ControlReply::Failure(Failure {
                schema_version: 1,
                code,
                operation_id: None,
            })),
        )
            .into_response();
        response
            .headers_mut()
            .insert(VERSION_HEADER, HeaderValue::from_static("1"));
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
    }
}
#[track_caller]
fn refused() -> AppError {
    tracing::debug!(target: "aq_refusal", at = %std::panic::Location::caller(), "profiled refusal");
    AppError::Conflict("native source is not currently available to this assignment".into())
}
use crate::error::db;
fn version(headers: &HeaderMap) -> Result<(), AppError> {
    if headers.get_all(VERSION_HEADER).iter().count() != 1
        || headers.get(VERSION_HEADER).is_none_or(|v| v != "1")
    {
        return Err(AppError::BadRequest(
            "native source version1 required".into(),
        ));
    }
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Range {
    offset: u64,
    limit: u32,
}
pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/v1/worker/profiled/source/{attempt}", get(status))
        .route("/v1/worker/profiled/source/{attempt}/{member}", get(chunk))
        .with_state(state)
}
async fn candidate(
    state: &AppState,
    auth: &AuthenticatedNode,
    attempt: Uuid,
) -> Result<LaunchBinding, AppError> {
    if attempt.is_nil() {
        return Err(AppError::BadRequest(
            "non-nil source attempt required".into(),
        ));
    }
    let value:serde_json::Value=sqlx::query_scalar("SELECT l.launch_binding FROM profiled_launches l JOIN execution_attempts a USING(attempt_id) WHERE l.attempt_id=$1 AND a.node_id=$2 AND a.kind='profiled_v1'").bind(attempt).bind(auth.id()).fetch_optional(&state.store.pool).await.map_err(db)?.ok_or_else(||AppError::NotFound("source assignment not found".into()))?;
    let binding: LaunchBinding = serde_json::from_value(value).map_err(|_| refused())?;
    binding.validate().map_err(|_| refused())?;
    Ok(binding)
}
async fn locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    binding: &LaunchBinding,
) -> Result<LinkedExport, AppError> {
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    sqlx::query("SET LOCAL statement_timeout='3s'")
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    crate::worker_api::lock_current_node(tx, auth).await?;
    let aq_admission::SessionIdentity::Remote {
        node_id, boot_id, ..
    } = binding.session
    else {
        return Err(refused());
    };
    if node_id != auth.id() {
        return Err(AppError::Forbidden("source node mismatch".into()));
    }
    let runtime = registration::current_runtime_locked(state, tx, node_id, boot_id).await?;
    let config = state
        .config
        .profiled_execution
        .as_deref()
        .ok_or_else(|| refused())?;
    if config
        .qualified_runtimes
        .iter()
        .find(|r| r.runtime == runtime.runtime && r.qualification == runtime.qualification)
        .is_none_or(|r| !r.features.contains(&Feature::ArtifactTransferV1))
    {
        return Err(refused());
    }
    let view = authority::load_locked(state, tx, &binding.session, Policy::IdentityOnly).await?;
    if view.kind != Kind::ProfiledV1
        || !view.holds_task
        || view.revoked
        || view.stopped
        || view.cancellation_id.is_some()
        || view.offer_id != binding.offer_id
        || view.execution_spec_digest != binding.execution_spec_digest.as_str()
        || view.launch_manifest_digest.as_ref() != Some(&binding.launch_manifest_digest)
    {
        return Err(refused());
    }
    let hot: serde_json::Value = sqlx::query_scalar(
        "SELECT state FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE",
    )
    .bind(binding.attempt_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?
    .ok_or_else(|| refused())?;
    let hot: HotState = serde_json::from_value(hot).map_err(|_| refused())?;
    hot.validate_against(binding).map_err(|_| refused())?;
    let launch=sqlx::query("SELECT launch_binding,offer_expires_at_ms,execution_spec FROM profiled_launches WHERE attempt_id=$1").bind(binding.attempt_id).fetch_one(&mut **tx).await.map_err(db)?;
    if launch.get::<serde_json::Value, _>("launch_binding")
        != serde_json::to_value(binding).map_err(|_| refused())?
    {
        return Err(refused());
    }
    let spec: aq_admission::execution_spec::ProfiledExecutionSpec =
        serde_json::from_value(launch.get("execution_spec")).map_err(|_| refused())?;
    spec.validate().map_err(|_| refused())?;
    let project = sqlx::query(
        "SELECT revision,binding FROM project_execution_bindings WHERE project_id=$1 FOR SHARE",
    )
    .bind(view.project_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?
    .ok_or_else(|| refused())?;
    let project_binding: crate::project_bindings::domain::ProjectBinding =
        serde_json::from_value(project.get("binding")).map_err(|_| refused())?;
    if !project_binding.enabled
        || !project_binding.eligible_node(&state.config.projects, auth.id())
        || view.source.as_ref().is_none_or(|source| {
            source.repository_id != project_binding.repository_id
                || source.binding_generation != project_binding.repository_binding_generation
        })
        || project.get::<i64, _>("revision") as u64 != spec.project_revision
        || config
            .projects
            .iter()
            .find(|p| p.project_id == view.project_id)
            .is_none_or(|p| !p.enabled)
    {
        return Err(refused());
    }
    let (issued, deadline) = match hot.status {
        Status::Offered => (0, launch.get::<i64, _>("offer_expires_at_ms") as u64),
        Status::Preparing => {
            super::accepted_preparation::load_preparing_locked(state, tx, auth, binding, &hot)
                .await?
                .window()
        }
        Status::Granted => (
            0,
            hot.grant
                .as_ref()
                .ok_or_else(|| refused())?
                .expires_at_ms
                .min(hot.execution_deadline_ms.ok_or_else(|| refused())?),
        ),
        _ => return Err(refused()),
    };
    let linked = source_exports::load_linked_locked(tx, binding.attempt_id).await?;
    if linked.token().request().project_id != view.project_id
        || linked.token().request().project_revision != spec.project_revision
        || linked.manifest().source != view.source.ok_or_else(|| refused())?
        || linked.manifest().attempt != view.attempt
        || linked.manifest().resolved_spec_digest != view.execution_spec_digest
    {
        return Err(refused());
    }
    let now = u64::try_from((state.clock)().timestamp_millis()).map_err(|_| refused())?;
    if now == 0 || now < issued || now >= deadline {
        return Err(refused());
    }
    Ok(linked)
}
async fn status(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<Uuid>, PathRejection>,
) -> Result<Response, Error> {
    version(&headers)?;
    let Path(attempt) = path.map_err(|_| AppError::BadRequest("invalid source attempt".into()))?;
    let auth = crate::delegated_permits::runtime::authenticate_runner(
        &state,
        &headers,
        aq_admission_sandbox::sandbox::RunnerOperation::FetchPreparationMaterial,
        Some(attempt),
    )
    .await?;
    let binding = candidate(&state, &auth, attempt).await?;
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    let before = locked(&state, &mut tx, &auth, &binding).await?;
    tx.commit().await.map_err(db)?;
    let observation = crate::source_retention::profiled_base::inspect_retained(
        &state,
        before.token(),
        before.fact(),
    )
    .await?;
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    let after = locked(&state, &mut tx, &auth, &binding).await?;
    check_observation(&state, &before, &after, &observation)?;
    let mut response = Json(after.manifest()).into_response();
    tx.commit().await.map_err(db)?;
    response
        .headers_mut()
        .insert(VERSION_HEADER, HeaderValue::from_static("1"));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    Ok(response)
}
fn check_observation(
    state: &AppState,
    before: &LinkedExport,
    after: &LinkedExport,
    observation: &crate::source_retention::profiled_base::RetentionObservation,
) -> Result<(), AppError> {
    if before.token().request_digest() != after.token().request_digest()
        || before.fact().canonical_bytes()? != after.fact().canonical_bytes()?
    {
        return Err(refused());
    }
    observation.validate_for(after.token())?;
    let now = u64::try_from((state.clock)().timestamp_millis()).map_err(|_| refused())?;
    if now < observation.observed_at_ms() || now >= observation.expires_at_ms() {
        return Err(refused());
    }
    Ok(())
}
async fn chunk(
    State(state): State<AppState>,
    headers: HeaderMap,
    path: Result<Path<(Uuid, String)>, PathRejection>,
    range: Result<Query<Range>, QueryRejection>,
) -> Result<Response, Error> {
    version(&headers)?;
    let Path((attempt, member)) =
        path.map_err(|_| AppError::BadRequest("invalid source path".into()))?;
    let Query(range) = range.map_err(|_| AppError::BadRequest("invalid source range".into()))?;
    if !(1..=262144).contains(&range.limit) {
        return Err(AppError::BadRequest("source range limit".into()).into());
    }
    let member = match member.as_str() {
        "manifest" => crate::source_retention::profiled_base::Member::Manifest,
        "bundle" => crate::source_retention::profiled_base::Member::Bundle,
        _ => return Err(AppError::BadRequest("unknown source member".into()).into()),
    };
    let auth = crate::delegated_permits::runtime::authenticate_runner(
        &state,
        &headers,
        aq_admission_sandbox::sandbox::RunnerOperation::FetchPreparationMaterial,
        Some(attempt),
    )
    .await?;
    let binding = candidate(&state, &auth, attempt).await?;
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    let before = locked(&state, &mut tx, &auth, &binding).await?;
    tx.commit().await.map_err(db)?;
    let observation = crate::source_retention::profiled_base::inspect_retained(
        &state,
        before.token(),
        before.fact(),
    )
    .await?;
    let bytes = crate::source_retention::profiled_base::read_chunk(
        &state,
        before.token(),
        before.fact(),
        member,
        range.offset,
        range.limit,
    )
    .await?;
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    let after = locked(&state, &mut tx, &auth, &binding).await?;
    check_observation(&state, &before, &after, &observation)?;
    let descriptor = serde_json::to_vec(after.manifest()).map_err(|_| AppError::Internal)?;
    let encoded = URL_SAFE_NO_PAD.encode(descriptor);
    if encoded.len() > 4096 {
        return Err(AppError::PayloadTooLarge("source descriptor header bound".into()).into());
    }
    tx.commit().await.map_err(db)?;
    let mut response = (StatusCode::OK, bytes).into_response();
    let h = response.headers_mut();
    h.insert(VERSION_HEADER, HeaderValue::from_static("1"));
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(
        TRANSFER_HEADER,
        HeaderValue::from_str(&encoded).map_err(|_| AppError::Internal)?,
    );
    h.insert(
        OFFSET_HEADER,
        HeaderValue::from_str(&range.offset.to_string()).map_err(|_| AppError::Internal)?,
    );
    Ok(response)
}
