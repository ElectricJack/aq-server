//! Explicit negotiated machine-only report routes; no execution authority reply.
use crate::{AppState, error::AppError};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State, rejection::BytesRejection},
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
    routing::post,
};
struct Error(AppError);
impl From<AppError> for Error {
    fn from(e: AppError) -> Self {
        Self(e)
    }
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let mut r = self.0.into_response();
        r.headers_mut()
            .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
        r
    }
}
pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/v1/worker/profiled/inventory-v2", post(inventory))
        .route("/v1/worker/profiled/material-presence", post(report))
        .layer(DefaultBodyLimit::max(
            aq_admission::scoped_presence::MAX_REQUEST_BYTES,
        ))
        .with_state(state.presence_view())
        .merge(
            Router::new()
                .route(
                    "/v1/worker/profiled/inventory-status",
                    axum::routing::get(inventory_status),
                )
                .with_state(state),
        )
}
async fn inventory(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Error> {
    let auth = crate::worker_api::authenticate_context(&state, &headers).await?;
    let body = body.map_err(|_| AppError::PayloadTooLarge("typed inventory body bound".into()))?;
    let request = aq_admission::scoped_presence::decode_inventory(&body)
        .map_err(|_| AppError::BadRequest("invalid typed inventory".into()))?;
    let mut diagnostic = InventoryAttempt {
        state: &state,
        node_id: auth.id(),
        request_id: uuid::Uuid::new_v4(),
        operation_id: request.operation_id,
        stage: "validate",
        completed: false,
    };
    let result =
        super::inventory::record_with_stage(&state, &auth, &request, &mut diagnostic.stage).await;
    diagnostic.completed = true;
    let mut response = match result {
        Ok(receipt) => {
            state
                .store
                .inventory_diagnostics
                .accept(auth.id(), &receipt);
            Json(receipt).into_response()
        }
        Err(error) => {
            diagnostic.reject(diagnostic.stage, &error);
            Error(error).into_response()
        }
    };
    response.headers_mut().insert(
        "x-aq-inventory-request-id",
        diagnostic
            .request_id
            .to_string()
            .parse()
            .expect("UUID header"),
    );
    Ok(no_store(response))
}

struct InventoryAttempt<'a> {
    state: &'a AppState,
    node_id: uuid::Uuid,
    request_id: uuid::Uuid,
    operation_id: uuid::Uuid,
    stage: &'static str,
    completed: bool,
}

impl InventoryAttempt<'_> {
    fn reject(&self, stage: &'static str, error: &AppError) {
        let status = error.status().as_u16();
        let code = error.to_api_error().error;
        tracing::warn!(
            node_id = %self.node_id, request_id = %self.request_id,
            operation_id = %self.operation_id, stage, status, %code, "worker inventory rejected"
        );
        if let Ok(now) = super::now(self.state) {
            self.state.store.inventory_diagnostics.reject(
                self.node_id,
                aq_dashboard_contract::InventoryRejection {
                    request_id: self.request_id,
                    observed_at_ms: now,
                    stage: stage.into(),
                    status,
                    code,
                },
            );
        }
    }
}

impl Drop for InventoryAttempt<'_> {
    fn drop(&mut self) {
        // Admission cancels the handler future at its existing request deadline.
        if !self.completed {
            self.reject(self.stage, &AppError::Busy);
        }
    }
}

async fn inventory_status(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, Error> {
    let auth = crate::worker_api::authenticate_context(&state, &headers).await?;
    let observation = state.store.inventory_diagnostics.get(auth.id());
    Ok(no_store(
        Json(serde_json::json!({
            "server_time_ms": super::now(&state)?,
            "accepted": observation.accepted,
            "rejection": observation.rejection,
        }))
        .into_response(),
    ))
}

async fn report(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Error> {
    let auth = crate::worker_api::authenticate_context(&state, &headers).await?;
    let body = body.map_err(|_| AppError::PayloadTooLarge("scoped presence body bound".into()))?;
    let request = aq_admission::scoped_presence::decode_report(&body)
        .map_err(|_| AppError::BadRequest("invalid scoped presence".into()))?;
    let receipt = super::report::record(&state, &auth, &request).await?;
    Ok(no_store(Json(receipt).into_response()))
}
fn no_store(mut response: Response) -> Response {
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}
