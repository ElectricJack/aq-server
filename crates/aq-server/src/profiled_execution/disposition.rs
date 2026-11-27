//! Explicit owner abandonment of stopped, unsettled attempts. This is a terminal
//! server disposition, not successful Graph completion or source/effect release.
use super::settlement::actor::ActorPin;
use crate::{AppState, admin_api::ApiJson, authz::RequireOwner, error::AppError, store::User};
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
        "disposition requires an unsettled held attempt with verified stop proof or a launch closure".into(),
    )
}

fn replay(
    old: &[sqlx::postgres::PgRow],
    attempt: Uuid,
    request: &Request,
    actor: Uuid,
) -> Result<Receipt, AppError> {
    if old.len() != 1 {
        return Err(conflict());
    }
    let receipt: Receipt =
        serde_json::from_value(old[0].get("receipt")).map_err(|_| AppError::Internal)?;
    if receipt.attempt_id != attempt
        || receipt.operation_id != request.operation_id
        || receipt.reason != request.reason
        || receipt.actor_id != actor
    {
        return Err(conflict());
    }
    Ok(receipt)
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
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR KEY SHARE")
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
        let receipt = replay(&old, attempt, request, actor.id())?;
        tx.commit().await.map_err(db)?;
        return Ok(receipt);
    }
    let hot: Value = row.get("state");
    let unsettled: bool = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM profiled_settlement_evaluations WHERE attempt_id=$1 AND disposition='accepted') AND NOT EXISTS(SELECT 1 FROM profiled_task_completions WHERE attempt_id=$1) AND NOT EXISTS(SELECT 1 FROM profiled_auto_settlements WHERE attempt_id=$1 AND disposition IN ('prepared','possible','confirmed')) AND NOT EXISTS(SELECT 1 FROM profiled_delivery_evidence WHERE attempt_id=$1 AND state='indeterminate')")
        .bind(attempt).fetch_one(&mut *tx).await.map_err(db)?;
    // A published delivery no longer blocks: a result delivered while its settlement was
    // still held is never settled again, and the owner had no way to release its claim.
    let stop: Option<String> = sqlx::query_scalar("SELECT encode(sha256(receipt_bytes),'hex') FROM profiled_stopped_receipts WHERE attempt_id=$1")
        .bind(attempt).fetch_optional(&mut *tx).await.map_err(db)?;
    let stopped = hot["status"] == "finished"
        && hot["stop"]["kind"] == "stopped"
        && stop.is_some()
        && hot["stop"]["receipt"]["digest"].as_str() == stop.as_deref();
    // A granted launch its worker closed before the agent started: its stop proof is the
    // launch closure's receipt. The server keeps such an attempt for an operator when the
    // closure does not prove that nothing ran, and this is that operator's action.
    let closed =
        hot["status"] == "cancelling" && hot["revoked"] == true && hot["stop"]["kind"] == "unknown";
    let stop = if stopped || !closed {
        stop
    } else {
        sqlx::query_scalar("SELECT receipt_digest FROM profiled_launch_closures WHERE attempt_id=$1 AND encode(sha256(receipt_bytes),'hex')=receipt_digest")
            .bind(attempt).fetch_optional(&mut *tx).await.map_err(db)?
    };
    if !row.get::<bool, _>("holds_task") || !unsettled || !(stopped || closed) || stop.is_none() {
        // Granted and never started: it may still start until its grant expires, and then the
        // server closes it itself.
        let pending = aq_machine_control::lapse::pending(&mut tx, attempt).await;
        return Err(match pending.map_err(crate::machine_api::mapped)? {
            Some((_, at)) => AppError::Conflict(format!(
                "this granted attempt has not started and may still start until its grant expires at {}; the server then closes it as {} and frees its task and node, without a disposition",
                chrono::DateTime::from_timestamp_millis(at)
                    .unwrap_or_default()
                    .to_rfc3339(),
                aq_machine_control::lapse::REASON
            )),
            None => conflict(),
        });
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
    super::journal::revoke(&mut tx, attempt).await?;
    tx.commit().await.map_err(db)?;
    Ok(receipt)
}

pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/api/execution/attempts/{id}/disposition",
            get(read).post(write),
        )
        .route(
            "/api/execution/projects/{project}/attempts/{id}/closed-launch-settlement",
            axum::routing::post(settle_closed),
        )
        .layer(DefaultBodyLimit::max(8192))
        .with_state(state.control_view())
}
async fn write(
    State(state): State<AppState>,
    RequireOwner(actor): RequireOwner,
    Path(id): Path<Uuid>,
    ApiJson(request): ApiJson<Request>,
) -> Result<Json<Receipt>, AppError> {
    Ok(Json(abandon(&state, &actor, id, &request).await?))
}
/// `attempt.settle_closed_launch`: the attested settlement of a closed launch that does not
/// prove nonstart (`aq_machine_control::closed_launch`); never a disposition. Idempotent.
async fn settle_closed(
    State(state): State<AppState>,
    RequireOwner(actor): RequireOwner,
    Path((project, attempt)): Path<(Uuid, Uuid)>,
    ApiJson(r): ApiJson<Request>,
) -> Result<Json<Value>, AppError> {
    use super::closure::settlement::{ClosureSettlement::Held, settle_and_release};
    use aq_machine_control::closed_launch::{Attestation, admit, receipt};
    let (pool, mc) = (&state.store.pool, crate::machine_api::mapped);
    let retention = state.source_retention.as_deref();
    let socket = retention.map(|r| r.config.git_socket.as_path());
    #[cfg(feature = "delivery_playbook")]
    let head = aq_delivery::git_grant::granted_ref_head(pool, socket, attempt).await;
    #[cfg(not(feature = "delivery_playbook"))]
    let head = socket.and(None);
    let actor = (actor.id, actor.email);
    let a = Attestation::new(r.operation_id, r.reason, project, actor, head);
    admit(pool, attempt, &a).await.map_err(mc)?;
    let settled = settle_and_release(&state, attempt, Some(&a)).await?;
    match (receipt(pool, attempt, &a).await.map_err(mc)?, settled) {
        (Some(r), s) => Ok(Json(json!({"attestation": r, "settlement": s}))),
        (None, Held(code)) => Err(AppError::Conflict(code.into())),
        (None, _) => Err(AppError::Conflict("closed_launch_not_held".into())),
    }
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
