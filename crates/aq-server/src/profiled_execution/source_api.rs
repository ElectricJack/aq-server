//! Current-credential native source reads. No request creates an export, grant,
//! source write or receipt. All Git/file I/O occurs outside authority locks.
use super::{
    registration,
    source_exports::{self, LinkedExport},
};
use crate::source_retention::profiled_base::ChunkRange;
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
    let read = SourceRead::qualified(state, auth, binding, tx).await?;
    let view = authority::load_locked(state, tx, &binding.session, Policy::IdentityOnly).await?;
    if !current_assignment(&view, binding) {
        return Err(refused());
    }
    let hot = load_hot(tx, binding).await?;
    let (launch, revision) = load_launch(tx, binding).await?;
    read.check_project(tx, &view, revision).await?;
    let (issued, deadline) = read.window(tx, &hot, &launch).await?;
    let linked = source_exports::load_linked_locked(tx, binding.attempt_id).await?;
    verify_link(&linked, &view, revision)?;
    let now = u64::try_from((state.clock)().timestamp_millis()).map_err(|_| refused())?;
    if now == 0 || !(issued..deadline).contains(&now) {
        return Err(refused());
    }
    Ok(linked)
}
struct SourceRead<'a> {
    state: &'a AppState,
    auth: &'a AuthenticatedNode,
    binding: &'a LaunchBinding,
    config: &'a super::config::ProfiledConfig,
}
impl<'a> SourceRead<'a> {
    async fn qualified(
        state: &'a AppState,
        auth: &'a AuthenticatedNode,
        binding: &'a LaunchBinding,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<Self, AppError> {
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
        Ok(Self {
            state,
            auth,
            binding,
            config,
        })
    }
    async fn check_project(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        view: &authority::AuthorityView,
        revision: u64,
    ) -> Result<(), AppError> {
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
        let eligible = project_binding.enabled
            && project_binding.eligible_node(&self.state.config.projects, self.auth.id());
        let repository_matches = view.source.as_ref().is_some_and(|source| {
            (source.repository_id, source.binding_generation)
                == (
                    project_binding.repository_id,
                    project_binding.repository_binding_generation,
                )
        });
        if !eligible || !repository_matches {
            return Err(refused());
        }
        let enabled_revision = project.get::<i64, _>("revision") as u64 == revision
            && self
                .config
                .projects
                .iter()
                .find(|project| project.project_id == view.project_id)
                .is_some_and(|project| project.enabled);
        if enabled_revision {
            Ok(())
        } else {
            Err(refused())
        }
    }
    async fn window(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        hot: &HotState,
        launch: &sqlx::postgres::PgRow,
    ) -> Result<(u64, u64), AppError> {
        Ok(match hot.status {
            Status::Offered => (0, launch.get::<i64, _>("offer_expires_at_ms") as u64),
            Status::Preparing => super::accepted_preparation::load_preparing_locked(
                self.state,
                tx,
                self.auth,
                self.binding,
                hot,
            )
            .await?
            .window(),
            Status::Granted => (
                0,
                hot.grant
                    .as_ref()
                    .ok_or_else(|| refused())?
                    .expires_at_ms
                    .min(hot.execution_deadline_ms.ok_or_else(|| refused())?),
            ),
            _ => return Err(refused()),
        })
    }
}
fn current_assignment(view: &authority::AuthorityView, binding: &LaunchBinding) -> bool {
    let active = view.kind == Kind::ProfiledV1
        && view.holds_task
        && !view.revoked
        && !view.stopped
        && view.cancellation_id.is_none();
    let digests = view.execution_spec_digest == binding.execution_spec_digest.as_str()
        && view.launch_manifest_digest.as_ref() == Some(&binding.launch_manifest_digest);
    active && digests && view.offer_id == binding.offer_id
}
async fn load_hot(
    tx: &mut Transaction<'_, Postgres>,
    binding: &LaunchBinding,
) -> Result<HotState, AppError> {
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
    Ok(hot)
}
async fn load_launch(
    tx: &mut Transaction<'_, Postgres>,
    binding: &LaunchBinding,
) -> Result<(sqlx::postgres::PgRow, u64), AppError> {
    let launch=sqlx::query("SELECT launch_binding,offer_expires_at_ms,execution_spec FROM profiled_launches WHERE attempt_id=$1").bind(binding.attempt_id).fetch_one(&mut **tx).await.map_err(db)?;
    if launch.get::<serde_json::Value, _>("launch_binding")
        != serde_json::to_value(binding).map_err(|_| refused())?
    {
        return Err(refused());
    }
    let spec: aq_admission::execution_spec::ProfiledExecutionSpec =
        serde_json::from_value(launch.get("execution_spec")).map_err(|_| refused())?;
    spec.validate().map_err(|_| refused())?;
    Ok((launch, spec.project_revision))
}
fn verify_link(
    linked: &LinkedExport,
    view: &authority::AuthorityView,
    revision: u64,
) -> Result<(), AppError> {
    let request = linked.token().request();
    let project = (request.project_id, request.project_revision);
    if project != (view.project_id, revision) {
        return Err(refused());
    }
    let manifest = linked.manifest();
    let source = view.source.as_ref().ok_or_else(refused)?;
    let assigned = &manifest.source == source
        && manifest.attempt == view.attempt
        && manifest.resolved_spec_digest == view.execution_spec_digest;
    if assigned { Ok(()) } else { Err(refused()) }
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
        .insert("aq-source-max-range", HeaderValue::from_static("1048576"));
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
    range: Result<Query<ChunkRange>, QueryRejection>,
) -> Result<Response, Error> {
    version(&headers)?;
    let Path((attempt, member)) =
        path.map_err(|_| AppError::BadRequest("invalid source path".into()))?;
    let Query(range) = range.map_err(|_| AppError::BadRequest("invalid source range".into()))?;
    if !range.valid_limit() {
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
    let bytes =
        crate::source_retention::profiled_base::read_chunk(&state, &before, member, range).await?;
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
