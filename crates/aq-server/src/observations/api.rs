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

type PrivateJson<T> = ([(header::HeaderName, &'static str); 1], Json<T>);

fn no_store<T>(body: T) -> PrivateJson<T> {
    ([(header::CACHE_CONTROL, "no-store")], Json(body))
}

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
) -> std::result::Result<PrivateJson<Value>, AppError> {
    let node = crate::worker_api::authenticate_context(&state, &headers).await?;
    super::vm_time::record(&state, &node, &fact).await?;
    Ok(no_store(json!({"recorded": true})))
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
) -> std::result::Result<PrivateJson<aq_observation_admission::Reply>, AppError> {
    let node = crate::worker_api::authenticate_context(&state, &headers).await?;
    Ok(no_store(
        transport::mutate_authenticated(&state, &node, &request).await?,
    ))
}
async fn transport_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> std::result::Result<PrivateJson<aq_observation_admission::Status>, AppError> {
    let node = crate::worker_api::authenticate_context(&state, &headers).await?;
    Ok(no_store(
        transport::status_authenticated(&state, &node).await?,
    ))
}
pub fn admin_routes(mut state: AppState) -> Router<AppState> {
    state.store = state.store.observation_view();
    Router::new()
        .route("/api/observations/sessions/{attempt}/events", get(events))
        .route("/api/observations/sessions/{attempt}/usage", get(usage))
        .with_state(state)
}
async fn events(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(attempt): Path<Uuid>,
    Query(page): Query<inspection::EventPageQuery>,
) -> std::result::Result<PrivateJson<inspection::EventsPage>, AppError> {
    Ok(no_store(
        inspection::events(&state, user.id, attempt, page).await?,
    ))
}
async fn usage(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(attempt): Path<Uuid>,
) -> std::result::Result<PrivateJson<inspection::SessionUsage>, AppError> {
    Ok(no_store(inspection::usage(&state, user.id, attempt).await?))
}
impl From<Error> for AppError {
    fn from(error: Error) -> Self {
        match error {
            Error::Refused => Self::Forbidden("observation source is not admitted".into()),
            Error::Conflict => Self::Conflict("observation identity or cursor conflicts".into()),
            Error::Capacity => Self::Maintenance("observation retention quota exhausted".into()),
            Error::Limit(message) => Self::Maintenance(message),
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
) -> std::result::Result<impl axum::response::IntoResponse, AppError> {
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
    // ingest, which refuses and lists each source without an enrollment.
    let attempts: std::collections::BTreeSet<Uuid> = batch
        .events
        .iter()
        .map(|event| event.body.binding.attempt_id)
        .collect();
    for attempt in attempts {
        // A limit refusal is reported as itself, not as "source not admitted".
        let enrolled = catalog::register_attempt(&state, attempt, Some(node.id())).await;
        if let Err(limit @ Error::Limit(_)) = enrolled {
            return Err(limit.into());
        }
    }
    let receipt = ingest_authenticated(&state, &node, &packet).await?;
    Ok(no_store(receipt))
}
