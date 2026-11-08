//! Service-machine routes (spec 0007 phase 1), under a node credential.
use aq_machine_control::{self as mc, Error as McError, NewSlot};
use aq_protocol::{
    MAX_MACHINE_SLOTS, SlotEnrollment, SlotsRequest, SlotsResponse, valid_machine_key,
};
use axum::extract::{DefaultBodyLimit, State};
use axum::http::HeaderMap;
use axum::routing::post;
use axum::{Json, Router};

use crate::AppState;
use crate::admin_api::ApiJson;
use crate::error::AppError;
use crate::tokens::{new_node_credential, verifier};
use crate::worker_api::{authenticate_context, lock_current_node};

pub fn machine_routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/v1/worker/slots", post(slots))
        .with_state(state)
        .layer(DefaultBodyLimit::max(16 * 1024))
}

fn mapped(e: McError) -> AppError {
    match e {
        McError::KeyInUse => AppError::Conflict("machine key is in use by another machine; revoke it from the Swarm page or choose another key".into()),
        McError::Invalid(m) => AppError::BadRequest(m.into()),
        McError::Conflict { .. } => AppError::Conflict("generation".into()),
        McError::NotFound | McError::Db(_) => AppError::Internal,
    }
}

async fn slots(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<SlotsRequest>,
) -> Result<Json<SlotsResponse>, AppError> {
    let auth = authenticate_context(&state, &headers).await?;
    if req.schema_version != 1 {
        return Err(AppError::BadRequest("schema_version".into()));
    }
    if !valid_machine_key(&req.machine_key) {
        return Err(AppError::BadRequest(
            "machine_key must be 1-63 characters of [a-z0-9-], not starting with -".into(),
        ));
    }
    if !(1..=MAX_MACHINE_SLOTS).contains(&req.slots) {
        return Err(AppError::BadRequest("slots must be 1-8".into()));
    }
    let mut tx = state
        .store
        .pool
        .begin()
        .await
        .map_err(|_| AppError::Internal)?;
    lock_current_node(&mut tx, &auth).await?;
    let caller = mc::claim_key(&mut tx, auth.id(), &req.machine_key)
        .await
        .map_err(mapped)?;
    let credentials: Vec<String> = (1..req.slots).map(|_| new_node_credential()).collect();
    let hashes: Vec<String> = credentials.iter().map(|c| verifier(c)).collect();
    let node = auth.node();
    let new = NewSlot {
        base_name: &node.name,
        worker_version: &node.worker_version,
        protocol_version: node.protocol_version,
    };
    let minted = mc::mint_slots(&mut tx, &caller, req.slots, &new, &hashes, (state.clock)())
        .await
        .map_err(mapped)?;
    tx.commit().await.map_err(|_| AppError::Internal)?;
    let slots = minted
        .into_iter()
        .map(|m| SlotEnrollment {
            slot: m.slot,
            node_id: m.node_id,
            credential: m.created.then(|| credentials[m.slot as usize - 2].clone()),
        })
        .collect();
    Ok(Json(SlotsResponse {
        schema_version: 1,
        machine_key: req.machine_key,
        slots,
    }))
}
