//! Work-view projections for AQ's owner Graph reads (`human-graph-1`, G11
//! X8-X13): task-state counts, edge projection, readiness with its blockers
//! and hold reasons, and a change summary for each task history entry.
//!
//! The all-project task list joins Graph's page to AQ's current owner and project bindings.
pub mod attribution;
pub mod delivery;
pub mod epic_retirement;
pub mod history;
pub mod knowledge_actions;
pub mod overview;
pub mod projection;
pub mod subtree_activation;
pub mod task_git_author;
pub mod task_graph;
pub mod task_list;
use serde_json::{Map, Value, json};
use uuid::Uuid;

/// Task-state buckets for omitted and summary counts; an unset state is `unknown`.
pub const STATES: [&str; 5] = ["draft", "active", "completed", "canceled", "unknown"];
/// Longest history change summary, in characters.
pub const MAX_SUMMARY: usize = 512;

/// The bucket for a task state.
pub fn bucket(state: &Value) -> &'static str {
    state
        .as_str()
        .and_then(|s| STATES[..4].iter().copied().find(|b| *b == s))
        .unwrap_or("unknown")
}
/// Zero counts for every bucket.
pub fn buckets() -> Map<String, Value> {
    STATES.iter().map(|k| ((*k).to_owned(), json!(0))).collect()
}
/// Count one task in its state's bucket.
pub fn count(map: &mut Map<String, Value>, state: &Value) {
    let slot = map.entry(bucket(state)).or_insert(json!(0));
    *slot = json!(slot.as_u64().unwrap_or(0) + 1);
}
/// Counts as decimal strings, the wire form.
pub fn decimal(map: Map<String, Value>) -> Map<String, Value> {
    map.into_iter()
        .map(|(k, v)| (k, json!(v.as_u64().unwrap_or(0).to_string())))
        .collect()
}

/// Live task descendants of `root` in a bounded Graph tree, counted once each
/// by state (decimal strings), with their total and whether no level was
/// truncated. None when a level is malformed or crosses `workspace`.
pub fn descendants(
    tree: &Value,
    workspace: Uuid,
    root: Uuid,
) -> Option<(Map<String, Value>, u64, bool)> {
    let mut counts = buckets();
    let mut complete = true;
    let mut seen = std::collections::BTreeSet::from([root.to_string()]);
    let mut stack = vec![tree];
    while let Some(t) = stack.pop() {
        complete &= t["truncated"] == false;
        for child in t["children"].as_array()? {
            let n = &child["node"];
            if n["workspace_id"] != json!(workspace) {
                return None;
            }
            let first = seen.insert(n["id"].as_str()?.to_owned());
            let live_task = n["kind"] == "task" && n["deleted"] == false;
            if first && live_task {
                count(&mut counts, &n["task_state"]);
            }
            stack.push(child);
        }
    }
    let total = counts.values().filter_map(Value::as_u64).sum();
    Some((decimal(counts), total, complete))
}

/// The public projection of a Graph edge: identity, endpoints, type and
/// description, with positive revisions as decimal strings. None when a
/// revision is malformed.
pub fn edge(edge: &Value) -> Option<Value> {
    let mut e = Map::new();
    for k in ["id", "source", "target", "edge_type", "description"] {
        e.insert(k.into(), edge[k].clone());
    }
    for k in ["version", "source_revision", "target_revision"] {
        let v = if edge[k].is_null() && k != "version" {
            Value::Null
        } else {
            json!(edge[k].as_u64().filter(|n| *n > 0)?.to_string())
        };
        e.insert(k.into(), v);
    }
    Some(Value::Object(e))
}

/// Live `blocks` edges that target `task` in a depth-1 Graph Context, at most
/// 64. `satisfied` is true only for a completed source and null when the
/// source is not in the Context. None when the Context lacks node or edge lists.
pub fn blockers(context: &Value, task: Uuid) -> Option<Vec<Value>> {
    let nodes = context["nodes"].as_array()?;
    let mut out = Vec::new();
    for e in context["edges"].as_array()? {
        let live_block = e["edge_type"] == "blocks" && e["deleted"] == false;
        if !live_block || e["target"] != json!(task) {
            continue;
        }
        let source = nodes
            .iter()
            .find(|n| n["id"] == e["source"] && n["deleted"] == false);
        let (title, state) = source.map_or((Value::Null, Value::Null), |n| {
            (n["title"].clone(), n["task_state"].clone())
        });
        let satisfied = state
            .as_str()
            .map_or(Value::Null, |s| json!(s == "completed"));
        out.push(json!({"edge_id":e["id"],"id":e["source"],"title":title,"task_state":state,"satisfied":satisfied}));
        if out.len() >= 64 {
            break;
        }
    }
    Some(out)
}

<<<<<<< HEAD
/// Longest list of live children a hierarchy reports.
pub const MAX_HIERARCHY_CHILDREN: usize = 16;

/// What the task's place in the Graph hierarchy says about running it.
pub struct Hierarchy {
    /// Relationships that keep the task out of the runnable queue, ancestors
    /// nearest first, then live children (at most [`MAX_HIERARCHY_CHILDREN`]).
    pub blocking: Vec<Value>,
    /// False when a Context was truncated, an ancestor could not be read, or
    /// the ancestry was needed but not supplied.
    pub complete: bool,
}

fn live_parent_edge(e: &Value) -> bool {
    e["edge_type"] == "parent-child" && e["deleted"] == false
}

/// Whether the hierarchy above `task` has to be read from a deeper incoming
/// Context: the depth-1 Context shows a live parent, or it was truncated.
pub fn needs_ancestry(context: &Value, task: Uuid) -> bool {
    context["truncated"] != false
        || context["edges"].as_array().is_some_and(|edges| {
            edges
                .iter()
                .any(|e| live_parent_edge(e) && e["target"] == json!(task))
        })
}

/// The hierarchy relationships that keep an active `task` from being runnable
/// (Graph's ready query excludes it): a draft or retired ancestor within
/// `max_levels` above it, or any live child (a container is not a leaf).
/// `context` is the task's depth-1 Context in both directions; `incoming` is
/// its incoming Context at `max_levels` depth, needed when
/// [`needs_ancestry`]. None when a Context lacks node or edge lists.
pub fn hierarchy(
    context: &Value,
    incoming: Option<&Value>,
    task: Uuid,
    max_levels: u32,
) -> Option<Hierarchy> {
    let mut complete = context["truncated"] == false
        && incoming.is_none_or(|c| c["truncated"] == false)
        && (incoming.is_some() || !needs_ancestry(context, task));
    let pools: Vec<&Value> = std::iter::once(context).chain(incoming).collect();
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    for c in pools {
        nodes.extend(c["nodes"].as_array()?);
        edges.extend(c["edges"].as_array()?);
    }
    let node = |id: &Value| nodes.iter().find(|n| &n["id"] == id);
    let mut blocking = Vec::new();
    let mut seen = std::collections::BTreeSet::from([task]);
    let mut level = vec![task];
    for depth in 1..=max_levels {
        let mut above = Vec::new();
        for cur in &level {
            for e in edges
                .iter()
                .filter(|e| live_parent_edge(e) && e["target"] == json!(cur))
            {
                let id: Uuid = e["source"].as_str()?.parse().ok()?;
                if !seen.insert(id) {
                    continue;
                }
                let Some(n) = node(&e["source"]).filter(|n| n["deleted"] == false) else {
                    complete = false;
                    continue;
                };
                if n["kind"] != "task" {
                    continue;
                }
                let code = if n["retired"] == true {
                    Some("ancestor_retired")
                } else if n["task_state"] == "draft" {
                    Some("ancestor_draft")
                } else {
                    None
                };
                if let Some(code) = code {
                    blocking.push(json!({"relationship":"ancestor","level":depth,"id":id,"title":n["title"],
                        "task_state":n["task_state"],"retired":n["retired"],"edge_id":e["id"],"code":code}));
                }
                above.push(id);
            }
        }
        level = above;
    }
    let children = edges
        .iter()
        .filter(|e| live_parent_edge(e) && e["source"] == json!(task));
    let mut listed = 0;
    for e in children {
        let n = node(&e["target"]);
        if n.is_some_and(|n| n["deleted"] == true || n["retired"] == true) {
            continue;
        }
        listed += 1;
        if listed > MAX_HIERARCHY_CHILDREN {
            break;
        }
        let field = |k: &str| n.map_or(Value::Null, |n| n[k].clone());
        blocking.push(json!({"relationship":"child","level":1,"id":e["target"],"title":field("title"),
            "task_state":field("task_state"),"retired":field("retired"),"edge_id":e["id"],"code":"has_live_children"}));
    }
    Some(Hierarchy { blocking, complete })
}

/// [`hierarchy`] for the work view: reads the incoming ancestry through
/// `incoming` only when [`needs_ancestry`]. A read that returns None (it
/// failed) leaves the answer incomplete, not an error.
pub async fn hierarchy_via<F: Future<Output = Option<Value>>>(
    context: &Value,
    task: Uuid,
    max_levels: u32,
    incoming: impl FnOnce() -> F,
) -> Option<Hierarchy> {
    let deeper = match needs_ancestry(context, task) {
        true => incoming().await,
        false => None,
    };
    hierarchy(context, deeper.as_ref(), task, max_levels)
}

/// Parent of `task` from a present `parent-child` edge (the source is the
/// parent) in a Context; `Null` without one. `check` validates the parent's
/// node for its kind. None when the parent node fails it.
pub fn parent(
    context: &Value,
    task: Uuid,
    check: impl Fn(&Value, &str) -> Option<Value>,
) -> Option<Value> {
    let edges = context["edges"].as_array().into_iter().flatten();
    let Some(edge) = { edges }.find(|e| live_parent_edge(e) && e["target"] == json!(task)) else {
        return Some(Value::Null);
    };
    let pid = &edge["source"];
    let nodes = context["nodes"].as_array().into_iter().flatten();
    let Some(found) = { nodes }.find(|n| &n["id"] == pid && n["deleted"] == false) else {
        return Some(
            json!({"id":pid,"kind":null,"title":null,"task_state":null,"edge_id":edge["id"]}),
        );
    };
    let n = check(found, found["kind"].as_str()?)?;
    Some(
        json!({"id":pid,"kind":n["kind"],"title":n["title"],"task_state":n["task_state"],"edge_id":edge["id"]}),
    )
}

/// Holding attempt (the agent session attached to each task) for a page of
/// tasks in this project. At most one attempt holds a task.
pub async fn holding_executions(
    pool: &PgPool,
    project: Uuid,
    tasks: &[Uuid],
) -> Result<Vec<Value>, sqlx::Error> {
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
    .await?;
    // Spec 0014 §7.1: what each attempt ran with, from its own record.
    let ran_with = aq_role_settings::attempts::ran_with_rows(pool, &rows).await;
    rows.iter()
        .map(|row| {
            let attempt: Uuid = row.try_get("attempt_id")?;
            let mut execution = json!({
                "task_id": row.try_get::<Uuid, _>("task_id")?,
                "attempt_id": attempt,
                "session_id": row.try_get::<Uuid, _>("session_id")?,
                "status": row.try_get::<String, _>("status")?,
            });
            if let Some(summary) = ran_with.get(&attempt) {
                execution["ran_with"] = json!(summary);
            }
            Ok(execution)
        })
        .collect()
}

/// The hold of the attempt named by an execution projection's `attempt_id`
/// (the agent session attached to a task), read from its authority state;
/// None when there is no such attempt.
pub async fn attempt_hold(
    pool: &sqlx::PgPool,
    attempt_id: &Value,
) -> Result<Option<Hold>, sqlx::Error> {
    let Some(attempt) = attempt_id.as_str().and_then(|a| a.parse().ok()) else {
        return Ok(None);
    };
    let state = sqlx::query_scalar::<_, Value>("SELECT COALESCE(h.state,e.state) FROM execution_attempts e LEFT JOIN profiled_authority_hot h USING(attempt_id) WHERE e.attempt_id=$1")
        .bind(attempt).fetch_optional(pool).await?;
    Ok(state.map(|state| Hold::from_authority(attempt, &state)))
}

/// Whether the latest confirmed default-policy settlement of `task` found no
/// file change (owner ruling 4b).
pub async fn completed_without_changes(pool: &sqlx::PgPool, task: Uuid) -> bool {
    let quiet = sqlx::query_scalar::<_, Option<String>>(
        "SELECT s.evidence#>>'{delivery,state}' FROM profiled_auto_settlements s JOIN execution_attempts e USING(attempt_id) WHERE e.task_id=$1 AND s.disposition='confirmed' ORDER BY s.updated_at_ms DESC LIMIT 1",
    );
    matches!(quiet.bind(task).fetch_optional(pool).await, Ok(Some(Some(s))) if s == "no_change")
}

/// Add the settled delivery outcome to a completed task's work view.
pub async fn show_no_change_settlement(data: &mut Value, pool: &PgPool, task: Uuid) {
    if data["task_state"] == "completed" && completed_without_changes(pool, task).await {
        data["completed_without_changes"] = json!(true);
    }
}

/// The attempt holding a task, from AQ's execution authority state (the hot
/// authority for adopted native attempts, else the attempt's own state).
pub struct Hold {
    pub attempt_id: Uuid,
    /// `offered`, `preparing`, `granted`, `cancelling`, `finished`, `declined`,
    /// `unknown` or `never_granted`.
    pub status: String,
    /// A B3 cancellation is recorded for the attempt.
    pub cancelled: bool,
    /// The stop proof is a Stopped or NotStarted receipt.
    pub stopped: bool,
}
impl Hold {
    pub fn from_authority(attempt_id: Uuid, state: &Value) -> Self {
        Self {
            attempt_id,
            status: state["status"].as_str().unwrap_or("unknown").to_owned(),
            cancelled: !state["cancellation_id"].is_null(),
            stopped: matches!(
                state["stop"]["kind"].as_str(),
                Some("stopped" | "not_started")
            ),
        }
    }
    /// Still executing, or not provably stopped, while it holds the task.
    pub fn running(&self) -> bool {
        !self.stopped
            && !matches!(
                self.status.as_str(),
                "finished" | "declined" | "never_granted"
            )
    }
    /// Why a task whose attempt no longer runs is still held; None while running.
    pub fn reason(&self) -> Option<Value> {
        if self.running() {
            return None;
        }
        let (code, message) = if !self.stopped {
            (
                "attempt_finished_stop_unconfirmed",
                "The attempt has finished but its stop is not proven yet, so it still holds the task.",
            )
        } else if self.cancelled {
            (
                "cancelled_attempt_holds_task",
                "The attempt was cancelled and has stopped, but it still holds the task: a cancelled attempt keeps its retained source for a replacement (B4). The claim is released when the task is retried or the owner abandons the attempt.",
            )
        } else {
            (
                "attempt_stopped_not_released",
                "The attempt has finished and stopped, but it still holds the task until its result is settled, the task is retried or the owner abandons the attempt.",
            )
        };
        Some(
            json!({"code":code,"attempt_id":self.attempt_id,"status":self.status,"message":message}),
        )
    }
}

/// Format the current scheduling hold and any recorded result for that same attempt.
pub async fn recorded_hold(
    pool: &PgPool,
    project: Uuid,
    task: Uuid,
    current: Option<&aq_machine_control::holds::Current>,
) -> Option<Value> {
    let h = current?;
    let phrase = aq_machine_control::holds::phrase(&h.reason);
    let shown = task_hold(h.attempt_id, &h.status, &h.reason, &phrase);
    if aq_machine_control::holds::is_budget_hold(&h.reason)
        && let Some(evidence) = delivery::budget_evidence(pool, project, task, h.attempt_id).await
    {
        let mut shown = shown;
        if let Some(Value::String(message)) = shown.get_mut("message") {
            message.push(' ');
            message.push_str(&evidence);
        }
        return Some(shown);
    }
    Some(shown)
}

/// A server-side hold of the task itself (R6): AQ does not offer it, for `code` (a server reason
/// code, `phrase` its fixed description), about `attempt_id` (with its authority `status`).
pub fn task_hold(attempt_id: Uuid, status: &str, code: &str, phrase: &str) -> Value {
    let message = format!("AQ does not offer this task: {phrase} (hold reason: {code}).");
    json!({"code":"task_held","attempt_id":attempt_id,"status":status,"message":message,"hold_reason":code})
}

/// A delivery hold has a result and reason even after the playbook has ended.
pub fn delivery_hold(hold: &Hold, result: Uuid, reason: &str, evidence: Option<&str>) -> Value {
    let why = match reason {
        "returns_exhausted" => "the delivery playbook reached its repair-return limit",
        _ => "the delivery playbook held the recorded result",
    };
    let mut shown = task_hold(
        hold.attempt_id,
        &hold.status,
        reason,
        &format!("{why} for result {result}"),
    );
    if let Some(evidence) = evidence {
        shown["message"] = json!(format!(
            "{} {evidence}",
            shown["message"].as_str().unwrap_or_default()
        ));
    }
    shown
}

/// Epic 0 (spec 0016 section 7): the task read's word on a stopped attempt
/// that still holds its task with a recorded result no playbook decided:
/// while no delivery playbook is active, the result waits and nothing else
/// completes, publishes or reworks it. `delivery_policy` says whether a
/// delivery playbook is active for the task's project.
pub fn result_waiting(attempt: Uuid, result: Uuid, delivery_policy: bool) -> Value {
    let (code, message) = match delivery_policy {
        false => (
            "result_ready_no_delivery_policy",
            "Result ready, no delivery policy: the attempt has stopped with a result, and no delivery playbook is active to decide it. AQ does not complete, publish or send back a result by itself, so the result waits.",
        ),
        true => (
            "result_ready",
            "Result ready: the attempt has stopped with a result, which waits for the delivery playbook's decision.",
        ),
    };
    json!({"code": code, "attempt_id": attempt, "result_id": result, "message": message})
}

/// Work-view readiness. The state is the first match of: the task's own
/// `draft`, `completed` or `canceled` state; `running` while the holding
/// attempt executes; `held` when it has stopped but keeps the task claim, or
/// an active task has a server-side hold ([`task_hold`]); `waiting` for an
/// unsatisfied blocker or a hierarchy relationship ([`hierarchy`]) that keeps
/// the task out of the runnable queue; `unknown` for a truncated Context;
/// otherwise `ready`. `reasons` explains a stopped attempt's hold in any
/// state, then the task's hold.
pub fn readiness(
    task_state: Option<&str>,
    hold: Option<&Hold>,
    held: Option<Value>,
    blockers: Vec<Value>,
    complete: bool,
) -> Value {
    readiness_with(
        task_state,
        hold,
        held.into_iter().collect(),
        blockers,
        complete,
    )
}
/// [`readiness`] for a task that may be held for more than one reason: its
/// recorded hold ([`task_hold`]) and its role (spec 0014 §5.5: `role_disabled`
/// or `role_missing`, as `{code, role, message}`). Each is listed, in order.
pub fn readiness_with(
    task_state: Option<&str>,
    hold: Option<&Hold>,
    held: Vec<Value>,
    blockers: Vec<Value>,
    complete: bool,
) -> Value {
    readiness_in(task_state, hold, held, blockers, Vec::new(), complete)
}
/// [`readiness_with`] for a task whose place in the hierarchy keeps it out of
/// the runnable queue. A non-empty `hierarchy` is listed as `hierarchy` on
/// the result and makes an otherwise ready active task `waiting`; it is
/// omitted when empty, so older readers see the shape they know.
pub fn readiness_in(
    task_state: Option<&str>,
    hold: Option<&Hold>,
    held: Vec<Value>,
    blockers: Vec<Value>,
    hierarchy: Vec<Value>,
    complete: bool,
) -> Value {
    let open = blockers.iter().any(|b| b["satisfied"] != true);
    let held = if task_state == Some("active") {
        held
    } else {
        Vec::new()
    };
    let hierarchy = if task_state == Some("active") {
        hierarchy
    } else {
        Vec::new()
    };
    let state = match task_state {
        Some(s @ ("completed" | "canceled" | "draft")) => s,
        _ if hold.is_some_and(Hold::running) => "running",
        _ if hold.is_some() || !held.is_empty() => "held",
        _ if open || !hierarchy.is_empty() => "waiting",
        _ if !complete => "unknown",
        _ => "ready",
    };
    let reasons: Vec<Value> = hold
        .and_then(Hold::reason)
        .into_iter()
        .chain(held)
        .collect();
    let mut out = json!({"state":state,"blockers":blockers,"complete":complete,"reasons":reasons});
    if !hierarchy.is_empty() {
        out["hierarchy"] = Value::Array(hierarchy);
    }
    out
}

=======
mod hierarchy;
pub use hierarchy::{
    Hierarchy, MAX_HIERARCHY_CHILDREN, hierarchy, hierarchy_via, needs_ancestry, parent,
};
mod holds;
pub use holds::{
    Hold, attempt_hold, attempt_recovery_wait, completed_without_changes, graph_completion_wait,
    holding_executions, launch_retry_wait, owner_retry_wait, recorded_hold,
    show_no_change_settlement, task_hold,
};
#[cfg(feature = "delivery_playbook")]
pub use holds::{delivery_hold, result_waiting};
mod readiness;
pub use readiness::{TaskDependencies, readiness};
mod changes;
pub use changes::{changes, predecessor};
>>>>>>> refs/remotes/central/main
mod task_edit;
pub use task_edit::{
    DependencyAdd, DependencyRemove, TaskEdit, TaskReprioritize, TaskRoleSet, TaskRouteClear,
    TaskRouteSet,
};

pub const STORY_POINTS_KEY: &str = "aq:story_points";
pub const MAX_STORY_POINTS: u16 = 1000;

/// The estimate stored on a Graph task. Legacy tasks may have no estimate.
pub fn story_points(node: &Value) -> Option<u16> {
    let points = node["metadata"][STORY_POINTS_KEY].as_u64()?;
    u16::try_from(points)
        .ok()
        .filter(|n| (1..=MAX_STORY_POINTS).contains(n))
}

#[cfg(test)]
mod tests;

mod execution;
pub use execution::{
    Execution, TaskActivate, TaskActivateSubtree, TaskComplete, TaskDelete, TaskReopen, TaskRetry,
};

mod downstream;
pub use downstream::blocked_by;

#[cfg(test)]
mod delivery_tests {
    #[test]
    fn a_waiting_result_names_the_missing_delivery_policy() {
        let (a, r) = (uuid::Uuid::from_u128(1), uuid::Uuid::from_u128(2));
        let none = super::result_waiting(a, r, false);
        assert_eq!(none["code"], "result_ready_no_delivery_policy");
        assert_eq!(none["result_id"], r.to_string());
        assert!(
            none["message"]
                .as_str()
                .unwrap()
                .starts_with("Result ready, no delivery policy")
        );
        assert_eq!(super::result_waiting(a, r, true)["code"], "result_ready");
    }
}
