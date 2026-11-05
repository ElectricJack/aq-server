//! Work-view task projections: readiness, blockers, parent, attached execution,
//! history timeline and per-scope completion counts (G11 X8-X13).
//!
//! Readiness is derived server-side from one bounded depth-1 Graph Context and
//! the attempt holding the task. It never claims `ready` when the Context was
//! truncated without a known open blocker: that is reported as `unknown`.
use super::*;

const HISTORY_LIMIT: u16 = 25;
const ATTEMPT_LIMIT: i64 = 16;

/// Holding attempt (the agent session attached to each task) for a page of
/// tasks in this project. At most one attempt holds a task.
pub(super) async fn executions(pool: &PgPool, project: Uuid, tasks: &[Uuid]) -> Result<Vec<Value>> {
    if tasks.is_empty() {
        return Ok(Vec::new());
    }
    let rows = sqlx::query(
        "SELECT e.task_id,e.attempt_id,e.session_id,COALESCE(h.state->>'status',e.state->>'status','unknown') AS status \
         FROM execution_attempts e LEFT JOIN profiled_authority_hot h USING(attempt_id) \
         WHERE e.task_id=ANY($1) AND e.kind='profiled_v1' AND e.holds_task \
         AND e.state->'offer'->'manifest'->'bindings'->>'project_id'=$2 \
         ORDER BY e.task_id,e.attempt_id LIMIT 65",
    )
    .bind(tasks)
    .bind(project.to_string())
    .fetch_all(pool)
    .await
    .map_err(|_| unavailable())?;
    rows.iter()
        .map(|row| {
            Ok(json!({
                "task_id": row.try_get::<Uuid, _>("task_id").map_err(|_| unavailable())?,
                "attempt_id": row.try_get::<Uuid, _>("attempt_id").map_err(|_| unavailable())?,
                "session_id": row.try_get::<Uuid, _>("session_id").map_err(|_| unavailable())?,
                "status": row.try_get::<String, _>("status").map_err(|_| unavailable())?,
            }))
        })
        .collect()
}

/// `GET .../tasks/{node}/work`: readiness with blocker explanation, parent and
/// the attached execution for one task.
pub(super) async fn work(
    State(s): State<AppState>,
    Extension(r): Extension<Arc<ReadRuntime>>,
    RequireOwner(u): RequireOwner,
    Path((p, id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Envelope>> {
    if id.is_nil() {
        return Err(invalid());
    }
    let a = access(&s, &r, u, p).await?;
    let w = a.record.binding.graph_workspace_id;
    let raw = context(&s, w, id).await?;
    let root = node(&raw["node"], w, "task")?;
    let complete = !raw["truncated"].as_bool().ok_or_else(unavailable)?;
    let nodes = raw["nodes"].as_array().ok_or_else(unavailable)?;
    let mut blockers = Vec::new();
    for e in raw["edges"].as_array().ok_or_else(unavailable)? {
        if e["edge_type"] != "blocks" || e["deleted"] != false || e["target"] != json!(id) {
            continue;
        }
        let source = nodes
            .iter()
            .find(|n| n["id"] == e["source"] && n["deleted"] == false);
        let (title, state) = match source {
            Some(n) => (n["title"].clone(), n["task_state"].clone()),
            None => (Value::Null, Value::Null),
        };
        let satisfied = match state.as_str() {
            Some("completed") => json!(true),
            Some(_) => json!(false),
            None => Value::Null,
        };
        blockers.push(json!({"edge_id":e["id"],"id":e["source"],"title":title,"task_state":state,"satisfied":satisfied}));
        if blockers.len() >= 64 {
            break;
        }
    }
    let execution = executions(&r.pool, p, &[id])
        .await?
        .pop()
        .unwrap_or(Value::Null);
    let open = blockers.iter().any(|b| b["satisfied"] != true);
    let state = match root["task_state"].as_str() {
        Some(s @ ("completed" | "canceled" | "draft")) => s,
        _ if !execution.is_null() => "running",
        _ if open => "waiting",
        _ if !complete => "unknown",
        _ => "ready",
    };
    let parent = parent(&raw, w, id)?;
    finish(
        &s,
        &r,
        a,
        json!({"task_id":id,"task_state":root["task_state"],"version":root["version"],
            "readiness":{"state":state,"blockers":blockers,"complete":complete},
            "parent":parent,"execution":execution}),
    )
    .await
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(super) struct HistoryPage {
    cursor: Option<String>,
}

/// `GET .../tasks/{node}/history`: Graph history entries (newest page from the
/// service order) plus the task's recorded attempts for the inspector timeline.
pub(super) async fn history(
    State(s): State<AppState>,
    Extension(r): Extension<Arc<ReadRuntime>>,
    RequireOwner(u): RequireOwner,
    Path((p, id)): Path<(Uuid, Uuid)>,
    q: std::result::Result<Query<HistoryPage>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Envelope>> {
    let Query(q) = q.map_err(|_| invalid())?;
    if id.is_nil() {
        return Err(invalid());
    }
    let a = access(&s, &r, u, p).await?;
    let w = a.record.binding.graph_workspace_id;
    let binding = cursor::Binding {
        actor: a.actor.id,
        project: p,
        revision: a.record.revision,
        workspace: w,
        kind: format!("history:{id}"),
        limit: HISTORY_LIMIT,
        q: None,
    };
    let mut query = vec![("limit".to_owned(), HISTORY_LIMIT.to_string())];
    if let Some(token) = q.cursor {
        query.push((
            "cursor".into(),
            r.cursors.decode(&token, &binding, stamp(&s))?,
        ));
    }
    // The node read fences kind: history is only served for live tasks.
    let raw = runtime(&s)?
        .human_get(&format!("/v1/workspaces/{w}/nodes/{id}"), &[], 1_000_000)
        .await?;
    if raw["id"] != json!(id) {
        return Err(unavailable());
    }
    node(&raw, w, "task")?;
    let page = runtime(&s)?
        .human_get(
            &format!("/v1/workspaces/{w}/nodes/{id}/history"),
            &query,
            1_000_000,
        )
        .await?;
    if !contract::upstream_valid("HistoryPage", &page) {
        return Err(unavailable());
    }
    let rows = page["items"].as_array().ok_or_else(unavailable)?;
    if rows.len() > usize::from(HISTORY_LIMIT) {
        return Err(unavailable());
    }
    let items: Vec<Value> = rows
        .iter()
        .map(|h| {
            Ok(json!({
                "sequence": h["sequence"].as_u64().filter(|n| *n > 0).ok_or_else(unavailable)?.to_string(),
                "entry_id": h["entry_id"], "category": h["category"], "operation": h["operation"],
                "actor_id": h["actor_id"], "reason": h["reason"], "created_at": h["created_at"],
            }))
        })
        .collect::<Result<_>>()?;
    let next = match page["next_cursor"].as_str() {
        Some(c) => Some(r.cursors.encode(&binding, c, stamp(&s))?),
        None => None,
    };
    let attempts = sqlx::query(
        "SELECT e.attempt_id,e.session_id,e.holds_task,COALESCE(h.state->>'status',e.state->>'status','unknown') AS status \
         FROM execution_attempts e LEFT JOIN profiled_authority_hot h USING(attempt_id) \
         WHERE e.task_id=$1 AND e.kind='profiled_v1' \
         AND e.state->'offer'->'manifest'->'bindings'->>'project_id'=$2 \
         ORDER BY e.holds_task DESC,e.attempt_id LIMIT $3",
    )
    .bind(id)
    .bind(p.to_string())
    .bind(ATTEMPT_LIMIT + 1)
    .fetch_all(&r.pool)
    .await
    .map_err(|_| unavailable())?;
    let attempts_truncated = attempts.len() as i64 > ATTEMPT_LIMIT;
    let attempts: Vec<Value> = attempts
        .iter()
        .take(ATTEMPT_LIMIT as usize)
        .map(|row| {
            Ok(json!({
                "attempt_id": row.try_get::<Uuid, _>("attempt_id").map_err(|_| unavailable())?,
                "session_id": row.try_get::<Uuid, _>("session_id").map_err(|_| unavailable())?,
                "holds_task": row.try_get::<bool, _>("holds_task").map_err(|_| unavailable())?,
                "status": row.try_get::<String, _>("status").map_err(|_| unavailable())?,
            }))
        })
        .collect::<Result<_>>()?;
    finish(
        &s,
        &r,
        a,
        json!({"task_id":id,"items":items,"next_cursor":next,
            "attempts":attempts,"attempts_truncated":attempts_truncated}),
    )
    .await
}

/// `GET .../tasks/{node}/summary`: completion counts over the task's bounded
/// structural subtree (Graph tree, depth 5). Delivery is owned by D1 and is
/// reported as unavailable rather than fabricated.
pub(super) async fn summary(
    State(s): State<AppState>,
    Extension(r): Extension<Arc<ReadRuntime>>,
    RequireOwner(u): RequireOwner,
    Path((p, id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Envelope>> {
    if id.is_nil() {
        return Err(invalid());
    }
    let a = access(&s, &r, u, p).await?;
    let w = a.record.binding.graph_workspace_id;
    let tree = runtime(&s)?
        .human_get(
            &format!("/v1/workspaces/{w}/nodes/{id}/tree"),
            &[
                ("depth".into(), "5".into()),
                ("limit".into(), "200".into()),
                ("max_bytes".into(), "1048576".into()),
            ],
            1_048_576,
        )
        .await?;
    if !contract::upstream_valid("Tree", &tree) || tree["node"]["id"] != json!(id) {
        return Err(unavailable());
    }
    node(&tree["node"], w, "task")?;
    let mut counts = buckets();
    let mut complete = true;
    let mut seen = std::collections::BTreeSet::from([id.to_string()]);
    let mut stack = vec![&tree];
    while let Some(t) = stack.pop() {
        complete &= t["truncated"] == false;
        for child in t["children"].as_array().ok_or_else(unavailable)? {
            let n = &child["node"];
            if n["workspace_id"] != json!(w) {
                return Err(unavailable());
            }
            let cid = n["id"].as_str().ok_or_else(unavailable)?;
            if seen.insert(cid.to_owned()) && n["kind"] == "task" && n["deleted"] == false {
                count(&mut counts, &n["task_state"]);
            }
            stack.push(child);
        }
    }
    let total: u64 = counts.values().filter_map(Value::as_u64).sum();
    let counts: serde_json::Map<_, _> = counts
        .into_iter()
        .map(|(k, v)| (k, json!(v.as_u64().unwrap_or(0).to_string())))
        .collect();
    finish(
        &s,
        &r,
        a,
        json!({"task_id":id,"scope":"descendants",
            "implementation":{"total":total.to_string(),"by_state":counts,"complete":complete},
            "delivery":{"state":"unavailable","owner":"D1"}}),
    )
    .await
}
