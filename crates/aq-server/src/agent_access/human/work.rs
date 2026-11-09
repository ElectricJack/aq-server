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
    let blockers = aq_human_work::blockers(&raw, id).ok_or_else(unavailable)?;
    let execution = executions(&r.pool, p, &[id])
        .await?
        .pop()
        .unwrap_or(Value::Null);
    // QA P1-01: a stopped attempt that keeps the claim is `held`, not running.
    let hold = match execution["attempt_id"].as_str().and_then(|a| a.parse().ok()) {
        Some(attempt) => sqlx::query_scalar::<_, Value>("SELECT COALESCE(h.state,e.state) FROM execution_attempts e LEFT JOIN profiled_authority_hot h USING(attempt_id) WHERE e.attempt_id=$1")
            .bind(attempt).fetch_optional(&r.pool).await.map_err(|_| unavailable())?
            .map(|state| aq_human_work::Hold::from_authority(attempt, &state)),
        None => None,
    };
    // R6: the task's recorded hold; observing the task closed or retired clears it.
    let open = root["task_state"] == "active" && root["retired"] != true;
    if !open {
        aq_machine_control::holds::clear_closed(&r.pool, id, (s.clock)()).await;
    }
    let held = aq_machine_control::holds::current(&r.pool, p, id).await;
    let held = (held.map_err(|_| unavailable())?).map(|h| {
        let phrase = aq_machine_control::holds::phrase(&h.reason);
        aq_human_work::task_hold(h.attempt_id, &h.status, &h.reason, &phrase)
    });
    let readiness = aq_human_work::readiness(
        root["task_state"].as_str(),
        hold.as_ref(),
        held,
        blockers,
        complete,
    );
    let parent = parent(&raw, w, id)?;
    finish(
        &s,
        &r,
        a,
        json!({"task_id":id,"task_state":root["task_state"],"version":root["version"],
            "readiness":readiness,
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
    // QA P1-08: say what changed; the oldest revision diffs against Graph's prior one.
    let prior = match aq_human_work::predecessor(rows, id) {
        Some(n) => runtime(&s)?
            .human_get(
                &format!("/v1/workspaces/{w}/nodes/{id}/revisions/{n}"),
                &[],
                1_000_000,
            )
            .await
            .ok()
            .filter(|v| contract::upstream_valid("RevisionSnapshot", v))
            .map(|v| v["node"].clone()),
        None => None,
    };
    let changes = aq_human_work::changes(rows, id, prior.as_ref());
    let actors = actors(&s, &r, id, rows).await?;
    let items: Vec<Value> = rows
        .iter()
        .zip(changes.into_iter().zip(actors))
        .map(|(h, (change, actor))| {
            Ok(json!({
                "sequence": h["sequence"].as_u64().filter(|n| *n > 0).ok_or_else(unavailable)?.to_string(),
                "entry_id": h["entry_id"], "category": h["category"], "operation": h["operation"],
                "actor_id": actor, "reason": h["reason"], "created_at": h["created_at"], "change": change,
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

/// QA P1-07: Graph names AQ's one mutation identity as every AQ write's actor.
/// An entry in a change set that an owner's journalled operation confirmed
/// (dashboard action or task create), or the creation of a task an owner's
/// operation created (#97), names that owner's email instead.
async fn actors(s: &AppState, r: &ReadRuntime, task: Uuid, rows: &[Value]) -> Result<Vec<Value>> {
    let mut out: Vec<Value> = rows.iter().map(|h| h["actor_id"].clone()).collect();
    let Some(service) = s
        .agent_access
        .as_deref()
        .and_then(|rt| rt.mutation_actor.clone())
    else {
        return Ok(out);
    };
    let ours = |h: &Value| h["actor_id"] == json!(service);
    let sets: Vec<Uuid> = rows
        .iter()
        .filter(|h| ours(h))
        .filter_map(|h| h["change_set_id"].as_str()?.parse().ok())
        .collect();
    if sets.is_empty() {
        return Ok(out);
    }
    let found: Vec<(Option<Uuid>, String)> = sqlx::query_as("SELECT c.cs,u.email FROM unnest($1::uuid[]) AS c(cs) JOIN users u ON u.id=COALESCE((SELECT d.owner_id FROM dashboard_operations d WHERE d.disposition='confirmed' AND d.response->'receipt_refs' @> jsonb_build_array(jsonb_build_object('type','graph_change_set','id',c.cs::text)) LIMIT 1),(SELECT h.actor_id FROM human_graph_operations h WHERE h.raw_receipt->>'change_set_id'=c.cs::text LIMIT 1)) UNION ALL SELECT NULL,u.email FROM users u WHERE u.id=COALESCE((SELECT d.owner_id FROM dashboard_operations d WHERE d.operation_id=$2),(SELECT h.actor_id FROM human_graph_operations h WHERE h.operation_id=$2))")
        .bind(&sets).bind(task).fetch_all(&r.pool).await.map_err(|_| unavailable())?;
    for (h, actor) in rows.iter().zip(out.iter_mut()).filter(|(h, _)| ours(h)) {
        let set = found
            .iter()
            .find(|f| f.0.is_some() && h["change_set_id"] == json!(f.0));
        let created = found
            .iter()
            .find(|f| f.0.is_none() && h["operation"] == "created" && h["category"] != "edge");
        if let Some((_, email)) = set.or(created) {
            *actor = json!(email);
        }
    }
    Ok(out)
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
    let (counts, total, complete) =
        aq_human_work::descendants(&tree, w, id).ok_or_else(unavailable)?;
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
