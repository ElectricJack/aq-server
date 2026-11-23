//! Work-view task projections: readiness, blockers, parent, attached execution,
//! history timeline and per-scope completion counts (G11 X8-X13).
//! Readiness combines Graph dependencies, execution holds and current scheduler observations.
use super::*;

const HISTORY_LIMIT: u16 = 25;

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
    let held = aq_human_work::recorded_hold(&r.pool, p, id, held.as_ref()).await;
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
        (Some(h), true) if h.stopped => {
            let namespace = s
                .config
                .playbooks
                .as_deref()
                .filter(|pb| {
                    pb.profile.is_delivery()
                        && (pb.installation_routing || pb.namespace.project_id == p)
                })
                .map(|pb| (pb.namespace.namespace_id, pb.namespace.generation));
            aq_human_work::delivery::waiting(&r.pool, namespace, h).await
        }
        _ => None,
    });
    let (hierarchy, complete) = match open {
        true => hierarchy_of(&s, w, id, &raw).await?,
        false => (Vec::new(), complete),
    };
    let mut readiness = aq_human_work::readiness(
        root["task_state"].as_str(),
        hold.as_ref(),
        held,
        aq_human_work::TaskDependencies {
            blockers,
            hierarchy,
            complete,
        },
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
    aq_human_work::show_no_change_settlement(&mut data, &r.pool, id).await;
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
    let service = s
        .agent_access
        .as_ref()
        .and_then(|rt| rt.mutation_actor.as_deref());
    let items = aq_human_work::history::items(
        &r.pool,
        service,
        (p, w),
        aq_human_work::history::TaskHistory {
            task: id,
            rows,
            prior: prior.as_ref(),
        },
    )
    .await
    .ok_or_else(unavailable)?;
    let next = match page["next_cursor"].as_str() {
        Some(c) => Some(r.cursors.encode(&binding, c, stamp(&s))?),
        None => None,
    };
    let (attempts, attempts_truncated) = aq_human_work::history::attempts(&r.pool, p, id)
        .await
        .map_err(|_| unavailable())?;
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
