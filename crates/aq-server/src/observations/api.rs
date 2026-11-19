//! Bounded observation routes mounted on the separate machine and human listeners.
//! Worker bearer credentials only. The batch route enrolls only catalog-derived
//! sources for the caller's own attempts; workers never supply source policy.
use super::*;
use crate::{admin_api::ApiJson, authz::CurrentUser, error::AppError};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, header},
    routing::{get, post},
};

pub fn routes(mut state: AppState) -> Router<AppState> {
    state.store = state.store.observation_view();
    Router::new()
        .route("/v1/worker/observations/batch", post(batch))
        .route("/v1/worker/observations/vm-time", post(vm_time))
        .layer(DefaultBodyLimit::max(MAX_BATCH_BYTES))
        .with_state(state)
}
async fn vm_time(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiJson(fact): ApiJson<aq_accounting::vm_time::Fact>,
) -> std::result::Result<([(header::HeaderName, &'static str); 1], Json<Value>), AppError> {
    let node = crate::worker_api::authenticate_context(&state, &headers).await?;
    super::vm_time::record(&state, &node, &fact).await?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({"recorded": true})),
    ))
}
/// Transport ownership only, not source catalog enrollment or execution authority.
pub fn transport_routes(mut state: AppState) -> Router<AppState> {
    state.store = state.store.observation_view();
    Router::new()
        .route(
            "/v1/worker/observations/transport",
            post(transport_mutation).get(transport_status),
        )
        .layer(DefaultBodyLimit::max(aq_observation_admission::MAX_BYTES))
        .with_state(state)
}
async fn transport_mutation(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<aq_observation_admission::Request>,
) -> std::result::Result<
    (
        [(header::HeaderName, &'static str); 1],
        Json<aq_observation_admission::Reply>,
    ),
    AppError,
> {
    let node = crate::worker_api::authenticate_context(&state, &headers).await?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(transport::mutate_authenticated(&state, &node, &request).await?),
    ))
}
async fn transport_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> std::result::Result<
    (
        [(header::HeaderName, &'static str); 1],
        Json<aq_observation_admission::Status>,
    ),
    AppError,
> {
    let node = crate::worker_api::authenticate_context(&state, &headers).await?;
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(transport::status_authenticated(&state, &node).await?),
    ))
}
pub fn admin_routes(mut state: AppState) -> Router<AppState> {
    state.store = state.store.observation_view();
    Router::new()
        .route("/api/observations/sessions/{attempt}/events", get(events))
        .route("/api/observations/sessions/{attempt}/usage", get(usage))
        .with_state(state)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    #[serde(default)]
    after_sequence: u64,
    through_sequence: Option<u64>,
    #[serde(default = "page_limit")]
    limit: u32,
}
fn page_limit() -> u32 {
    16
}
async fn events(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(attempt): Path<Uuid>,
    Query(page): Query<Page>,
) -> std::result::Result<
    (
        [(header::HeaderName, &'static str); 1],
        Json<inspection::EventsPage>,
    ),
    AppError,
> {
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(
            inspection::events(
                &state,
                user.id,
                attempt,
                page.after_sequence,
                page.through_sequence,
                page.limit,
            )
            .await?,
        ),
    ))
}
async fn usage(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(attempt): Path<Uuid>,
) -> std::result::Result<
    (
        [(header::HeaderName, &'static str); 1],
        Json<inspection::SessionUsage>,
    ),
    AppError,
> {
    Ok((
        [(header::CACHE_CONTROL, "no-store")],
        Json(inspection::usage(&state, user.id, attempt).await?),
    ))
}
impl From<Error> for AppError {
    fn from(error: Error) -> Self {
        match error {
            Error::Refused => Self::Forbidden("observation source is not admitted".into()),
            Error::Conflict => Self::Conflict("observation identity or cursor conflicts".into()),
            Error::Capacity => Self::Maintenance("observation retention quota exhausted".into()),
            Error::Invalid => Self::BadRequest("invalid observation document".into()),
            Error::Authority(v) => v.into(),
            Error::Database(_) => Self::Internal,
        }
    }
}
async fn batch(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiJson(packet): ApiJson<Packet>,
) -> std::result::Result<([(header::HeaderName, &'static str); 1], Json<Receipt>), AppError> {
    let node = crate::delegated_permits::runtime::authenticate_runner(
        &state,
        &headers,
        aq_admission_sandbox::sandbox::RunnerOperation::Observe,
        None,
    )
    .await?;
    let Message::Batch(batch) = &packet.body.message else {
        return Err(crate::delegated_permits::runtime::refused());
    };
    crate::delegated_permits::runtime::require_attempts(
        &node,
        batch
            .events
            .iter()
            .map(|event| event.body.binding.attempt_id),
    )?;
    for event in &batch.events {
        crate::delegated_permits::runtime::require_session(&node, &event.body.binding.session)?;
    }
    // Production attempt path: enroll each attempt's trusted catalog source
    // (idempotent) before ingest. Uncovered or refused attempts are left to
    // ingest, which refuses events without an admitted source.
    let attempts: std::collections::BTreeSet<Uuid> = batch
        .events
        .iter()
        .map(|event| event.body.binding.attempt_id)
        .collect();
    for attempt in attempts {
        let _ = catalog::register_attempt(&state, attempt, Some(node.id())).await;
    }
    let receipt = ingest_authenticated(&state, &node, &packet).await?;
    Ok(([(header::CACHE_CONTROL, "no-store")], Json(receipt)))
}
