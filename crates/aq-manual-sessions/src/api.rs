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
        .route("/api/manual-agents/{id}", get(detail))
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
async fn detail(
    State(s): State<Service>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<Session>> {
    (s.owner)(headers).await?;
    Ok(Json(session(&s, id).await?))
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
    if !assigned(&b["allowed_node_ids"], node) {
        return Err(conflict("This worker is not assigned to the project"));
    }
    if b["allowed_worker_groups"]
        .as_array()
        .is_some_and(|v| !v.is_empty())
    {
        return Err(conflict("Manual worker-group placement is not configured"));
    }
    // A launch pins its level and candidate: the level's id is the admission
    // class, and the snapshot digest says which documents it was read from.
    let choice = match selected {
        Some(selected) => intelligence::resolve(s, selected).await?,
        None => {
            let level = b["default_intelligence"]
                .as_str()
                .unwrap_or(aq_project_policy::DEFAULT_INTELLIGENCE);
            intelligence::first_of(s, level).await?
        }
    };
    Ok(
        json!({"project_name":b["name"],"repository_id":b["repository_id"],"source":source,"provider":choice["provider"],"model":choice["model"],"reasoning":choice["reasoning"],"harness":choice["harness"],"intelligence_profile":{"profile_id":choice["profile_id"],"candidate_id":choice["candidate_id"]},"profile_name":choice["name"],"snapshot_digest":choice["snapshot_digest"],"intelligence_class":choice["profile_id"]}),
    )
}
fn assigned(ids: &Value, node: Uuid) -> bool {
    let node = node.to_string();
    ids.as_array()
        .is_none_or(|ids| ids.is_empty() || ids.iter().any(|id| id.as_str() == Some(node.as_str())))
}
async fn list(
    State(s): State<Service>,
    headers: HeaderMap,
    Query(q): Query<Scope>,
) -> Result<Json<Value>> {
    (s.owner)(headers).await?;
    let rows:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(m)-'actor_id'-'stopped_at_ms' FROM manual_agent_sessions m WHERE ($1::uuid IS NULL OR project_id=$1) AND (stopped_at_ms IS NULL OR stopped_at_ms>$2) ORDER BY created_at_ms DESC,id LIMIT 256").bind(q.project_id).bind(now()-86400000).fetch_all(&s.pool).await?;
    let reports:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(r) FROM manual_worker_reports r JOIN nodes n ON n.id=r.node_id WHERE n.revoked_at IS NULL").fetch_all(&s.pool).await?;
    let workers = available_workers(&s.pool, reports).await?;
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
    let profiles = intelligence::choices(&s.pool).await?;
    Ok(Json(
        json!({"launch_profiles":profiles,"sessions":rows,"workers":workers,"slots":slots,"projects":projects,"server_time_ms":now()}),
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Launch {
    operation_id: Uuid,
    #[serde(default)]
    intelligence_profile: Option<intelligence::Selection>,
}
async fn available_workers(pool: &PgPool, reports: Vec<Value>) -> Result<Vec<Value>> {
    let mut workers = Vec::new();
    let mut conn = pool.acquire().await?;
    for mut report in reports {
        let node = serde_json::from_value(report["node_id"].clone())
            .map_err(|_| bad("Invalid worker report"))?;
        let limits = aq_machine_control::management::capacity(&mut conn, node)
            .await
            .map_err(management::storage)?;
        if !limits.removed {
            report["max_slots"] =
                json!(limits.maximum(report["max_slots"].as_u64().unwrap_or(1) as u32));
            workers.push(report);
        }
    }
    Ok(workers)
}
fn same_launch(
    old: &Session,
    node: Uuid,
    project: Uuid,
    selected: &Option<intelligence::Selection>,
) -> bool {
    old.node_id == node
        && old.project_id == project
        && old.spec.get("requested_intelligence_profile").or_else(|| {
            old.spec
                .get("intelligence_profile")
                .filter(|_| old.spec.get("intelligence_class").is_none())
        }) == selected
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
    let mut spec = spec(&s, project, node, op.intelligence_profile.as_ref()).await?;
    if let Some(selected) = &op.intelligence_profile {
        spec["requested_intelligence_profile"] =
            serde_json::to_value(selected).map_err(|_| bad("Invalid profile choice"))?;
    }
    let mut tx = s.pool.begin().await?;
    reserve_launch(&mut tx, node, &spec).await?;
    sqlx::query("INSERT INTO manual_agent_sessions(id,actor_id,node_id,project_id,spec,status,stage,created_at_ms,updated_at_ms) VALUES($1,$2,$3,$4,$5,'pending','Waiting for a slot',$6,$6) ON CONFLICT(id) DO NOTHING").bind(op.operation_id).bind(actor).bind(node).bind(project).bind(&spec).bind(now()).execute(&mut *tx).await?;
    tx.commit().await?;
    let saved = session(&s, op.operation_id).await?;
    if !same_launch(&saved, node, project, &op.intelligence_profile) {
        return Err(conflict("Launch identity is already used"));
    }
    Ok(Json(saved))
}
async fn reserve_launch(conn: &mut sqlx::PgConnection, node: Uuid, spec: &Value) -> Result<()> {
    aq_machine_control::allocation::lock(&mut *conn, node)
        .await
        .map_err(management::storage)?;
    sqlx::query("SELECT pg_advisory_xact_lock(107107)")
        .execute(&mut *conn)
        .await?;
    let live: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM manual_agent_sessions WHERE stopped_at_ms IS NULL",
    )
    .fetch_one(&mut *conn)
    .await?;
    if live >= 256 {
        return Err(conflict(
            "There are already 256 pending or running manual agents",
        ));
    }
    let available:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM nodes n JOIN manual_worker_reports r ON r.node_id=n.id WHERE n.id=$1 AND n.revoked_at IS NULL AND r.observed_at_ms>$2 AND r.providers @> $3::jsonb)").bind(node).bind(now()-15000).bind(json!([spec["provider"]])).fetch_one(&mut *conn).await?;
    if !available {
        return Err(conflict(
            "This worker has not reported interactive support for the provider",
        ));
    }
    if aq_machine_control::management::capacity(&mut *conn, node)
        .await
        .map_err(management::storage)?
        .removed
    {
        return Err(conflict("This worker has been removed"));
    }
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,107))")
        .bind(node.to_string())
        .execute(&mut *conn)
        .await?;
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM manual_agent_sessions WHERE node_id=$1 AND stopped_at_ms IS NULL",
    )
    .bind(node)
    .fetch_one(&mut *conn)
    .await?;
    if count >= 32 {
        return Err(conflict(
            "This worker already has 32 pending or running sessions",
        ));
    }
    Ok(())
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
    if v.slots > 1024 || v.expected_revision > i64::MAX as u64 {
        return Err(bad("Slots must be between zero and 1024"));
    }
    binding(&s, project).await?;
    let request = json!({"node":node,"project":project,"body":v});
    let mut tx = s.pool.begin().await?;
    aq_machine_control::allocation::lock(&mut tx, node)
        .await
        .map_err(management::storage)?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,107))")
        .bind(node.to_string())
        .execute(&mut *tx)
        .await?;
    if let Some(receipt) = v.prior(&mut tx, actor, &request).await? {
        return Ok(Json(receipt));
    }
    let max:Option<i32>=sqlx::query_scalar("SELECT max_slots FROM manual_worker_reports r JOIN nodes n ON n.id=r.node_id WHERE r.node_id=$1 AND n.revoked_at IS NULL AND r.observed_at_ms>$2").bind(node).bind(now()-15000).fetch_optional(&mut *tx).await?;
    let limits = aq_machine_control::management::capacity(&mut tx, node)
        .await
        .map_err(management::storage)?;
    if limits.removed || max.is_none_or(|max| v.slots > limits.maximum(max as u32)) {
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

impl Slots {
    async fn prior(
        &self,
        conn: &mut sqlx::PgConnection,
        actor: Uuid,
        request: &Value,
    ) -> Result<Option<Value>> {
        if let Some(row) = sqlx::query(
            "SELECT actor_id,request,receipt FROM manual_agent_operations WHERE operation_id=$1",
        )
        .bind(self.operation_id)
        .fetch_optional(&mut *conn)
        .await?
        {
            if row.get::<Uuid, _>("actor_id") != actor || row.get::<Value, _>("request") != *request
            {
                return Err(conflict("Slot operation identity is already used"));
            }
            return Ok(Some(row.get("receipt")));
        }
        Ok(None)
    }
}
