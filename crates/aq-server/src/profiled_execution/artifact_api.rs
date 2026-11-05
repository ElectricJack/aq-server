//! Authenticated bounded ranges of already linked, retained inline artifacts.
//! No request can nominate a file path, register source data or grant execution.
use super::{artifacts, registration};
use crate::{
    AppState,
    error::AppError,
    execution::{
        authority::{self, Kind, Policy, hot::HotState},
        profiled::Status,
    },
};
use aq_admission::transport::{ArtifactFetch, Feature, TransportDocument};
use axum::{
    Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
    routing::post,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;
use sqlx::Row;
use uuid::Uuid;
pub const RETENTION_HEADER: &str = "aq-artifact-retention";
pub const OFFSET_HEADER: &str = "aq-artifact-offset";
pub const MAX_REQUEST_BYTES: usize = 16 * 1024;
fn refused() -> AppError {
    AppError::Conflict("artifact is not currently available to this assignment".into())
}
fn db(_: sqlx::Error) -> AppError {
    AppError::Internal
}
pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/v1/worker/profiled/artifact", post(fetch))
        .layer(DefaultBodyLimit::max(MAX_REQUEST_BYTES))
        .with_state(state)
}
async fn fetch(
    State(state): State<AppState>,
    headers: HeaderMap,
    crate::admin_api::ApiJson(request): crate::admin_api::ApiJson<ArtifactFetch>,
) -> Result<Response, AppError> {
    TransportDocument::ArtifactFetch(request.clone())
        .validate()
        .map_err(|_| AppError::BadRequest("invalid artifact fetch".into()))?;
    let auth = crate::delegated_permits::runtime::authenticate_runner(
        &state,
        &headers,
        aq_admission_sandbox::sandbox::RunnerOperation::UploadArtifact,
        None,
    )
    .await?;
    crate::delegated_permits::runtime::require_binding(&auth, &request.binding)?;
    if auth.id() != request.identity.node_id {
        return Err(AppError::Forbidden("artifact node mismatch".into()));
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
    crate::worker_api::lock_current_node(&mut tx, &auth).await?;
    let runtime =
        registration::current_runtime_locked(&state, &mut tx, auth.id(), request.identity.boot_id)
            .await?;
    if state
        .config
        .profiled_execution
        .as_ref()
        .and_then(|c| {
            c.qualified_runtimes
                .iter()
                .find(|r| r.runtime == runtime.runtime)
        })
        .is_none_or(|r| !r.features.contains(&Feature::ArtifactTransferV1))
    {
        return Err(refused());
    }
    let view = authority::load_locked(
        &state,
        &mut tx,
        &request.binding.session,
        Policy::IdentityOnly,
    )
    .await?;
    if view.kind != Kind::ProfiledV1
        || !view.holds_task
        || view.revoked
        || view.cancellation_id.is_some()
        || view.stopped
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
    let sealed = sqlx::query(
        "SELECT launch_binding,offer_expires_at_ms FROM profiled_launches WHERE attempt_id=$1",
    )
    .bind(request.binding.attempt_id)
    .fetch_one(&mut *tx)
    .await
    .map_err(db)?;
    if sealed.get::<Value, _>("launch_binding")
        != serde_json::to_value(&request.binding).map_err(|_| AppError::Internal)?
    {
        return Err(refused());
    }
    let (issued, expires) = match hot.status {
        Status::Offered => (0, sealed.get::<i64, _>("offer_expires_at_ms") as u64),
        Status::Preparing => super::accepted_preparation::load_preparing_locked(
            &state,
            &mut tx,
            &auth,
            &request.binding,
            &hot,
        )
        .await?
        .window(),
        Status::Granted => (
            0,
            hot.grant
                .as_ref()
                .ok_or_else(refused)?
                .expires_at_ms
                .min(hot.execution_deadline_ms.ok_or_else(refused)?),
        ),
        _ => return Err(refused()),
    };
    let rows=sqlx::query("SELECT a.artifact_id FROM profiled_launch_artifacts l JOIN admission_artifacts a USING(artifact_id) WHERE l.attempt_id=$1 AND a.descriptor->'artifact'=$2 LIMIT 2")
        .bind(request.binding.attempt_id).bind(serde_json::to_value(&request.artifact).map_err(|_|AppError::Internal)?).fetch_all(&mut *tx).await.map_err(db)?;
    if rows.len() != 1 {
        return Err(refused());
    }
    let item = artifacts::load_locked(&mut tx, rows[0].get::<Uuid, _>("artifact_id")).await?;
    if item.project_id != view.project_id || item.descriptor.artifact != request.artifact {
        return Err(refused());
    }
    let now = u64::try_from((state.clock)().timestamp_millis()).map_err(|_| refused())?;
    if now == 0 || now < issued || now >= expires {
        return Err(refused());
    }
    if hot.status == Status::Granted {
        view.active_at(now)?;
    }
    // load_locked already verified the immutable bytes and receipt. Only time
    // can change while these same owned bytes remain in this transaction.
    if now < item.descriptor.retained_at_ms || now >= item.descriptor.retain_until_ms {
        return Err(refused());
    }
    let offset = usize::try_from(request.offset).map_err(|_| refused())?;
    if offset >= item.bytes.len() {
        return Err(AppError::BadRequest(
            "artifact range is outside retained bytes".into(),
        ));
    }
    let end = offset
        .checked_add(request.length as usize)
        .ok_or_else(refused)?
        .min(item.bytes.len());
    let encoded = URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&item.descriptor).map_err(|_| AppError::Internal)?);
    if encoded.len() > 4096 {
        return Err(refused());
    }
    let mut response = item.bytes[offset..end].to_vec().into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        RETENTION_HEADER,
        HeaderValue::from_str(&encoded).map_err(|_| AppError::Internal)?,
    );
    response.headers_mut().insert(
        OFFSET_HEADER,
        HeaderValue::from_str(&request.offset.to_string()).map_err(|_| AppError::Internal)?,
    );
    tx.commit().await.map_err(db)?;
    Ok(response)
}
