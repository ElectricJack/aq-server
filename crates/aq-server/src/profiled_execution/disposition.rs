//! Explicit owner abandonment of stopped, unsettled attempts. This is a terminal
//! server disposition, not successful Graph completion or source/effect release.
use super::settlement::actor::ActorPin;
use crate::{AppState, authz::RequireOwner, error::AppError, store::User};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, Path, State},
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub operation_id: Uuid,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub operation_id: Uuid,
    pub attempt_id: Uuid,
    pub task_id: Uuid,
    pub actor_id: Uuid,
    pub disposition: String,
    pub reason: String,
    pub stop_receipt_digest: String,
    pub recorded_at_ms: i64,
}

/// First store format carrying `profiled_stopped_dispositions` (migration 66).
pub const DISPOSITION_FORMAT: u32 = 66;

/// Whether the owner terminally disposed this task. A store held at an earlier
/// migration prefix has no disposition table and so records no disposition,
/// instead of failing every poll and replacement assessment that reads it.
pub(crate) async fn task_disposed(
    conn: &mut sqlx::PgConnection,
    task: Uuid,
) -> Result<bool, AppError> {
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&mut *conn)
            .await
            .map_err(db)?;
    if version.parse::<u32>().map_err(|_| AppError::Internal)? < DISPOSITION_FORMAT {
        return Ok(false);
    }
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM profiled_stopped_dispositions WHERE task_id=$1)",
    )
    .bind(task)
    .fetch_one(&mut *conn)
    .await
    .map_err(db)
}

fn db(error: sqlx::Error) -> AppError {
    tracing::error!(%error, "stopped disposition storage failure");
    AppError::Internal
}
fn conflict() -> AppError {
    AppError::Conflict(
        "disposition requires an unsettled held attempt with verified stop proof".into(),
    )
}

/// Uses the same node -> attempt/hot locks as results, cancellation and
/// settlement. Historical receipts replay exactly, including after restart.
pub async fn abandon(
    state: &AppState,
    actor: &User,
    attempt: Uuid,
    request: &Request,
) -> Result<Receipt, AppError> {
    if attempt.is_nil()
        || request.operation_id.is_nil()
        || request.reason.trim().is_empty()
        || request.reason.len() > 4096
    {
        return Err(AppError::BadRequest(
            "invalid disposition identity or reason".into(),
        ));
    }
    let actor = ActorPin::observe(state, actor).await?;
    let node: Uuid = sqlx::query_scalar(
        "SELECT node_id FROM execution_attempts WHERE attempt_id=$1 AND kind='profiled_v1'",
    )
    .bind(attempt)
    .fetch_optional(&state.store.pool)
    .await
    .map_err(db)?
    .ok_or_else(|| AppError::NotFound("profiled attempt not found".into()))?;
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("SET LOCAL statement_timeout='5s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(node)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
    // Serialize reuse of an operation id across nodes as well as exact retries.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,0))")
        .bind(format!("aq.stopped-disposition:{}", request.operation_id))
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    let row = sqlx::query("SELECT a.task_id,a.holds_task,h.state FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) WHERE a.attempt_id=$1 FOR UPDATE OF a,h")
        .bind(attempt).fetch_one(&mut *tx).await.map_err(db)?;
    actor.fence(&mut tx).await?;
    let old = sqlx::query(
        "SELECT receipt FROM profiled_stopped_dispositions WHERE attempt_id=$1 OR operation_id=$2",
    )
    .bind(attempt)
    .bind(request.operation_id)
    .fetch_all(&mut *tx)
    .await
    .map_err(db)?;
    if !old.is_empty() {
        if old.len() != 1 {
            return Err(conflict());
        }
        let receipt: Receipt =
            serde_json::from_value(old[0].get("receipt")).map_err(|_| AppError::Internal)?;
        if receipt.attempt_id != attempt
            || receipt.operation_id != request.operation_id
            || receipt.reason != request.reason
            || receipt.actor_id != actor.id()
        {
            return Err(conflict());
        }
        tx.commit().await.map_err(db)?;
        return Ok(receipt);
    }
    let hot: Value = row.get("state");
    let unsettled: bool = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM profiled_settlement_evaluations WHERE attempt_id=$1 AND disposition='accepted') AND NOT EXISTS(SELECT 1 FROM profiled_task_completions WHERE attempt_id=$1) AND NOT EXISTS(SELECT 1 FROM profiled_auto_settlements WHERE attempt_id=$1 AND disposition IN ('prepared','possible','confirmed'))")
        .bind(attempt).fetch_one(&mut *tx).await.map_err(db)?;
    let stop: Option<String> = sqlx::query_scalar("SELECT encode(sha256(receipt_bytes),'hex') FROM profiled_stopped_receipts WHERE attempt_id=$1")
        .bind(attempt).fetch_optional(&mut *tx).await.map_err(db)?;
    if !row.get::<bool, _>("holds_task")
        || !unsettled
        || hot["status"] != "finished"
        || hot["stop"]["kind"] != "stopped"
        || stop.is_none()
        || hot["stop"]["receipt"]["digest"].as_str() != stop.as_deref()
    {
        return Err(conflict());
    }
    let recorded_at_ms = (state.clock)().timestamp_millis();
    if recorded_at_ms <= 0 {
        return Err(AppError::Internal);
    }
    let receipt = Receipt {
        operation_id: request.operation_id,
        attempt_id: attempt,
        task_id: row.get("task_id"),
        actor_id: actor.id(),
        disposition: "abandoned".into(),
        reason: request.reason.clone(),
        stop_receipt_digest: stop.ok_or_else(conflict)?,
        recorded_at_ms,
    };
    sqlx::query("SELECT set_config('aq.stopped_disposition_owner','1',true)")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("INSERT INTO profiled_stopped_dispositions(attempt_id,task_id,operation_id,actor_id,receipt) VALUES($1,$2,$3,$4,$5)")
        .bind(attempt).bind(receipt.task_id).bind(request.operation_id).bind(actor.id())
        .bind(json!(receipt)).execute(&mut *tx).await.map_err(db)?;
    sqlx::query("UPDATE execution_attempts SET holds_task=false WHERE attempt_id=$1")
        .bind(attempt)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    // Close launch-scoped access; retained source and unresolved effects keep
    // their existing owner records and are never declared quiescent here.
    sqlx::query("UPDATE agent_launch_bindings SET revoked=true,credential_verifier=NULL,credential_expires_at_ms=NULL,credential_version=credential_version+1,authority_revision=authority_revision+1 WHERE attempt_id=$1")
        .bind(attempt).execute(&mut *tx).await.map_err(db)?;
    tx.commit().await.map_err(db)?;
    Ok(receipt)
}

pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/api/execution/attempts/{id}/disposition",
            get(read).post(write),
        )
        .layer(DefaultBodyLimit::max(8192))
        .with_state(state.control_view())
}
async fn write(
    State(state): State<AppState>,
    RequireOwner(actor): RequireOwner,
    Path(id): Path<Uuid>,
    crate::admin_api::ApiJson(request): crate::admin_api::ApiJson<Request>,
) -> Result<Json<Receipt>, AppError> {
    Ok(Json(abandon(&state, &actor, id, &request).await?))
}
async fn read(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    Path(id): Path<Uuid>,
) -> Result<Json<Receipt>, AppError> {
    let value: Value =
        sqlx::query_scalar("SELECT receipt FROM profiled_stopped_dispositions WHERE attempt_id=$1")
            .bind(id)
            .fetch_optional(&state.store.pool)
            .await
            .map_err(db)?
            .ok_or_else(|| AppError::NotFound("stopped disposition not found".into()))?;
    Ok(Json(
        serde_json::from_value(value).map_err(|_| AppError::Internal)?,
    ))
}
