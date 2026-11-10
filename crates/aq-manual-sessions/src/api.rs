use super::*;
use axum::{
    Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    routing::{get, post},
};
use sqlx::Row;
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Scope {
    project_id: Option<Uuid>,
}
pub(super) fn routes() -> Router<Service> {
    Router::new()
        .route("/api/manual-agents", get(list))
        .route(
            "/api/manual-agents/workers/{node}/projects/{project}",
            post(start),
        )
        .route(
            "/api/manual-agents/workers/{node}/projects/{project}/slots",
            post(slots),
        )
        .route("/api/manual-agents/{id}/stop", post(stop))
        .merge(terminal::routes())
        .layer(DefaultBodyLimit::max(16384))
}
async fn binding(s: &Service, id: Uuid) -> Result<Value> {
    let b: Option<Value> =
        sqlx::query_scalar("SELECT binding FROM project_execution_bindings WHERE project_id=$1")
            .bind(id)
            .fetch_optional(&s.pool)
            .await?;
    b.filter(|v| v["enabled"] == true)
        .ok_or_else(|| conflict("Start this project before launching an agent"))
}
async fn spec(
    s: &Service,
    project: Uuid,
    node: Uuid,
    selected: Option<&intelligence::Selection>,
) -> Result<Value> {
    let source = s
        .sources
        .get(&project)
        .ok_or_else(|| conflict("Worker source setup is incomplete"))?;
    let b = binding(s, project).await?;
    if let Some(ids) = b["allowed_node_ids"].as_array()
        && !ids.is_empty()
        && !ids.iter().any(|id| id.as_str() == Some(&node.to_string()))
    {
        return Err(conflict("This worker is not assigned to the project"));
    }
    if b["allowed_worker_groups"]
        .as_array()
        .is_some_and(|v| !v.is_empty())
    {
        return Err(conflict("Manual worker-group placement is not configured"));
    }
    if let Some(selected) = selected {
        let choice = intelligence::resolve(s, selected).await?;
        return Ok(
            json!({"project_name":b["name"],"repository_id":b["repository_id"],"source":source,"provider":choice["provider"],"model":choice["model"],"reasoning":choice["reasoning"],"reasoning_configuration_reference":choice["reasoning_configuration_reference"],"options":choice["options"],"harness":choice["harness"],"intelligence_profile":selected,"profile_name":choice["name"]}),
        );
    }
    let artifact: Vec<u8> =
        sqlx::query_scalar("SELECT artifact FROM profile_catalog_artifacts WHERE digest=$1")
            .bind(
                b["catalog_digest"]
                    .as_str()
                    .ok_or_else(|| bad("Project catalog is unavailable"))?,
            )
            .fetch_one(&s.pool)
            .await?;
    let cat: Value =
        serde_json::from_slice(&artifact).map_err(|_| bad("Project catalog is invalid"))?;
    let allowed = b["allowed_intelligence_profiles"]
        .as_array()
        .ok_or_else(|| bad("Project models are unavailable"))?;
    let profiles = cat["catalog"]["intelligence"]
        .as_array()
        .ok_or_else(|| bad("Project catalog is invalid"))?;
    let model = allowed
        .iter()
        .filter_map(Value::as_str)
        .find_map(|id| {
            profiles
                .iter()
                .find(|p| format!("{}@{}", p["id"].as_str().unwrap_or(""), p["version"]) == id)
                .and_then(|p| p["candidates"].as_array())
                .and_then(|cs| cs.iter().find(|c| c["provider"] == "anthropic"))
        })
        .ok_or_else(|| {
            conflict("No installed interactive provider is configured for this project")
        })?;
    Ok(
        json!({"project_name":b["name"],"repository_id":b["repository_id"],"source":source,"provider":model["provider"],"model":model["model"]}),
    )
}
async fn list(
    State(s): State<Service>,
    headers: HeaderMap,
    Query(q): Query<Scope>,
) -> Result<Json<Value>> {
    (s.owner)(headers).await?;
    let rows:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(m)-'actor_id'-'stopped_at_ms' FROM manual_agent_sessions m WHERE ($1::uuid IS NULL OR project_id=$1) AND (stopped_at_ms IS NULL OR stopped_at_ms>$2) ORDER BY created_at_ms DESC,id LIMIT 256").bind(q.project_id).bind(now()-86400000).fetch_all(&s.pool).await?;
    let reports:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(r) FROM manual_worker_reports r JOIN nodes n ON n.id=r.node_id WHERE n.revoked_at IS NULL").fetch_all(&s.pool).await?;
    let slots: Vec<Value> = sqlx::query_scalar(
        "SELECT to_jsonb(p) FROM manual_project_slots p WHERE $1::uuid IS NULL OR project_id=$1",
    )
    .bind(q.project_id)
    .fetch_all(&s.pool)
    .await?;
    let mut projects = Vec::new();
    for id in s.sources.keys() {
        if let Ok(b) = binding(&s, *id).await {
            projects.push(json!({"id":id,"name":b["name"],"repository_id":b["repository_id"]}));
        }
    }
    let profiles = intelligence::choices(&s).await?;
    Ok(Json(
        json!({"launch_profiles":profiles,"sessions":rows,"workers":reports,"slots":slots,"projects":projects,"server_time_ms":now()}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Launch {
    operation_id: Uuid,
    #[serde(default)]
    intelligence_profile: Option<intelligence::Selection>,
}
fn same_launch(
    old: &Session,
    node: Uuid,
    project: Uuid,
    selected: &Option<intelligence::Selection>,
) -> bool {
    old.node_id == node
        && old.project_id == project
        && old.spec.get("intelligence_profile")
            == selected
                .as_ref()
                .map(serde_json::to_value)
                .transpose()
                .ok()
                .flatten()
                .as_ref()
}
async fn start(
    State(s): State<Service>,
    headers: HeaderMap,
    Path((node, project)): Path<(Uuid, Uuid)>,
    Json(op): Json<Launch>,
) -> Result<Json<Session>> {
    let actor = (s.owner)(headers).await?;
    valid_id(op.operation_id)?;
    if let Ok(old) = session(&s, op.operation_id).await {
        if same_launch(&old, node, project, &op.intelligence_profile) {
            return Ok(Json(old));
        }
        return Err(conflict("Launch identity is already used"));
    }
    let spec = spec(&s, project, node, op.intelligence_profile.as_ref()).await?;
    let mut tx = s.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(107107)")
        .execute(&mut *tx)
        .await?;
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM manual_agent_sessions WHERE stopped_at_ms IS NULL",
    )
    .fetch_one(&mut *tx)
    .await?;
    if live >= 256 {
        return Err(conflict(
            "There are already 256 pending or running manual agents",
        ));
    }
    let available:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM nodes n JOIN manual_worker_reports r ON r.node_id=n.id WHERE n.id=$1 AND n.revoked_at IS NULL AND r.observed_at_ms>$2 AND r.repositories @> $3::jsonb AND r.providers @> $4::jsonb)").bind(node).bind(now()-15000).bind(json!([spec["repository_id"]])).bind(json!([spec["provider"]])).fetch_one(&mut *tx).await?;
    if !available {
        return Err(conflict(
            "This worker has not reported interactive support for the project",
        ));
    }
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,107))")
        .bind(node.to_string())
        .execute(&mut *tx)
        .await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM manual_agent_sessions WHERE node_id=$1 AND stopped_at_ms IS NULL",
    )
    .bind(node)
    .fetch_one(&mut *tx)
    .await?;
    if count >= 32 {
        return Err(conflict(
            "This worker already has 32 pending or running sessions",
        ));
    }
    sqlx::query("INSERT INTO manual_agent_sessions(id,actor_id,node_id,project_id,spec,status,stage,created_at_ms,updated_at_ms) VALUES($1,$2,$3,$4,$5,'pending','Waiting for a slot',$6,$6) ON CONFLICT(id) DO NOTHING").bind(op.operation_id).bind(actor).bind(node).bind(project).bind(&spec).bind(now()).execute(&mut *tx).await?;
    tx.commit().await?;
    let saved = session(&s, op.operation_id).await?;
    if !same_launch(&saved, node, project, &op.intelligence_profile) {
        return Err(conflict("Launch identity is already used"));
    }
    Ok(Json(saved))
}
async fn stop(
    State(s): State<Service>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(op): Json<Operation>,
) -> Result<Json<Session>> {
    (s.owner)(headers).await?;
    valid_id(op.operation_id)?;
    sqlx::query("UPDATE manual_agent_sessions SET desired_running=false,status=CASE WHEN stopped_at_ms IS NULL THEN 'stopping' ELSE status END,stage=CASE WHEN stopped_at_ms IS NULL THEN 'Stopping; preserving workspace' ELSE stage END,updated_at_ms=$2 WHERE id=$1").bind(id).bind(now()).execute(&s.pool).await?;
    Ok(Json(session(&s, id).await?))
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Slots {
    operation_id: Uuid,
    expected_revision: u64,
    slots: u32,
}
async fn slots(
    State(s): State<Service>,
    headers: HeaderMap,
    Path((node, project)): Path<(Uuid, Uuid)>,
    Json(v): Json<Slots>,
) -> Result<Json<Value>> {
    let actor = (s.owner)(headers).await?;
    valid_id(v.operation_id)?;
    if v.slots > 8 || v.expected_revision > i64::MAX as u64 {
        return Err(bad("Slots must be between zero and eight"));
    }
    binding(&s, project).await?;
    let request = json!({"node":node,"project":project,"body":v});
    let mut tx = s.pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,107))")
        .bind(node.to_string())
        .execute(&mut *tx)
        .await?;
    if let Some(row) = sqlx::query(
        "SELECT actor_id,request,receipt FROM manual_agent_operations WHERE operation_id=$1",
    )
    .bind(v.operation_id)
    .fetch_optional(&mut *tx)
    .await?
    {
        if row.get::<Uuid, _>("actor_id") != actor || row.get::<Value, _>("request") != request {
            return Err(conflict("Slot operation identity is already used"));
        }
        return Ok(Json(row.get("receipt")));
    }
    let max:Option<i32>=sqlx::query_scalar("SELECT max_slots FROM manual_worker_reports r JOIN nodes n ON n.id=r.node_id WHERE r.node_id=$1 AND n.revoked_at IS NULL AND r.observed_at_ms>$2").bind(node).bind(now()-15000).fetch_optional(&mut *tx).await?;
    if max.is_none_or(|max| v.slots > max as u32) {
        return Err(conflict("The worker cannot provide that many slots"));
    }
    let current:Option<(i32,i64)>=sqlx::query_as("SELECT slots,revision FROM manual_project_slots WHERE node_id=$1 AND project_id=$2 FOR UPDATE").bind(node).bind(project).fetch_optional(&mut *tx).await?;
    if current.map_or(0, |c| c.1) != v.expected_revision as i64 {
        return Err(conflict(
            "Slot configuration changed; refresh before retrying",
        ));
    }
    let revision = v.expected_revision + 1;
    sqlx::query("INSERT INTO manual_project_slots(node_id,project_id,slots,revision) VALUES($1,$2,$3,$4) ON CONFLICT(node_id,project_id) DO UPDATE SET slots=EXCLUDED.slots,revision=EXCLUDED.revision").bind(node).bind(project).bind(v.slots as i32).bind(revision as i64).execute(&mut *tx).await?;
    let receipt = json!({"operation_id":v.operation_id,"node_id":node,"project_id":project,"slots":v.slots,"revision":revision});
    sqlx::query("INSERT INTO manual_agent_operations(operation_id,actor_id,request,receipt) VALUES($1,$2,$3,$4)").bind(v.operation_id).bind(actor).bind(request).bind(&receipt).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Json(receipt))
}
