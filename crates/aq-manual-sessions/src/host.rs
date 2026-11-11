//! Owner host shells, separate from agent lifetime and transcript storage.
use super::*;
use axum::{
    Router,
    extract::{DefaultBodyLimit, Path, State},
    routing::{get, post},
};

pub(super) fn human_routes() -> Router<Service> {
    Router::new()
        .route("/api/worker-terminals/{node}", get(inspect).post(open))
        .route("/api/worker-terminals/shells/{id}/stop", post(stop))
        .route(
            "/api/worker-terminals/shells/{id}/terminal/connect",
            post(terminal::connect),
        )
        .route(
            "/api/worker-terminals/shells/{id}/terminal/live",
            get(stream::browser),
        )
        .layer(DefaultBodyLimit::max(4096))
}
pub(super) fn worker_routes() -> Router<Service> {
    Router::new()
        .route("/v1/worker/host-terminals", post(poll))
        .route(
            "/v1/worker/host-terminals/{id}/terminal-stream",
            get(stream::worker),
        )
        .layer(DefaultBodyLimit::max(4096))
}
pub(super) async fn subject(s: &Service, id: Uuid) -> Result<(Uuid, bool)> {
    let row: Option<(Uuid,bool)>=sqlx::query_as("SELECT h.node_id,h.desired_running AND h.status='running' AND h.worker_seen_at_ms>$2 AND n.revoked_at IS NULL FROM worker_host_shells h JOIN nodes n ON n.id=h.node_id WHERE h.id=$1")
        .bind(id).bind(now()-15000).fetch_optional(&s.pool).await?;
    row.ok_or_else(missing)
}
async fn current(s: &Service, node: Uuid) -> Result<Value> {
    let shell: Option<Value>=sqlx::query_scalar("SELECT to_jsonb(h)-'actor_id' FROM worker_host_shells h WHERE node_id=$1 ORDER BY desired_running DESC,created_at_ms DESC,id DESC LIMIT 1")
        .bind(node).fetch_optional(&s.pool).await?;
    let ready:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM worker_host_receivers r JOIN nodes n ON n.id=r.node_id WHERE r.node_id=$1 AND r.seen_at_ms>$2 AND n.revoked_at IS NULL)")
        .bind(node).bind(now()-15000).fetch_one(&s.pool).await?;
    let name: Option<String> = sqlx::query_scalar("SELECT name FROM nodes WHERE id=$1")
        .bind(node)
        .fetch_optional(&s.pool)
        .await?;
    Ok(
        json!({"ready":ready,"node_name":name.ok_or_else(missing)?,"shell":shell,"server_time_ms":now()}),
    )
}
async fn inspect(
    State(s): State<Service>,
    headers: HeaderMap,
    Path(node): Path<Uuid>,
) -> Result<Json<Value>> {
    (s.owner)(headers).await?;
    Ok(Json(current(&s, node).await?))
}
async fn open(
    State(s): State<Service>,
    headers: HeaderMap,
    Path(node): Path<Uuid>,
    Json(op): Json<Operation>,
) -> Result<Json<Value>> {
    let actor = (s.owner)(headers).await?;
    valid_id(op.operation_id)?;
    let mut tx = s.pool.begin().await?;
    logins::live(&mut tx, node).await?;
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(node)
        .execute(&mut *tx)
        .await?;
    let ready: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM worker_host_receivers WHERE node_id=$1 AND seen_at_ms>$2)",
    )
    .bind(node)
    .bind(now() - 15000)
    .fetch_one(&mut *tx)
    .await?;
    if !ready {
        return Err(conflict(
            "Host terminal is unavailable; update this worker's controls",
        ));
    }
    let old: Option<(Uuid, Uuid, Uuid)> =
        sqlx::query_as("SELECT node_id,actor_id,shell_id FROM worker_host_operations WHERE id=$1")
            .bind(op.operation_id)
            .fetch_optional(&mut *tx)
            .await?;
    let id = if let Some((n, a, id)) = old {
        if (n, a) != (node, actor) {
            return Err(conflict("Operation identity is already used"));
        }
        id
    } else {
        let active: Option<Uuid> = sqlx::query_scalar(
            "SELECT id FROM worker_host_shells WHERE node_id=$1 AND desired_running",
        )
        .bind(node)
        .fetch_optional(&mut *tx)
        .await?;
        let id = active.unwrap_or(op.operation_id);
        if active.is_none() {
            sqlx::query("INSERT INTO worker_host_shells(id,node_id,actor_id,created_at_ms,updated_at_ms) VALUES($1,$2,$3,$4,$4)").bind(id).bind(node).bind(actor).bind(now()).execute(&mut *tx).await?;
        }
        sqlx::query(
            "INSERT INTO worker_host_operations(id,actor_id,node_id,shell_id) VALUES($1,$2,$3,$4)",
        )
        .bind(op.operation_id)
        .bind(actor)
        .bind(node)
        .bind(id)
        .execute(&mut *tx)
        .await?;
        id
    };
    tx.commit().await?;
    let mut response = current(&s, node).await?;
    response["shell"] = sqlx::query_scalar::<_, Value>(
        "SELECT to_jsonb(h)-'actor_id' FROM worker_host_shells h WHERE id=$1",
    )
    .bind(id)
    .fetch_one(&s.pool)
    .await?;
    Ok(Json(response))
}
async fn stop(
    State(s): State<Service>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(op): Json<Operation>,
) -> Result<Json<Value>> {
    (s.owner)(headers).await?;
    valid_id(op.operation_id)?;
    let node:Option<Uuid>=sqlx::query_scalar("UPDATE worker_host_shells SET desired_running=FALSE,updated_at_ms=$2 WHERE id=$1 RETURNING node_id").bind(id).bind(now()).fetch_optional(&s.pool).await?;
    Ok(Json(current(&s, node.ok_or_else(missing)?).await?))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Poll {
    id: Option<Uuid>,
    status: Option<String>,
}
async fn poll(
    State(s): State<Service>,
    headers: HeaderMap,
    Json(p): Json<Poll>,
) -> Result<Json<Value>> {
    let node = (s.node)(headers.clone()).await?;
    if p.id.is_some() != p.status.is_some()
        || p.status
            .as_ref()
            .is_some_and(|s| !matches!(s.as_str(), "running" | "ended"))
    {
        return Err(bad("Invalid host shell receipt"));
    }
    let mut tx = s.pool.begin().await?;
    logins::live(&mut tx, node).await?;
    if (s.node)(headers).await? != node {
        return Err(Error::enrolled_worker_required());
    }
    sqlx::query("INSERT INTO worker_host_receivers(node_id,seen_at_ms) VALUES($1,$2) ON CONFLICT(node_id) DO UPDATE SET seen_at_ms=$2").bind(node).bind(now()).execute(&mut *tx).await?;
    if let (Some(id), Some(status)) = (p.id, p.status) {
        let n=sqlx::query("UPDATE worker_host_shells SET status=$3,desired_running=desired_running AND $3<>'ended',worker_seen_at_ms=$4,updated_at_ms=$4 WHERE id=$1 AND node_id=$2")
            .bind(id).bind(node).bind(status).bind(now()).execute(&mut *tx).await?.rows_affected();
        if n != 1 {
            return Err(missing());
        }
    }
    let shells:Vec<Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',id,'desired_running',desired_running) FROM worker_host_shells WHERE node_id=$1 AND status<>'ended' ORDER BY created_at_ms LIMIT 8").bind(node).fetch_all(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"shells":shells})))
}
