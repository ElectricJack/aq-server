//! Service-machine routes (spec 0007 phase 1), under a node credential.
use aq_machine_control::{self as mc, Error as McError, NewSlot};
use aq_protocol::{
    ControlRequest, ControlResponse, HandoffNotice, MAX_MACHINE_SLOTS, SlotEnrollment,
    SlotsRequest, SlotsResponse, valid_machine_key,
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
        .with_state(state.clone())
        .merge(
            Router::new()
                .route("/v1/worker/control", post(control))
                .route("/v1/worker/handoffs", post(handoffs))
                .route("/v1/worker/lost-attempts", post(lost_attempts))
                .with_state(state.control_view()),
        )
        .layer(DefaultBodyLimit::max(16 * 1024))
}

pub(crate) fn mapped(e: McError) -> AppError {
    match e {
        McError::KeyInUse => AppError::Conflict("machine key is in use by another machine; revoke it from the Swarm page or choose another key".into()),
        McError::Invalid(m) => AppError::BadRequest(m.into()),
        McError::Refused(m) => AppError::Conflict(m.into()),
        e @ McError::MachineNode(_) => AppError::Conflict(e.to_string()),
        McError::Conflict { .. } => AppError::Conflict("generation".into()),
        McError::NotFound => AppError::Internal,
        // Contention is a retryable 503 (`Busy`, `Retry-After`); other database errors stay 500.
        McError::Db(e) => crate::error::db(e),
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
    let key = &req.machine_key;
    mc::lock_machine(&mut tx, key).await.map_err(mapped)?;
    lock_current_node(&mut tx, &auth).await?;
    let caller = mc::claim_key(&mut tx, auth.id(), key)
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
    let minted = mc::mint_slots(
        &mut tx,
        &caller,
        mc::SlotRequest {
            wanted: req.slots,
            template: &new,
            hashes: &hashes,
        },
        (state.clock)(),
    )
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

async fn control(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<ControlRequest>,
) -> Result<Json<ControlResponse>, AppError> {
    let auth = authenticate_context(&state, &headers).await?;
    if ![1, aq_protocol::NODE_SLOTS_SCHEMA_VERSION].contains(&req.schema_version) {
        return Err(AppError::BadRequest("schema_version".into()));
    }
    let node = auth.node();
    let now = (state.clock)();
    let mut conn = state
        .store
        .pool
        .acquire()
        .await
        .map_err(|_| AppError::Internal)?;
    let me = mc::machine_of(&mut conn, node.id).await.map_err(mapped)?;
    let Some(key) = me.machine_key else {
        return Err(AppError::Conflict(
            "this node is not a service machine; enroll it with aq-worker service enroll".into(),
        ));
    };
    let c = mc::control(&mut conn, &key).await.map_err(mapped)?;
    let handoff = mc::pending_handoffs(&mut conn, &key)
        .await
        .map_err(mapped)?;
    let slots = if req.schema_version == 2 {
        Some(mc::slots::count(&mut conn, node.id).await.map_err(mapped)?)
    } else {
        None
    };
    drop(conn);
    state
        .store
        .record_heartbeat(node.id, &node.worker_version, node.protocol_version, now)
        .await?;
    Ok(Json(ControlResponse {
        schema_version: req.schema_version,
        machine_key: key,
        desired: c.desired,
        generation: c.generation,
        handoff,
        qualification: None,
        slots,
    }))
}

async fn handoffs(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiJson(mut req): ApiJson<HandoffNotice>,
) -> Result<Json<HandoffNotice>, AppError> {
    let auth = authenticate_context(&state, &headers).await?;
    if req.schema_version != 1 {
        return Err(AppError::BadRequest("schema_version".into()));
    }
    let mut conn = state
        .store
        .pool
        .acquire()
        .await
        .map_err(|_| AppError::Internal)?;
    let now = (state.clock)();
    req.reason = mc::record_handoff(&mut conn, auth.id(), &req, now)
        .await
        .map_err(mapped)?;
    Ok(Json(req))
}

async fn lost_attempts(
    State(state): State<AppState>,
    headers: HeaderMap,
    ApiJson(req): ApiJson<aq_protocol::LostAttemptRelease>,
) -> Result<Json<aq_protocol::LostAttemptReleased>, AppError> {
    let auth = authenticate_context(&state, &headers).await?;
    Ok(Json(
        crate::profiled_execution::lost::release(&state, &auth, &req).await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    /// M1 residual: contention on a machine route is a retryable 503 (`Busy`, `Retry-After`);
    /// any other database error stays a 500.
    #[tokio::test]
    async fn a_contended_database_error_is_busy_and_any_other_is_internal() {
        let db = crate::test_support::TestDb::create().await;
        let pool = db.pool().await;
        let mut held = pool.begin().await.unwrap();
        let lock = "SELECT pg_advisory_xact_lock(57)";
        sqlx::query(lock).execute(&mut *held).await.unwrap();
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("SET LOCAL lock_timeout='20ms'")
            .execute(&mut *tx)
            .await
            .unwrap();
        let timeout = sqlx::query(lock).execute(&mut *tx).await.unwrap_err();
        assert!(matches!(mapped(McError::Db(timeout)), AppError::Busy));
        let other = McError::Db(sqlx::Error::RowNotFound);
        assert!(matches!(mapped(other), AppError::Internal));
    }
}
