//! Durable finite action boundary backed by existing domain owners.
pub mod contract;
pub(crate) mod existing;
pub mod journal;
pub(crate) mod json;
mod registry;
use super::{DashboardUser, contract::Scope, errors::ApiError};
use crate::{AppState, store::User};
use aq_agent_grants::store::DraftReservation;
use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
pub use contract::*;
pub use registry::{ActionKind, Registry};
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// A committed receipt survives cancellation by an outer request deadline.
/// This is request-local evidence only; the SQL journal remains authoritative.
#[derive(Clone, Default)]
pub(crate) struct AdmissionReceipt(Arc<Mutex<ReceiptState>>);
#[derive(Default)]
struct ReceiptState {
    response: Option<OperationResponse>,
    authorization: Option<(AppState, User, Arc<dyn registry::Adapter>)>,
}
impl AdmissionReceipt {
    fn record(&self, value: OperationResponse) {
        self.0.lock().unwrap_or_else(|p| p.into_inner()).response = Some(value);
    }
    fn pin(&self, state: &AppState, user: &User, adapter: Arc<dyn registry::Adapter>) {
        self.0
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .authorization = Some((state.clone(), user.clone(), adapter));
    }
    pub(crate) async fn retained_response(&self) -> Option<axum::response::Response> {
        use axum::response::IntoResponse;
        let (value, (state, user, adapter)) = {
            let retained = self.0.lock().unwrap_or_else(|p| p.into_inner());
            (retained.response.clone()?, retained.authorization.clone()?)
        };
        // An outer deadline cannot turn an old admission into disclosure authority.
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            adapter.authorize_lookup(&state, &user, &value.scope, Some(&value.data)),
        )
        .await
        .ok()?
        .ok()?;
        Some((operation_status(&value.data), Json(value)).into_response())
    }
}
pub(crate) struct AgentAdmission<'a> {
    pub caller: &'a crate::agent_access::grant_check::AgentCaller,
    pub headers: &'a HeaderMap,
    pub reservation: Option<DraftReservation>,
}

enum RequestOrigin<'a> {
    Union,
    Legacy,
    Agent(AgentAdmission<'a>),
}

pub(crate) fn operation_status(operation: &Operation) -> StatusCode {
    if matches!(
        operation.disposition,
        Disposition::Pending | Disposition::Unknown
    ) {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    }
}

pub fn routes(registry: Arc<Registry>) -> Router<AppState> {
    Router::new()
        .route("/api/v1/actions", post(post_action))
        .route("/api/v1/operations/{id}", get(get_operation))
        .layer(DefaultBodyLimit::max(65536))
        .layer(Extension(registry))
}
async fn post_action(
    State(state): State<AppState>,
    DashboardUser(user): DashboardUser,
    Extension(registry): Extension<Arc<Registry>>,
    receipt: Option<Extension<AdmissionReceipt>>,
    body: Result<Json<json::UniqueJson>, axum::extract::rejection::JsonRejection>,
) -> Result<(StatusCode, Json<OperationResponse>), ApiError> {
    let Json(json::UniqueJson(raw)) = body.map_err(|e| ApiError::for_status(e.status()))?;
    let request: ActionRequest = serde_json::from_value(raw).map_err(|_| ApiError::invalid())?;
    let operation = submit_observed(
        &state,
        &registry,
        &user,
        request,
        receipt.as_ref().map(|r| &r.0),
        RequestOrigin::Union,
    )
    .await?;
    let status = operation_status(&operation);
    let response = response(&state, operation);
    if let Some(receipt) = receipt {
        receipt.record(response.clone());
    }
    Ok((status, Json(response)))
}
async fn get_operation(
    State(state): State<AppState>,
    DashboardUser(user): DashboardUser,
    Extension(registry): Extension<Arc<Registry>>,
    path: Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<Json<OperationResponse>, ApiError> {
    let Path(id) = path.map_err(|_| ApiError::invalid())?;
    let lookup = match journal::lookup_for_authorization(&state.store.pool, user.id, id).await? {
        Some(lookup) => lookup,
        None => {
            let adapter = crate::agent_access::human::TaskCreate {
                runtime: Arc::new(crate::agent_access::human::ReadRuntime::new(&state)),
                original: None,
            };
            let (request, scope, created) = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                adapter.retained_request(&state, &user, id),
            )
            .await
            .map_err(|_| ApiError::unavailable())?
            .map_err(neutral_lookup)?
            .ok_or_else(|| ApiError::for_status(StatusCode::NOT_FOUND))?;
            let digest = canonical_digest(&request, &scope)?;
            // Import metadata only. The admission dispatch flag is intentionally ignored:
            // the native owner already owns any effect under this immutable ID.
            let (operation, _) = journal::admit(
                &state,
                user.id,
                &request,
                scope.clone(),
                digest,
                created,
                None,
            )
            .await?;
            journal::Lookup {
                kind: request.kind,
                scope,
                operation: Some(operation),
            }
        }
    };
    let adapter = registry
        .get(&lookup.kind)
        .map_err(|_| ApiError::for_status(StatusCode::NOT_FOUND))?;
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        adapter.authorize_lookup(&state, &user, &lookup.scope, lookup.operation.as_ref()),
    )
    .await
    .map_err(|_| ApiError::unavailable())?
    .map_err(|error| {
        if matches!(error.status().as_u16(), 401 | 403 | 404) {
            ApiError::for_status(StatusCode::NOT_FOUND)
        } else {
            error
        }
    })?;
    let operation = lookup.operation.ok_or_else(|| journal::retired(id))?;
    let operation = reconcile(&state, &user, adapter.as_ref(), operation)
        .await
        .map_err(neutral_lookup)?;
    Ok(Json(response(&state, operation)))
}
fn neutral_lookup(error: ApiError) -> ApiError {
    if matches!(error.status().as_u16(), 401 | 403 | 404) {
        ApiError::for_status(StatusCode::NOT_FOUND)
    } else {
        error
    }
}
pub(crate) fn response(state: &AppState, operation: Operation) -> OperationResponse {
    use super::contract::{Completeness, Coverage, Observation, Quality, Timestamp};
    let now = (state.clock)().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    OperationResponse {
        api_version: ActionVersion::V1,
        request_id: Uuid::new_v4(),
        server_time: Timestamp(now.clone()),
        scope: operation.scope.clone(),
        pending_operations: if matches!(
            operation.disposition,
            Disposition::Pending | Disposition::Unknown
        ) {
            vec![operation.operation_id]
        } else {
            vec![]
        },
        data: operation,
        observation: Observation {
            observed_at: Timestamp(now.clone()),
            fresh_until: Timestamp(now),
            source: "aq_operation_journal".into(),
            quality: Quality::Reported,
            coverage: Completeness::Complete,
        },
        coverage: Coverage {
            state: Completeness::Complete,
            reason: None,
        },
    }
}
/// Creation-only machine registries pass an authenticated opaque session identity.
pub(crate) async fn submit_scoped(
    state: &AppState,
    registry: &Registry,
    user: &User,
    request: ActionRequest,
    admission: Option<AgentAdmission<'_>>,
) -> Result<Operation, ApiError> {
    let origin = admission.map_or(RequestOrigin::Union, RequestOrigin::Agent);
    submit_observed(state, registry, user, request, None, origin).await
}
pub(crate) async fn lookup_scoped(
    state: &AppState,
    registry: &Registry,
    user: &User,
    id: Uuid,
) -> Result<Operation, ApiError> {
    let lookup = journal::lookup_for_authorization(&state.store.pool, user.id, id)
        .await?
        .ok_or_else(|| ApiError::for_status(StatusCode::NOT_FOUND))?;
    let adapter = registry
        .get(&lookup.kind)
        .map_err(|_| ApiError::for_status(StatusCode::NOT_FOUND))?;
    adapter
        .authorize_lookup(state, user, &lookup.scope, lookup.operation.as_ref())
        .await
        .map_err(neutral_lookup)?;
    let op = lookup.operation.ok_or_else(|| journal::retired(id))?;
    reconcile(state, user, adapter.as_ref(), op)
        .await
        .map_err(neutral_lookup)
}
/// Resolve an exact replay before checking permissions against changed Graph facts.
pub(crate) async fn replay_scoped(
    state: &AppState,
    registry: &Registry,
    user: &User,
    request: &ActionRequest,
    scope: &Scope,
) -> Result<Option<Operation>, ApiError> {
    request.validate()?;
    let Some(existing) =
        journal::lookup_for_authorization(&state.store.pool, user.id, request.operation_id).await?
    else {
        return Ok(None);
    };
    if existing.kind != request.kind || existing.scope != *scope {
        return Err(ApiError::for_status(StatusCode::CONFLICT));
    }
    if let Some(op) = existing.operation
        && canonical_digest(request, scope)? != op.request_digest
    {
        return Err(ApiError::for_status(StatusCode::CONFLICT));
    }
    lookup_scoped(state, registry, user, request.operation_id)
        .await
        .map(Some)
}
pub(crate) async fn submit_legacy(
    state: &AppState,
    registry: &Registry,
    user: &User,
    request: ActionRequest,
) -> Result<Operation, ApiError> {
    submit_observed(state, registry, user, request, None, RequestOrigin::Legacy).await
}
async fn submit_observed(
    state: &AppState,
    registry: &Registry,
    user: &User,
    request: ActionRequest,
    receipt: Option<&AdmissionReceipt>,
    origin: RequestOrigin<'_>,
) -> Result<Operation, ApiError> {
    let legacy = matches!(origin, RequestOrigin::Legacy);
    if legacy {
        // Native supervisor bodies allow JSON escaping within the shipped 128 KiB envelope.
        request.validate_with_limit(131072)?;
    } else {
        request.validate()?;
    }
    let adapter = registry.get(&request.kind)?;
    let scope = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        adapter.authorize(state, user, &request),
    )
    .await
    .map_err(|_| ApiError::unavailable())??;
    let digest = canonical_digest(&request, &scope)?;
    let (original, dispatch) = journal::admit(
        state,
        user.id,
        &request,
        scope,
        digest,
        (state.clock)(),
        match origin {
            RequestOrigin::Agent(admission) => Some(admission),
            _ => None,
        },
    )
    .await?;
    if let Some(receipt) = receipt {
        receipt.pin(state, user, adapter.clone());
        receipt.record(response(state, original.clone()));
    }
    if !dispatch {
        return reconcile(state, user, adapter.as_ref(), original).await;
    }
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        adapter.execute(state, user, &request),
    )
    .await
    .unwrap_or_else(|_| Outcome::unknown());
    let completed = finish(state, user, original, outcome).await?;
    if !legacy {
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            adapter.authorize_lookup(state, user, &completed.scope, Some(&completed)),
        )
        .await
        .map_err(|_| ApiError::unavailable().operation(completed.operation_id))??;
    }
    Ok(completed)
}
pub(crate) async fn finish(
    state: &AppState,
    user: &User,
    original: Operation,
    outcome: Outcome,
) -> Result<Operation, ApiError> {
    let now = (state.clock)();
    let timestamp = now.to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let Some(completed) =
        original.with_outcome(outcome, timestamp, aq_machine_control::holds::valid_code)
    else {
        return Ok(original);
    };
    if journal::finish(&state.store.pool, user.id, &completed, now)
        .await
        .is_err()
    {
        // The receipt write itself may have committed. Re-read if possible;
        // otherwise return the Pending disposition whose admission is proven.
        return Ok(
            journal::lookup(&state.store.pool, user.id, original.operation_id)
                .await
                .unwrap_or(original),
        );
    }
    Ok(completed)
}
async fn reconcile(
    state: &AppState,
    user: &User,
    adapter: &dyn registry::Adapter,
    original: Operation,
) -> Result<Operation, ApiError> {
    let scope = original.scope.clone();
    let completed = if matches!(
        original.disposition,
        Disposition::Pending | Disposition::Unknown
    ) {
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            adapter.reconcile(state, user, &original),
        )
        .await
        .map_err(|_| ApiError::unavailable())??;
        match outcome {
            Some(outcome) => finish(state, user, original, outcome).await?,
            None => original,
        }
    } else {
        original
    };
    // Every retained response, including a terminal replay, requires current
    // lookup authority after journal/native IO and validation of returned refs.
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        adapter.authorize_lookup(state, user, &scope, Some(&completed)),
    )
    .await
    .map_err(|_| ApiError::unavailable())??;
    Ok(completed)
}

#[cfg(test)]
async fn submit(
    state: &AppState,
    registry: &Registry,
    user: &User,
    request: ActionRequest,
) -> Result<Operation, ApiError> {
    submit_observed(state, registry, user, request, None, RequestOrigin::Union).await
}
fn canonical_digest(request: &ActionRequest, scope: &Scope) -> Result<String, ApiError> {
    use sha2::{Digest, Sha256};
    let mut canonical = serde_json::json!({"request":request,"scope":scope});
    canonical.sort_all_objects();
    let bytes = serde_json::to_vec(&canonical).map_err(|_| ApiError::invalid())?;
    Ok(format!("sha256:{}", hex::encode(Sha256::digest(bytes))))
}
#[cfg(test)]
mod tests;
