use super::*;
use aq_machine_control::management as control;
use axum::{
    Router,
    extract::{Path, State},
    routing::{get, post},
};
use sqlx::Row;

pub(super) fn routes() -> Router<Service> {
    Router::new()
        .route("/api/worker-management", get(list))
        .route("/api/worker-management/{kind}/{id}", post(save))
}

async fn list(State(s): State<Service>, headers: HeaderMap) -> Result<Json<Value>> {
    (s.owner)(headers).await?;
    let mut conn = s.pool.acquire().await?;
    let settings = control::settings(&mut conn).await.map_err(storage)?;
    Ok(Json(json!({"settings": settings})))
}

pub(super) fn storage(error: aq_machine_control::Error) -> Error {
    match error {
        aq_machine_control::Error::NotFound => missing(),
        _ => Error(
            StatusCode::SERVICE_UNAVAILABLE,
            "Worker settings are temporarily unavailable",
        ),
    }
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Change {
    operation_id: Uuid,
    expected_revision: u64,
    label: Option<String>,
    max_agents: Option<u32>,
    #[serde(default)]
    remove: bool,
}

async fn save(
    State(s): State<Service>,
    headers: HeaderMap,
    Path((kind, id)): Path<(String, String)>,
    Json(mut change): Json<Change>,
) -> Result<Json<Value>> {
    let actor = (s.owner)(headers).await?;
    valid_id(change.operation_id)?;
    let valid_id = if kind == "node" {
        id.parse::<Uuid>().is_ok()
    } else {
        kind == "machine"
            && (aq_machine_control::valid_machine_key(&id)
                || id
                    .strip_prefix("node:")
                    .is_some_and(|v| v.parse::<Uuid>().is_ok()))
    };
    if !valid_id || change.expected_revision >= i64::MAX as u64 {
        return Err(bad("Invalid worker identity or revision"));
    }
    change.label = change
        .label
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty());
    if change
        .label
        .as_ref()
        .is_some_and(|v| v.chars().count() > 64 || v.chars().any(char::is_control))
    {
        return Err(bad("Names must be at most 64 characters"));
    }
    let maximum = if kind == "node" { 8 } else { i32::MAX as u32 };
    if change.max_agents.is_some_and(|v| v > maximum) {
        return Err(bad("Agent maximum is outside the supported range"));
    }
    let request = json!({"kind":kind, "id":id, "change":change});
    let mut tx = s.pool.begin().await?;
    control::lock(&mut tx).await.map_err(storage)?;
    if let Some(row) = sqlx::query(
        "SELECT actor_id,request,receipt FROM worker_setting_operations WHERE operation_id=$1",
    )
    .bind(change.operation_id)
    .fetch_optional(&mut *tx)
    .await?
    {
        if row.get::<Uuid, _>("actor_id") != actor || row.get::<Value, _>("request") != request {
            return Err(conflict(
                "This operation was already used for different settings",
            ));
        }
        return Ok(Json(row.get("receipt")));
    }
    let nodes: Vec<Uuid> = sqlx::query_scalar(
        "SELECT node_id FROM worker_machine_members WHERE ($1='node' AND node_id::text=$2) OR ($1='machine' AND machine_id=$2) ORDER BY node_id"
    ).bind(&kind).bind(&id).fetch_all(&mut *tx).await?;
    if nodes.is_empty() {
        return Err(missing());
    }
    sqlx::query("SELECT id FROM nodes WHERE id=ANY($1) ORDER BY id FOR UPDATE")
        .bind(&nodes)
        .fetch_all(&mut *tx)
        .await?;
    sqlx::query("UPDATE nodes SET machine_key=machine_key WHERE id=ANY($1)")
        .bind(&nodes)
        .execute(&mut *tx)
        .await?;
    let current: Option<i64> =
        sqlx::query_scalar("SELECT revision FROM worker_settings WHERE kind=$1 AND id=$2")
            .bind(&kind)
            .bind(&id)
            .fetch_optional(&mut *tx)
            .await?;
    if current.unwrap_or(0) as u64 != change.expected_revision {
        return Err(conflict("Settings changed. Refresh before saving again."));
    }
    let retired = kind == "node" && change.remove;
    let revision = change.expected_revision + 1;
    sqlx::query(
        "INSERT INTO worker_settings(kind,id,label,max_agents,removed,revision) VALUES($1,$2,$3,$4,$5,$6) \
         ON CONFLICT(kind,id) DO UPDATE SET label=EXCLUDED.label,max_agents=EXCLUDED.max_agents,removed=EXCLUDED.removed,revision=EXCLUDED.revision"
    ).bind(&kind).bind(&id).bind(&change.label).bind(change.max_agents.map(|v| v as i32))
        .bind(retired).bind(revision as i64).execute(&mut *tx).await?;
    if kind == "node"
        && let Some(label) = &change.label
    {
        sqlx::query("UPDATE nodes SET name=$2 WHERE id=ANY($1)")
            .bind(&nodes)
            .bind(label)
            .execute(&mut *tx)
            .await?;
    }
    if change.remove {
        // Retiring one execution node keeps the machine's control connection usable.
        sqlx::query(
            "UPDATE manual_agent_sessions SET desired_running=FALSE, \
             status=CASE WHEN status='pending' THEN 'stopped' ELSE 'stopping' END, \
             stage='Worker removed',updated_at_ms=$2, \
             stopped_at_ms=CASE WHEN status='pending' THEN $2 ELSE stopped_at_ms END \
             WHERE node_id=ANY($1) AND stopped_at_ms IS NULL",
        )
        .bind(&nodes)
        .bind(now())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "INSERT INTO attempt_handoffs(attempt_id,node_id,reason,requested_at) \
             SELECT attempt_id,node_id,'owner_stop',now() FROM execution_attempts \
             WHERE node_id=ANY($1) AND holds_task AND kind='profiled_v1' ON CONFLICT DO NOTHING",
        )
        .bind(&nodes)
        .execute(&mut *tx)
        .await?;
        if kind == "machine" {
            sqlx::query("UPDATE nodes SET revoked_at=COALESCE(revoked_at,now()) WHERE id=ANY($1)")
                .bind(&nodes)
                .execute(&mut *tx)
                .await?;
            sqlx::query("DELETE FROM machine_controls WHERE machine_key=$1")
                .bind(&id)
                .execute(&mut *tx)
                .await?;
        }
    }
    let receipt = json!({"kind":kind,"id":id,"label":change.label,"max_agents":change.max_agents,
        "removed":retired,"revision":revision});
    sqlx::query("INSERT INTO worker_setting_operations(operation_id,actor_id,request,receipt) VALUES($1,$2,$3,$4)")
        .bind(change.operation_id).bind(actor).bind(request).bind(&receipt).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(receipt))
}
