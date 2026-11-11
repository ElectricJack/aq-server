//! Owner-requested subscription delivery. No material is persisted in SQL or owner responses.
use super::*;
use axum::{
    Router,
    extract::{Path, State},
    routing::{get, post},
};
use sqlx::Row;
pub type Materials = Arc<
    dyn Fn(HeaderMap, Vec<String>) -> Pin<Box<dyn Future<Output = Result<Value>> + Send>>
        + Send
        + Sync,
>;
pub(super) fn human_routes() -> Router<Service> {
    Router::new()
        .route("/api/worker-logins", get(list))
        .route("/api/worker-logins/{node}", post(request))
}
pub(super) fn worker_routes() -> Router<Service> {
    Router::new().route("/v1/worker/logins", post(poll))
}
async fn list(State(s): State<Service>, headers: HeaderMap) -> Result<Json<Value>> {
    (s.owner)(headers).await?;
    let rows: Vec<Value> = sqlx::query_scalar("SELECT to_jsonb(d)-'actor_id' FROM worker_login_deliveries d JOIN nodes n ON n.id=d.node_id WHERE n.revoked_at IS NULL").fetch_all(&s.pool).await?;
    Ok(Json(json!({"deliveries":rows,"server_time_ms":now()})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    operation_id: Uuid,
    providers: Vec<String>,
}
async fn live(conn: &mut sqlx::PgConnection, node: Uuid) -> Result<()> {
    let current: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM nodes WHERE id=$1 AND revoked_at IS NULL)")
            .bind(node)
            .fetch_one(&mut *conn)
            .await?;
    if !current
        || !aq_machine_control::enrolled(&mut *conn, node)
            .await
            .map_err(management::storage)?
        || aq_machine_control::management::capacity(conn, node)
            .await
            .map_err(management::storage)?
            .removed
    {
        return Err(Error::enrolled_worker_required());
    }
    Ok(())
}
async fn request(
    State(s): State<Service>,
    headers: HeaderMap,
    Path(node): Path<Uuid>,
    Json(mut req): Json<Request>,
) -> Result<Json<Value>> {
    let actor = (s.owner)(headers).await?;
    valid_id(req.operation_id)?;
    if req.providers.is_empty()
        || req.providers.len() > 2
        || req
            .providers
            .iter()
            .any(|p| !matches!(p.as_str(), "anthropic" | "openai"))
    {
        return Err(bad("Choose Claude or Codex"));
    }
    req.providers.sort();
    req.providers.dedup();
    let mut tx = s.pool.begin().await?;
    aq_machine_control::management::lock(&mut tx)
        .await
        .map_err(management::storage)?;
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(node)
        .fetch_optional(&mut *tx)
        .await?;
    live(&mut tx, node).await?;
    if let Some(row) = sqlx::query(
        "SELECT node_id,actor_id,providers FROM worker_login_operations WHERE operation_id=$1",
    )
    .bind(req.operation_id)
    .fetch_optional(&mut *tx)
    .await?
    {
        if row.get::<Uuid, _>("node_id") != node
            || row.get::<Uuid, _>("actor_id") != actor
            || row.get::<Vec<String>, _>("providers") != req.providers
        {
            return Err(conflict(
                "Operation was used for different providers or worker",
            ));
        }
        return Ok(Json(json!({"generation":req.operation_id})));
    }
    sqlx::query("INSERT INTO worker_login_operations(operation_id,node_id,actor_id,providers) VALUES($1,$2,$3,$4)").bind(req.operation_id).bind(node).bind(actor).bind(&req.providers).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO worker_login_deliveries(node_id,generation,providers,status,requested_at_ms,actor_id) VALUES($1,$2,$3,'pending',$4,$5) ON CONFLICT(node_id) DO UPDATE SET generation=$2,providers=$3,status='pending',requested_at_ms=$4,applied_at_ms=NULL,actor_id=$5")
        .bind(node).bind(req.operation_id).bind(req.providers).bind(now()).bind(actor).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"generation":req.operation_id})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Poll {
    generation: Option<Uuid>,
    applied: Option<bool>,
}
async fn poll(
    State(s): State<Service>,
    headers: HeaderMap,
    Json(req): Json<Poll>,
) -> Result<Response> {
    let node = (s.node)(headers.clone()).await?;
    let mut tx = s.pool.begin().await?;
    live(&mut tx, node).await?;
    sqlx::query("INSERT INTO worker_login_deliveries(node_id,receiver_seen_at_ms) VALUES($1,$2) ON CONFLICT(node_id) DO UPDATE SET receiver_seen_at_ms=$2").bind(node).bind(now()).execute(&mut *tx).await?;
    if let (Some(generation), Some(applied)) = (req.generation, req.applied) {
        sqlx::query("UPDATE worker_login_deliveries SET status=$3,applied_at_ms=CASE WHEN $3='applied' THEN $4 ELSE NULL END WHERE node_id=$1 AND generation=$2 AND status IN ('pending','failed')")
            .bind(node).bind(generation).bind(if applied {"applied"} else {"failed"}).bind(now()).execute(&mut *tx).await?;
    }
    let row = sqlx::query(
        "SELECT generation,providers,status FROM worker_login_deliveries WHERE node_id=$1",
    )
    .bind(node)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    let mut body = json!({"generation":null,"logins":[]});
    if row.get::<String, _>("status") == "pending" && req.applied.is_none() {
        let material = s
            .login_materials
            .as_ref()
            .ok_or_else(|| conflict("Server logins are unavailable"))?;
        let logins = match material(headers, row.get("providers")).await {
            Ok(logins) => logins,
            Err(error) => {
                if error.0 != StatusCode::CONFLICT {
                    sqlx::query("UPDATE worker_login_deliveries SET status='failed' WHERE node_id=$1 AND generation=$2")
                        .bind(node).bind(row.get::<Option<Uuid>,_>("generation")).execute(&s.pool).await?;
                }
                return Err(error);
            }
        };
        body = json!({"generation":row.get::<Option<Uuid>,_>("generation"),"logins":logins});
    }
    Ok(([("cache-control", "no-store")], Json(body)).into_response())
}
