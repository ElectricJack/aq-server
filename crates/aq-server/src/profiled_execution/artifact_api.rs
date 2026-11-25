//! Authenticated bounded ranges of already linked, retained inline artifacts.
//! No request can nominate a file path, register source data or grant execution.
use super::{artifacts, registration};
use crate::worker_api::AuthenticatedNode;
use crate::{
    AppState,
    error::AppError,
    execution::{
        authority::{self, Kind, Policy, hot::HotState},
        profiled::Status,
    },
};
use aq_admission::transport::{ArtifactFetch, Feature, TransportDocument};
use aq_profiled_state::inline_artifact::storage;
use axum::{
    Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, HeaderValue, header},
    response::{IntoResponse, Response},
    routing::post,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde_json::Value;
use sqlx::{Postgres, Row, Transaction};
type Tx<'a> = Transaction<'a, Postgres>;
use uuid::Uuid;
pub const RETENTION_HEADER: &str = "aq-artifact-retention";
pub const OFFSET_HEADER: &str = "aq-artifact-offset";
pub const MAX_REQUEST_BYTES: usize = 16 * 1024;
#[track_caller]
fn refused() -> AppError {
    tracing::debug!(target: "aq_refusal", at = %std::panic::Location::caller(), "profiled refusal");
    AppError::Conflict("artifact is not currently available to this assignment".into())
}
use crate::error::db;
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
    Fetch {
        state: &state,
        request: &request,
        auth: &auth,
    }
    .read()
    .await
}
struct Fetch<'a> {
    state: &'a AppState,
    request: &'a ArtifactFetch,
    auth: &'a AuthenticatedNode,
}
impl Fetch<'_> {
    async fn read(&self) -> Result<Response, AppError> {
        let mut tx = self.state.store.pool.begin().await.map_err(db)?;
        self.lock_runtime(&mut tx).await?;
        let view = self.assignment(&mut tx).await?;
        let hot = self.hot(&mut tx).await?;
        let phase = self.phase_window(&mut tx, &hot).await?;
        let item = self.linked(&mut tx, view.project_id).await?;
        let now = u64::try_from((self.state.clock)().timestamp_millis()).map_err(|_| refused())?;
        if now == 0 || !phase.contains(&now) {
            return Err(refused());
        }
        if hot.status == Status::Granted {
            view.active_at(now)?;
        }
        // load_locked verified the immutable bytes and receipt. Only time
        // can change while these same owned bytes remain in this transaction.
        let retained = item.descriptor.retained_at_ms..item.descriptor.retain_until_ms;
        if !retained.contains(&now) {
            return Err(refused());
        }
        let response = range_response(item, self.request)?;
        tx.commit().await.map_err(db)?;
        Ok(response)
    }
    async fn lock_runtime(&self, tx: &mut Tx<'_>) -> Result<(), AppError> {
        storage::set_timeouts(tx).await.map_err(db)?;
        crate::worker_api::lock_current_node(tx, self.auth).await?;
        let runtime = registration::current_runtime_locked(
            self.state,
            tx,
            self.auth.id(),
            self.request.identity.boot_id,
        )
        .await?;
        if self
            .state
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
        Ok(())
    }
    async fn assignment(&self, tx: &mut Tx<'_>) -> Result<authority::AuthorityView, AppError> {
        let request = self.request;
        let view = authority::load_locked(
            self.state,
            tx,
            &request.binding.session,
            Policy::IdentityOnly,
        )
        .await?;
        let assigned = view.kind == Kind::ProfiledV1 && view.holds_task;
        let cancelled = view.revoked || view.cancellation_id.is_some() || view.stopped;
        let eligible = assigned && !cancelled;
        if eligible && self.matches_offer(&view) {
            Ok(view)
        } else {
            Err(refused())
        }
    }
    fn matches_offer(&self, view: &authority::AuthorityView) -> bool {
        let requested = &self.request.binding;
        let offer = (
            view.offer_id,
            view.launch_manifest_digest.as_ref(),
            view.execution_spec_digest.as_str(),
        );
        offer
            == (
                requested.offer_id,
                Some(&requested.launch_manifest_digest),
                requested.execution_spec_digest.as_str(),
            )
    }
    async fn hot(&self, tx: &mut Tx<'_>) -> Result<HotState, AppError> {
        let request = self.request;
        let hot: Value = sqlx::query_scalar(
            "SELECT state FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE",
        )
        .bind(request.binding.attempt_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?
        .ok_or_else(refused)?;
        let hot: HotState = serde_json::from_value(hot).map_err(|_| refused())?;
        hot.validate_against(&request.binding)
            .map_err(|_| refused())?;
        Ok(hot)
    }
    async fn phase_window(
        &self,
        tx: &mut Tx<'_>,
        hot: &HotState,
    ) -> Result<std::ops::Range<u64>, AppError> {
        let request = self.request;
        let sealed = sqlx::query(
            "SELECT launch_binding,offer_expires_at_ms FROM profiled_launches WHERE attempt_id=$1",
        )
        .bind(request.binding.attempt_id)
        .fetch_one(&mut **tx)
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
                self.state,
                tx,
                self.auth,
                &request.binding,
                hot,
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
        Ok(issued..expires)
    }
    async fn linked(
        &self,
        tx: &mut Tx<'_>,
        project: Uuid,
    ) -> Result<artifacts::Retained, AppError> {
        let request = self.request;
        let artifact = serde_json::to_value(&request.artifact).map_err(|_| AppError::Internal)?;
        let id = storage::linked_id(tx, request.binding.attempt_id, artifact)
            .await
            .map_err(db)?
            .ok_or_else(refused)?;
        let item = artifacts::load_locked(tx, id).await?;
        if item.project_id != project || item.descriptor.artifact != request.artifact {
            return Err(refused());
        }
        Ok(item)
    }
}
fn range_response(
    item: artifacts::Retained,
    request: &ArtifactFetch,
) -> Result<Response, AppError> {
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
    Ok(response)
}
