//! Work-view task projections: readiness, blockers, parent, attached execution,
//! history timeline and per-scope completion counts (G11 X8-X13).
//! Readiness combines Graph dependencies, execution holds and current scheduler observations.
use super::*;

const HISTORY_LIMIT: u16 = 25;
const ATTEMPT_LIMIT: i64 = 16;

pub(super) async fn executions(pool: &PgPool, project: Uuid, tasks: &[Uuid]) -> Result<Vec<Value>> {
    aq_human_work::holding_executions(pool, project, tasks)
        .await
        .map_err(|_| unavailable())
}

/// A fixed label for an error's kind, for logs: never the variant's payload.
fn error_class(error: &AppError) -> &'static str {
    match error {
        AppError::Conflict(_) => "conflict",
        AppError::BadRequest(_) => "bad_request",
        AppError::NotFound(_) => "not_found",
        AppError::Busy => "busy",
        AppError::Internal => "internal",
        _ => "other",
    }
}

/// Show the current delivery decision or the policy that can decide an open result.
#[cfg(feature = "delivery_playbook")]
async fn result_waiting(
    s: &AppState,
    pool: &PgPool,
    project: Uuid,
    h: &aq_human_work::Hold,
) -> Option<Value> {
    use aq_playbook_domain::delivery_store as store;
    let mut db = pool.acquire().await.ok()?;
    let (result, hold) = store::result_wait(&mut db, h.attempt_id).await.ok()??;
    if let Some(reason) = hold {
        let evidence = store::held_pr_evidence(&mut db, result)
            .await
            .ok()
            .flatten();
        return Some(aq_human_work::delivery_hold(
            h,
            result,
            &reason,
            evidence.as_deref(),
        ));
    }
    let pb = s.config.playbooks.as_deref().filter(|pb| {
        pb.profile.is_delivery() && (pb.installation_routing || pb.namespace.project_id == project)
    });
    let active = match pb {
        Some(pb) => store::active(
            &mut db,
            (pb.namespace.namespace_id, pb.namespace.generation),
        )
        .await
        .ok()?,
        None => false,
    };
    let mut waiting = aq_human_work::result_waiting(h.attempt_id, result, active);
    if let Ok(Some(check)) = store::checking_pr(&mut db, result).await
        && let Some(Value::String(message)) = waiting.get_mut("message")
    {
        message.push(' ');
        message.push_str(&check);
    }
    Some(waiting)
}

/// An optional read of the recorded result, restricted to the same claim as
/// the budget hold. Readiness stays available if the detail cannot be read.
#[cfg(feature = "delivery_playbook")]
async fn budget_evidence(
    pool: &PgPool,
    project: Uuid,
    task: Uuid,
    attempt: Uuid,
) -> Option<String> {
    let mut db = pool.acquire().await.ok()?;
    aq_playbook_domain::delivery_store::returned_result_evidence(&mut db, project, task, attempt)
        .await
        .ok()?
}

/// The hierarchy relationships that keep the active task `id` out of the
/// runnable queue (Graph's ready query leaves out a task under a draft or
/// retired ancestor and a container with live children), and whether the read
/// is complete. The ancestry above the depth-1 Context `raw` is one more
/// bounded incoming read; when it fails the answer is `unknown`, not an error.
async fn hierarchy_of(s: &AppState, w: Uuid, id: Uuid, raw: &Value) -> Result<(Vec<Value>, bool)> {
    let depth = aq_agent_grants::draft_scope::MAX_ANCESTRY;
    let read = || async {
        let read = context_at(s, w, id, depth, "incoming").await;
        read.inspect_err(
            |e| tracing::warn!(task = %id, class = error_class(e), "task ancestry unavailable"),
        )
        .ok()
    };
    let found = aq_human_work::hierarchy_via(raw, id, depth, read).await;
    let found = found.ok_or_else(unavailable)?;
    Ok((found.blocking, found.complete))
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
    let hold = aq_human_work::attempt_hold(&r.pool, &execution["attempt_id"]).await;
    let hold = hold.map_err(|_| unavailable())?;
    // R6: the task's recorded hold; observing the task closed or retired clears it.
    let open = root["task_state"] == "active" && root["retired"] != true;
    if !open {
        aq_machine_control::holds::clear_closed(&r.pool, id, (s.clock)()).await;
    }
    let held = aq_machine_control::holds::current(&r.pool, p, id).await;
    let mut held = held.map_err(|_| unavailable())?;
    if let Some(h) = held
        .as_mut()
        .filter(|h| aq_machine_control::holds::is_budget_hold(&h.reason))
    {
        let current = a.record.revision;
        let pinned =
            aq_profiled_state::replacement_sql::predecessor_project_revision(&r.pool, h.attempt_id)
                .await
                .map_err(|_| unavailable())?;
        if pinned != Some(current) {
            let now = u64::try_from((s.clock)().timestamp_millis()).map_err(|_| unavailable())?;
            if let Some(code) =
                crate::profiled_execution::replacement::retry_hold(&s, id, now, current).await?
            {
                h.reason = code.into();
            }
        }
    }
    #[cfg(feature = "delivery_playbook")]
    let evidence = match &held {
        Some(h) if aq_machine_control::holds::is_budget_hold(&h.reason) => {
            budget_evidence(&r.pool, p, id, h.attempt_id).await
        }
        _ => None,
    };
    let held = held.map(|h| {
        let phrase = aq_machine_control::holds::phrase(&h.reason);
        let reason = aq_human_work::task_hold(h.attempt_id, &h.status, &h.reason, &phrase);
        #[cfg(feature = "delivery_playbook")]
        if let Some(evidence) = evidence {
            let mut reason = reason;
            reason["message"] = json!(format!(
                "{} {evidence}",
                reason["message"].as_str().unwrap_or_default()
            ));
            return reason;
        }
        reason
    });
    // Spec 0014 §5.5: a task whose effective role is disabled or gone is held.
    let role = aq_role_settings::tasks::effective_role(
        &raw["node"]["metadata"],
        &a.record.binding.default_role,
    );
    let role_hold = crate::role_settings::hold(&s, &r.pool, &role).await?;
    // Spec 0014 §5.2: so is a task whose level has no document.
    let level_hold = match role_hold {
        Some(_) => None,
        None => {
            let binding = &a.record.binding;
            crate::role_settings::level_hold(&s, &r.pool, binding, &raw["node"]).await?
        }
    };
    // Spec 0014 §6.1: and one whose last offer was refused before any attempt.
    let offer_hold = match open && execution.is_null() && crate::role_settings::tables(&s) {
        true => aq_role_settings::refusals::shown(&r.pool, id).await,
        false => None,
    };
    let held = (held.into_iter().chain(role_hold).chain(level_hold)).chain(offer_hold);
    #[cfg(feature = "delivery_playbook")]
    let held = held.chain(match (&hold, open) {
        (Some(h), true) if h.stopped => result_waiting(&s, &r.pool, p, h).await,
        _ => None,
    });
    let (hierarchy, complete) = match open {
        true => hierarchy_of(&s, w, id, &raw).await?,
        false => (Vec::new(), complete),
    };
    let mut readiness = aq_human_work::readiness_in(
        root["task_state"].as_str(),
        hold.as_ref(),
        held.collect(),
        blockers,
        hierarchy,
        complete,
    );
    aq_swarm_settings::admission::show_task_wait(
        &mut readiness,
        &r.pool,
        id,
        (s.clock)().timestamp_millis(),
    )
    .await
    .map_err(|_| unavailable())?;
    let parent = parent(&raw, w, id)?;
    let mut data = json!({"task_id":id,"task_state":root["task_state"],"version":root["version"],
        "readiness":readiness,"parent":parent,"execution":execution});
    crate::role_settings::show(&mut data, &raw["node"], &a.record.binding.default_role);
    // Owner ruling 4b: the latest confirmed default-policy settlement found no file change.
    if root["task_state"] == "completed"
        && aq_human_work::completed_without_changes(&r.pool, id).await
    {
        data["completed_without_changes"] = json!(true);
    }
    let ready = data["readiness"]["state"].as_str().unwrap_or_default();
    // Optional field: when it cannot be computed the read still succeeds without it.
    match crate::routing::view(&s, id, &raw["node"], ready).await {
        Ok(Some(routing)) => data["routing"] = routing,
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(task = %id, class = error_class(&error), "task routing view omitted")
        }
    }
    finish(&s, &r, a, data).await
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
    let actors = aq_human_work::attribution::history(
        &r.pool,
        s.agent_access
            .as_ref()
            .and_then(|rt| rt.mutation_actor.as_deref()),
        (p, w),
        rows,
    )
    .await
    .map_err(|_| unavailable())?;
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
    let ran_with = aq_role_settings::attempts::ran_with_rows(&r.pool, &attempts).await;
    let attempts: Vec<Value> = attempts
        .iter()
        .take(ATTEMPT_LIMIT as usize)
        .map(|row| {
            let attempt: Uuid = row.try_get("attempt_id").map_err(|_| unavailable())?;
            let mut shown = json!({
                "attempt_id": attempt,
                "session_id": row.try_get::<Uuid, _>("session_id").map_err(|_| unavailable())?,
                "holds_task": row.try_get::<bool, _>("holds_task").map_err(|_| unavailable())?,
                "status": row.try_get::<String, _>("status").map_err(|_| unavailable())?,
            });
            if let Some(summary) = ran_with.get(&attempt) {
                shown["ran_with"] = json!(summary);
            }
            Ok(shown)
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
