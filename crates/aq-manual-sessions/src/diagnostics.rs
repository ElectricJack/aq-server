//! Fixed remote checks and pinned receiver updates, requested by the owner.
use super::*;
use axum::{
    Router,
    extract::{DefaultBodyLimit, Path, State},
    routing::{get, post},
};
use sqlx::Row;
pub(super) fn human_routes() -> Router<Service> {
    Router::new()
        .route("/api/worker-diagnostics", get(list))
        .route("/api/worker-diagnostics/{node}", post(request))
        .layer(DefaultBodyLimit::max(4096))
}
pub(super) fn worker_routes() -> Router<Service> {
    Router::new()
        .route("/v1/worker/diagnostics", post(poll))
        .layer(DefaultBodyLimit::max(24576))
}
async fn list(State(s): State<Service>, headers: HeaderMap) -> Result<Json<Value>> {
    (s.owner)(headers).await?;
    let jobs:Vec<Value>=sqlx::query_scalar("SELECT DISTINCT ON (j.node_id) to_jsonb(j)-'actor_id' FROM worker_diagnostic_jobs j JOIN nodes n ON n.id=j.node_id WHERE n.revoked_at IS NULL ORDER BY j.node_id,j.created_at_ms DESC,j.id DESC").fetch_all(&s.pool).await?;
    let receivers:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(r) FROM worker_diagnostic_receivers r JOIN nodes n ON n.id=r.node_id WHERE n.revoked_at IS NULL").fetch_all(&s.pool).await?;
    Ok(Json(
        json!({"jobs":jobs,"receivers":receivers,"server_time_ms":now()}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    operation_id: Uuid,
    #[serde(default = "default_kind")]
    kind: String,
    installer_sha256: Option<String>,
}
fn default_kind() -> String {
    "sandbox_startup_v1".into()
}
async fn request(
    State(s): State<Service>,
    headers: HeaderMap,
    Path(node): Path<Uuid>,
    Json(req): Json<Request>,
) -> Result<Json<Value>> {
    let actor = (s.owner)(headers).await?;
    valid_id(req.operation_id)?;
    if !matches!(
        req.kind.as_str(),
        "sandbox_startup_v1" | "receiver_update_v1"
    ) || (req.kind == "receiver_update_v1") != req.installer_sha256.is_some()
        || req.installer_sha256.as_ref().is_some_and(|s| {
            s.len() != 64
                || !s
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    {
        return Err(bad(
            "Choose a supported worker check or a pinned receiver update",
        ));
    }
    let mut tx = s.pool.begin().await?;
    aq_machine_control::management::lock(&mut tx)
        .await
        .map_err(management::storage)?;
    super::logins::live(&mut tx, node).await?;
    if let Some(row) = sqlx::query(
        "SELECT actor_id,node_id,kind,installer_sha256 FROM worker_diagnostic_jobs WHERE id=$1",
    )
    .bind(req.operation_id)
    .fetch_optional(&mut *tx)
    .await?
    {
        if row.get::<Uuid, _>("actor_id") != actor
            || row.get::<Uuid, _>("node_id") != node
            || row.get::<String, _>("kind") != req.kind
            || row.get::<Option<String>, _>("installer_sha256") != req.installer_sha256
        {
            return Err(conflict(
                "Operation was used for a different worker request",
            ));
        }
        return Ok(Json(json!({"operation_id":req.operation_id})));
    }
    let active:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM worker_diagnostic_jobs WHERE node_id=$1 AND status IN ('queued','running'))").bind(node).fetch_one(&mut *tx).await?;
    if active {
        return Err(conflict("A worker check is already queued or running"));
    }
    sqlx::query("INSERT INTO worker_diagnostic_jobs(id,node_id,actor_id,kind,installer_sha256,status,created_at_ms,updated_at_ms) VALUES($1,$2,$3,$4,$5,'queued',$6,$6)").bind(req.operation_id).bind(node).bind(actor).bind(req.kind).bind(req.installer_sha256).bind(now()).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(json!({"operation_id":req.operation_id})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Poll {
    #[serde(default = "version")]
    version: i32,
    operation_id: Option<Uuid>,
    report: Option<Value>,
}
fn version() -> i32 {
    1
}
fn valid_report(report: &Value) -> bool {
    report.as_object().is_some_and(|v| {
        v.len() <= 4
            && v.keys()
                .all(|k| matches!(k.as_str(), "ok" | "phase" | "error" | "details"))
            && v.get("ok").is_some_and(Value::is_boolean)
            && v.get("phase")
                .is_none_or(|v| v.as_str().is_some_and(|s| s.len() <= 128))
            && v.get("error")
                .is_none_or(|v| v.as_str().is_some_and(|s| s.len() <= 4096))
            && v.get("details").is_none_or(Value::is_object)
    }) && serde_json::to_vec(report).is_ok_and(|v| v.len() <= 16384)
}

async fn poll(
    State(s): State<Service>,
    headers: HeaderMap,
    Json(req): Json<Poll>,
) -> Result<Json<Value>> {
    let node = (s.node)(headers.clone()).await?;
    if req.version != 1
        || req.operation_id.is_some() != req.report.is_some()
        || req.report.as_ref().is_some_and(|v| !valid_report(v))
    {
        return Err(bad("Invalid worker diagnostic receipt"));
    }
    let mut tx = s.pool.begin().await?;
    super::logins::live(&mut tx, node).await?;
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(node)
        .fetch_optional(&mut *tx)
        .await?;
    if (s.node)(headers).await? != node {
        return Err(Error::enrolled_worker_required());
    }
    sqlx::query("INSERT INTO worker_diagnostic_receivers(node_id,seen_at_ms,version) VALUES($1,$2,$3) ON CONFLICT(node_id) DO UPDATE SET seen_at_ms=$2,version=$3").bind(node).bind(now()).bind(req.version).execute(&mut *tx).await?;
    if let (Some(id), Some(report)) = (req.operation_id, req.report) {
        let existing: Option<Value> = sqlx::query_scalar(
            "SELECT result FROM worker_diagnostic_jobs WHERE id=$1 AND node_id=$2",
        )
        .bind(id)
        .bind(node)
        .fetch_optional(&mut *tx)
        .await?
        .flatten();
        if existing.as_ref().is_some_and(|v| v != &report) {
            return Err(conflict("Diagnostic receipt changed"));
        }
        let changed=sqlx::query("UPDATE worker_diagnostic_jobs SET status='completed',result=$3,updated_at_ms=$4 WHERE id=$1 AND node_id=$2").bind(id).bind(node).bind(report).bind(now()).execute(&mut *tx).await?;
        if changed.rows_affected() != 1 {
            return Err(missing());
        }
        tx.commit().await?;
        return Ok(Json(json!({"operation_id":null})));
    }
    let job:Option<Value>=sqlx::query_scalar("SELECT jsonb_build_object('operation_id',id,'kind',kind,'installer_sha256',installer_sha256) FROM worker_diagnostic_jobs WHERE node_id=$1 AND status IN ('queued','running') ORDER BY created_at_ms LIMIT 1 FOR UPDATE").bind(node).fetch_optional(&mut *tx).await?;
    if let Some(ref job) = job {
        let id: Uuid = serde_json::from_value(job["operation_id"].clone())
            .map_err(|_| bad("Invalid retained worker check"))?;
        sqlx::query("UPDATE worker_diagnostic_jobs SET status='running',updated_at_ms=$2 WHERE id=$1 AND status='queued'").bind(id).bind(now()).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(Json(job.unwrap_or_else(|| json!({"operation_id":null}))))
}
