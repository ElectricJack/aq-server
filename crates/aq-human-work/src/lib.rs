//! Work-view projections for AQ's owner Graph reads (`human-graph-1`, G11
//! X8-X13): task-state counts, edge projection, readiness with its blockers
//! and hold reasons, and a change summary for each task history entry.
//!
//! The all-project task list joins Graph's page to AQ's current owner and project bindings.
pub mod attribution;
#[cfg(feature = "delivery_playbook")]
pub mod delivery;
pub mod epic_retirement;
pub mod history;
pub mod knowledge_actions;
pub mod projection;
pub mod subtree_activation;
pub mod task_git_author;
pub mod task_graph;
pub mod task_list;
use serde_json::{Map, Value, json};
use sqlx::{PgPool, Row};
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
            if seen.insert(n["id"].as_str()?.to_owned())
                && n["kind"] == "task"
                && n["deleted"] == false
            {
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
        if e["edge_type"] != "blocks" || e["deleted"] != false || e["target"] != json!(task) {
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
    _pool: &PgPool,
    _project: Uuid,
    _task: Uuid,
    current: Option<&aq_machine_control::holds::Current>,
) -> Option<Value> {
    let h = current?;
    let phrase = aq_machine_control::holds::phrase(&h.reason);
    let shown = task_hold(h.attempt_id, &h.status, &h.reason, &phrase);
    #[cfg(feature = "delivery_playbook")]
    if aq_machine_control::holds::is_budget_hold(&h.reason)
        && let Some(evidence) =
            delivery::budget_evidence(_pool, _project, _task, h.attempt_id).await
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
#[cfg(feature = "delivery_playbook")]
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
#[cfg(feature = "delivery_playbook")]
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

mod task_edit;
pub use task_edit::{TaskEdit, TaskReprioritize};

pub const STORY_POINTS_KEY: &str = "aq:story_points";
pub const MAX_STORY_POINTS: u16 = 1000;

/// The estimate stored on a Graph task. Legacy tasks may have no estimate.
pub fn story_points(node: &Value) -> Option<u16> {
    let points = node["metadata"][STORY_POINTS_KEY].as_u64()?;
    u16::try_from(points)
        .ok()
        .filter(|n| (1..=MAX_STORY_POINTS).contains(n))
}

/// Revision fields a history diff names, in display order. `priority` is the
/// `aq:priority` metadata key and `role` the `aq:role` key (spec 0014 §5.5);
/// `metadata` stands for any other metadata key.
const FIELDS: [&str; 17] = [
    "title",
    "body",
    "acceptance_criteria",
    "labels",
    "priority",
    "role",
    "metadata",
    "task_state",
    "provenance",
    "content_type",
    "retired",
    "retirement_reason",
    "successor_id",
    "valid_from",
    "valid_until",
    "review_at",
    "deleted",
];

/// The task's node snapshot that a revision entry carries, with its revision.
fn snapshot(entry: &Value, task: Uuid) -> Option<(&Value, u64)> {
    let d = &entry["data"];
    if entry["category"] != "revision" || d["id"] != json!(task) {
        return None;
    }
    Some((d, d["content_revision"].as_u64().filter(|n| *n > 0)?))
}

/// The content revision the oldest revision entry on a history page needs as
/// its predecessor, when the page does not hold that snapshot.
pub fn predecessor(items: &[Value], task: Uuid) -> Option<u64> {
    let held: Vec<u64> = items
        .iter()
        .filter_map(|e| snapshot(e, task).map(|s| s.1))
        .collect();
    items
        .iter()
        .filter(|e| e["operation"] != "created")
        .filter_map(|e| snapshot(e, task).map(|s| s.1))
        .filter(|r| *r > 1 && !held.contains(&(r - 1)))
        .min()
        .map(|r| r - 1)
}

/// One change summary per history entry, in order, or null when an entry
/// carries nothing AQ can describe. `prior` is the node snapshot at the
/// revision [`predecessor`] named, when it could be read.
///
/// A change is `{summary, fields, edge, complete}`. `fields` lists changed
/// fields as `{field, from, to}`; values are given only for task state,
/// priority, role, content type, retired and deleted, and are null otherwise.
/// `edge` is `{edge_type, direction, node_id}` for a dependency or link
/// change. `complete` is false when the previous revision was unavailable.
pub fn changes(items: &[Value], task: Uuid, prior: Option<&Value>) -> Vec<Value> {
    items
        .iter()
        .map(|e| {
            let op = e["operation"].as_str().unwrap_or("");
            match e["category"].as_str() {
                Some("lifecycle") => lifecycle(op, &e["data"]),
                Some("edge") => linked(op, &e["data"], task),
                Some("revision") => revision(items, e, op, task, prior),
                _ => None,
            }
            .unwrap_or(Value::Null)
        })
        .collect()
}

fn summary(text: String) -> String {
    text.chars().take(MAX_SUMMARY).collect()
}
fn field(name: &str, from: Value, to: Value) -> Value {
    json!({"field":name,"from":from,"to":to})
}
fn change(text: String, fields: Vec<Value>, edge: Value, complete: bool) -> Option<Value> {
    Some(json!({"summary":summary(text),"fields":fields,"edge":edge,"complete":complete}))
}
/// A task state (or null) as shown in a change; None for anything else.
fn state(v: &Value) -> Option<Value> {
    match v {
        Value::Null => Some(Value::Null),
        _ => STATES[..4].iter().find(|s| v == **s).map(|s| json!(s)),
    }
}
fn shown(v: &Value) -> String {
    v.as_str().unwrap_or("none").to_owned()
}

fn lifecycle(op: &str, d: &Value) -> Option<Value> {
    match op {
        "created" | "transitioned" => {
            let (from, to) = (state(&d["from"])?, state(&d["to"])?);
            let text = if op == "created" {
                format!("created as {}", shown(&to))
            } else {
                format!("{} → {}", shown(&from), shown(&to))
            };
            change(text, vec![field("task_state", from, to)], Value::Null, true)
        }
        "deleted" if d["deleted"] == true => change(
            "deleted".into(),
            vec![field("deleted", json!("false"), json!("true"))],
            Value::Null,
            true,
        ),
        "restored" => {
            let from = d["from_revision"].as_u64().filter(|n| *n > 0)?;
            let to = state(&d["task_state"])?;
            change(
                format!("restored revision {from}"),
                vec![field("task_state", Value::Null, to)],
                Value::Null,
                true,
            )
        }
        _ => None,
    }
}

fn linked(op: &str, d: &Value, task: Uuid) -> Option<Value> {
    let verb = matches!(op, "added" | "removed").then_some(op)?;
    let kind = d["edge_type"].as_str().filter(|t| {
        !t.is_empty() && t.len() <= 64 && t.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
    })?;
    let id = |k: &str| d[k].as_str()?.parse::<Uuid>().ok();
    let (source, target) = (id("source")?, id("target")?);
    let (direction, other, word) = if target == task {
        ("incoming", source, "from")
    } else if source == task {
        ("outgoing", target, "to")
    } else {
        return None;
    };
    let short = &other.to_string()[..8];
    change(
        format!("{verb} {kind} {word} {short}"),
        vec![],
        json!({"edge_type":kind,"direction":direction,"node_id":other}),
        true,
    )
}

fn revision(
    items: &[Value],
    e: &Value,
    op: &str,
    task: Uuid,
    prior: Option<&Value>,
) -> Option<Value> {
    let (after, r) = snapshot(e, task)?;
    if op == "created" {
        return change("created".into(), vec![], Value::Null, true);
    }
    let previous = items
        .iter()
        .filter_map(|i| snapshot(i, task))
        .find(|(_, n)| *n + 1 == r)
        .map(|(s, _)| s)
        .or(prior
            .filter(|p| p["id"] == json!(task) && p["content_revision"].as_u64() == Some(r - 1)));
    let label = op.replace('_', " ");
    let Some(before) = previous else {
        return change(
            format!("{label}; earlier revision unavailable"),
            vec![],
            Value::Null,
            false,
        );
    };
    let fields = diff(before, after);
    let text = if fields.is_empty() {
        label
    } else {
        fields
            .iter()
            .map(|f| {
                let name = f["field"].as_str().unwrap_or("").replace('_', " ");
                match (&f["from"], &f["to"]) {
                    (Value::Null, Value::Null) => name,
                    (a, b) => format!("{name} {} → {}", shown(a), shown(b)),
                }
            })
            .collect::<Vec<_>>()
            .join("; ")
    };
    change(text, fields, Value::Null, true)
}

/// Changed fields between two node snapshots of the same task.
fn diff(before: &Value, after: &Value) -> Vec<Value> {
    let priority = |n: &Value| n["metadata"]["aq:priority"].clone();
    let role = |n: &Value| n["metadata"]["aq:role"].clone();
    let other = |n: &Value| {
        let mut m = n["metadata"].as_object().cloned().unwrap_or_default();
        m.remove("aq:priority");
        m.remove("aq:role");
        m
    };
    // A role id; absent (the project's default applies) is shown as none.
    let role_text = |v: &Value| match v.as_str() {
        Some(s)
            if !s.is_empty()
                && s.len() <= 64
                && s.bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-') =>
        {
            json!(s)
        }
        _ => Value::Null,
    };
    let text = |v: &Value| match v {
        Value::Bool(b) => json!(b.to_string()),
        Value::Number(n) if n.is_i64() => json!(n.to_string()),
        Value::String(s)
            if !s.is_empty()
                && s.len() <= 64
                && s.bytes().all(|b| b.is_ascii_lowercase() || b == b'_') =>
        {
            json!(s)
        }
        _ => Value::Null,
    };
    let mut out = Vec::new();
    for name in FIELDS {
        let (a, b) = match name {
            "priority" => (priority(before), priority(after)),
            "role" => (role(before), role(after)),
            "metadata" => (Value::Object(other(before)), Value::Object(other(after))),
            _ => (before[name].clone(), after[name].clone()),
        };
        if a == b {
            continue;
        }
        if name == "role" {
            out.push(field(name, role_text(&a), role_text(&b)));
        } else if matches!(
            name,
            "task_state" | "priority" | "content_type" | "retired" | "deleted"
        ) {
            out.push(field(name, text(&a), text(&b)));
        } else {
            out.push(field(name, Value::Null, Value::Null));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(task: Uuid, revision: u64, priority: i64, title: &str) -> Value {
        json!({"id":task,"content_revision":revision,"title":title,"body":"b","labels":[],
            "metadata":{"aq:priority":priority,"aq:project_id":"p"},"task_state":"active",
            "verification":"unverified","retired":false,"deleted":false})
    }
    fn entry(category: &str, operation: &str, data: Value) -> Value {
        json!({"category":category,"operation":operation,"data":data})
    }
    fn authority(status: &str, stop: &str, cancellation: Value) -> Value {
        json!({"status":status,"stop":{"kind":stop},"cancellation_id":cancellation})
    }

    fn t(id: Uuid, state: &str) -> Value {
        json!({"id":id,"kind":"task","title":format!("T {id}"),"task_state":state,"retired":false,"deleted":false})
    }
    fn pc(source: Uuid, target: Uuid) -> Value {
        json!({"id":Uuid::new_v4(),"edge_type":"parent-child","source":source,"target":target,"deleted":false})
    }
    fn ctx(nodes: Vec<Value>, edges: Vec<Value>, truncated: bool) -> Value {
        json!({"nodes":nodes,"edges":edges,"truncated":truncated})
    }
    fn codes(h: &Hierarchy) -> Vec<(String, u64)> {
        h.blocking
            .iter()
            .map(|b| {
                (
                    b["code"].as_str().unwrap().into(),
                    b["level"].as_u64().unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn a_draft_or_retired_ancestor_within_three_levels_blocks() {
        let (task, parent, grand) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let edges = vec![pc(parent, task), pc(grand, parent)];
        let run = |p: Value, g: Value| {
            let c = ctx(vec![p, g], edges.clone(), false);
            hierarchy(&c, Some(&c), task, 2).unwrap()
        };
        let h = run(t(parent, "active"), t(grand, "active"));
        assert!(h.blocking.is_empty() && h.complete);
        let h = run(t(parent, "draft"), t(grand, "active"));
        assert_eq!(codes(&h), [("ancestor_draft".into(), 1)]);
        assert_eq!(h.blocking[0]["id"], json!(parent));
        assert_eq!(h.blocking[0]["edge_id"], edges[0]["id"]);
        let h = run(t(parent, "active"), t(grand, "draft"));
        assert_eq!(codes(&h), [("ancestor_draft".into(), 2)]);
        let mut retired = t(grand, "active");
        retired["retired"] = json!(true);
        let h = run(t(parent, "draft"), retired);
        assert_eq!(
            codes(&h),
            [("ancestor_draft".into(), 1), ("ancestor_retired".into(), 2)]
        );
        let h = run(t(parent, "completed"), t(grand, "canceled"));
        assert!(h.blocking.is_empty());
    }

    #[test]
    fn live_children_make_a_container_but_deleted_or_retired_ones_do_not() {
        let (task, a, b, c) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        let mut gone = t(b, "active");
        gone["deleted"] = json!(true);
        let mut old = t(c, "active");
        old["retired"] = json!(true);
        let edges = vec![pc(task, a), pc(task, b), pc(task, c)];
        let ctx1 = ctx(vec![t(a, "completed"), gone, old], edges, false);
        let h = hierarchy(&ctx1, None, task, 2).unwrap();
        assert_eq!(codes(&h), [("has_live_children".into(), 1)]);
        assert_eq!(h.blocking[0]["relationship"], "child");
        assert_eq!(h.blocking[0]["task_state"], "completed");
        let leaf = ctx(vec![t(b, "active")], vec![], false);
        let h = hierarchy(&leaf, None, task, 2).unwrap();
        assert!(h.blocking.is_empty() && h.complete);
    }

    #[test]
    fn missing_or_truncated_ancestry_is_incomplete_not_ready() {
        let (task, parent) = (Uuid::new_v4(), Uuid::new_v4());
        let edges = vec![pc(parent, task)];
        // A parent edge needs the ancestry read.
        let c = ctx(vec![t(parent, "active")], edges.clone(), false);
        assert!(needs_ancestry(&c, task));
        assert!(!hierarchy(&c, None, task, 2).unwrap().complete);
        assert!(hierarchy(&c, Some(&c), task, 2).unwrap().complete);
        // The parent node is not in either Context.
        let c = ctx(vec![], edges, false);
        assert!(!hierarchy(&c, Some(&c), task, 2).unwrap().complete);
        // A truncated Context.
        let c = ctx(vec![], vec![], true);
        assert!(needs_ancestry(&c, task));
        assert!(!hierarchy(&c, Some(&c), task, 2).unwrap().complete);
        let c = ctx(vec![], vec![], false);
        assert!(!needs_ancestry(&c, task));
        assert!(hierarchy(&c, None, task, 2).unwrap().complete);
        assert!(hierarchy(&json!({}), None, task, 2).is_none());
    }

    #[test]
    fn hierarchy_makes_an_active_task_wait_and_only_then() {
        let h = vec![json!({"code":"ancestor_draft"})];
        let r = readiness_in(Some("active"), None, vec![], vec![], h.clone(), true);
        assert_eq!(r["state"], "waiting");
        assert_eq!(r["hierarchy"], json!(h));
        // Truncated and nothing known: unknown. A relationship is waiting.
        let r = readiness_in(Some("active"), None, vec![], vec![], vec![], false);
        assert_eq!(r["state"], "unknown");
        assert!(r.get("hierarchy").is_none());
        let r = readiness_in(Some("active"), None, vec![], vec![], h.clone(), false);
        assert_eq!(r["state"], "waiting");
        // Terminal and draft tasks keep their own state and show no hierarchy.
        for s in ["draft", "completed", "canceled"] {
            let r = readiness_in(Some(s), None, vec![], vec![], h.clone(), true);
            assert_eq!(r["state"], s);
            assert!(r.get("hierarchy").is_none());
        }
        // A running attempt outranks it.
        let live = Hold::from_authority(
            Uuid::new_v4(),
            &authority("granted", "running", Value::Null),
        );
        let r = readiness_in(Some("active"), Some(&live), vec![], vec![], h, true);
        assert_eq!(r["state"], "running");
    }

    #[test]
    fn a_stopped_holding_attempt_is_held_not_running() {
        let a = Uuid::new_v4();
        let live = Hold::from_authority(a, &authority("granted", "running", Value::Null));
        let r = readiness(Some("active"), Some(&live), None, vec![], true);
        assert_eq!(r["state"], "running");
        assert_eq!(r["reasons"], json!([]));

        // QA P1-01: the MS1 cancel drill's attempt finished with stop proof.
        let cancelled =
            Hold::from_authority(a, &authority("finished", "stopped", json!(Uuid::new_v4())));
        let r = readiness(Some("active"), Some(&cancelled), None, vec![], true);
        assert_eq!(r["state"], "held");
        assert_eq!(r["reasons"][0]["code"], "cancelled_attempt_holds_task");
        assert_eq!(r["reasons"][0]["attempt_id"], json!(a));

        let finished = Hold::from_authority(a, &authority("finished", "stopped", Value::Null));
        let r = readiness(Some("active"), Some(&finished), None, vec![], true);
        assert_eq!(r["reasons"][0]["code"], "attempt_stopped_not_released");

        let unproven = Hold::from_authority(a, &authority("finished", "unknown", Value::Null));
        let r = readiness(Some("active"), Some(&unproven), None, vec![], true);
        assert_eq!(r["state"], "held");
        assert_eq!(r["reasons"][0]["code"], "attempt_finished_stop_unconfirmed");

        // A cancelling attempt that has not stopped still runs.
        let cancelling = Hold::from_authority(
            a,
            &authority("cancelling", "running", json!(Uuid::new_v4())),
        );
        assert_eq!(
            readiness(None, Some(&cancelling), None, vec![], true)["state"],
            "running"
        );
        // The task's own terminal state wins; the hold is still explained.
        let r = readiness(Some("canceled"), Some(&cancelled), None, vec![], true);
        assert_eq!(r["state"], "canceled");
        assert_eq!(r["reasons"][0]["code"], "cancelled_attempt_holds_task");
    }

    #[test]
    fn unheld_readiness_keeps_its_order() {
        let open = vec![json!({"satisfied":false})];
        assert_eq!(
            readiness(Some("active"), None, None, open, true)["state"],
            "waiting"
        );
        let done = vec![json!({"satisfied":true})];
        assert_eq!(
            readiness(Some("active"), None, None, done.clone(), false)["state"],
            "unknown"
        );
        assert_eq!(
            readiness(Some("active"), None, None, done, true)["state"],
            "ready"
        );
        assert_eq!(
            readiness(Some("draft"), None, None, vec![], true)["state"],
            "draft"
        );
    }

    #[test]
    fn a_task_hold_makes_an_active_task_held_with_its_reason() {
        let a = Uuid::new_v4();
        let held = || {
            Some(task_hold(
                a,
                "finished",
                "lineage_retry_budget_exhausted",
                "its retry budget is exhausted",
            ))
        };
        let r = readiness(Some("active"), None, held(), vec![], true);
        assert_eq!(r["state"], "held");
        assert_eq!(r["reasons"][0]["code"], "task_held");
        assert_eq!(
            r["reasons"][0]["hold_reason"],
            "lineage_retry_budget_exhausted"
        );
        assert_eq!(r["reasons"][0]["attempt_id"], json!(a));
        assert!(
            r["reasons"][0]["message"]
                .as_str()
                .unwrap()
                .contains("its retry budget is exhausted")
        );
        // A retired or completed task is not held, and no hold leaves it ready.
        let r = readiness(Some("canceled"), None, held(), vec![], true);
        assert_eq!(
            (r["state"].as_str(), r["reasons"].as_array().unwrap().len()),
            (Some("canceled"), 0)
        );
        assert_eq!(
            readiness(Some("active"), None, None, vec![], true)["state"],
            "ready"
        );
        // A stopped attempt's hold is listed first.
        let stopped = Hold::from_authority(a, &authority("finished", "stopped", Value::Null));
        let r = readiness(Some("active"), Some(&stopped), held(), vec![], true);
        assert_eq!(r["reasons"][0]["code"], "attempt_stopped_not_released");
        assert_eq!(r["reasons"][1]["code"], "task_held");
    }

    #[test]
    fn blockers_read_live_incoming_blocks_edges() {
        let (t, b, gone) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let ctx = json!({"nodes":[{"id":b,"deleted":false,"title":"First","task_state":"completed"}],
            "edges":[{"id":Uuid::new_v4(),"edge_type":"blocks","deleted":false,"source":b,"target":t},
                {"id":Uuid::new_v4(),"edge_type":"blocks","deleted":false,"source":gone,"target":t},
                {"id":Uuid::new_v4(),"edge_type":"blocks","deleted":true,"source":b,"target":t},
                {"id":Uuid::new_v4(),"edge_type":"references","deleted":false,"source":b,"target":t}]});
        let found = blockers(&ctx, t).unwrap();
        assert_eq!(found.len(), 2);
        assert_eq!(found[0]["satisfied"], true);
        assert_eq!(found[1]["satisfied"], Value::Null);
        assert!(blockers(&json!({}), t).is_none());
    }

    #[test]
    fn lifecycle_entries_say_from_and_to() {
        let t = Uuid::new_v4();
        let items = [
            entry(
                "lifecycle",
                "created",
                json!({"from":null,"to":"active","version":1}),
            ),
            entry(
                "lifecycle",
                "transitioned",
                json!({"from":"active","to":"completed","version":2}),
            ),
            entry(
                "lifecycle",
                "transitioned",
                json!({"from":"active","to":"<b>x</b>"}),
            ),
            entry("lifecycle", "deleted", json!({"deleted":true,"version":3})),
        ];
        let c = changes(&items, t, None);
        assert_eq!(c[0]["summary"], "created as active");
        assert_eq!(c[1]["summary"], "active → completed");
        assert_eq!(
            c[1]["fields"],
            json!([{"field":"task_state","from":"active","to":"completed"}])
        );
        assert_eq!(c[2], Value::Null, "unknown states are never echoed");
        assert_eq!(c[3]["summary"], "deleted");
    }

    #[test]
    fn parent_reads_the_live_parent_edge_and_defers_node_validation() {
        let (top, child) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let edge = json!({"id":"e","edge_type":"parent-child","deleted":false,"source":top,"target":child});
        let nodes =
            vec![json!({"id":top,"kind":"task","title":"t","task_state":"draft","deleted":false})];
        let pass = |n: &Value, _: &str| Some(n.clone());
        let found = parent(&ctx(nodes.clone(), vec![edge.clone()], false), child, pass);
        assert_eq!(found.unwrap()["task_state"], "draft");
        assert_eq!(
            parent(
                &ctx(nodes.clone(), vec![edge.clone()], false),
                child,
                |_, _| None
            ),
            None
        );
        assert_eq!(
            parent(&ctx(nodes, vec![edge], false), top, pass),
            Some(Value::Null)
        );
        let gone = json!({"id":"e","edge_type":"parent-child","deleted":false,"source":top,"target":child});
        let unlisted = parent(&ctx(vec![], vec![gone], false), child, pass).unwrap();
        assert_eq!(
            (unlisted["id"].clone(), unlisted["kind"].clone()),
            (json!(top), Value::Null)
        );
    }

    #[test]
    fn edge_entries_name_the_other_task_and_direction() {
        let (t, b) = (Uuid::new_v4(), Uuid::new_v4());
        let e = |op: &str, s: Uuid, d: Uuid| {
            entry(
                "edge",
                op,
                json!({"id":Uuid::new_v4(),"edge_type":"blocks","source":s,"target":d}),
            )
        };
        let c = changes(&[e("added", b, t), e("removed", t, b)], t, None);
        assert_eq!(
            c[0]["edge"],
            json!({"edge_type":"blocks","direction":"incoming","node_id":b})
        );
        assert_eq!(
            c[0]["summary"],
            format!("added blocks from {}", &b.to_string()[..8])
        );
        assert_eq!(c[1]["edge"]["direction"], "outgoing");
        assert!(
            c[1]["summary"]
                .as_str()
                .unwrap()
                .starts_with("removed blocks to ")
        );
    }

    #[test]
    fn a_role_change_is_named_in_the_history_with_its_values() {
        let t = Uuid::new_v4();
        let mut named = node(t, 2, 0, "Task");
        named["metadata"]["aq:role"] = json!("reviewer");
        let mut cleared = node(t, 3, 0, "Task");
        cleared["content_revision"] = json!(3);
        let items = [
            entry("revision", "created", node(t, 1, 0, "Task")),
            entry("revision", "updated", named),
            entry("revision", "updated", cleared),
        ];
        let c = changes(&items, t, None);
        assert_eq!(
            c[1]["fields"],
            json!([{"field":"role","from":null,"to":"reviewer"}])
        );
        assert_eq!(c[1]["summary"], "role none → reviewer");
        assert_eq!(c[2]["summary"], "role reviewer → none");
    }

    #[test]
    fn a_role_hold_makes_an_active_task_held_and_is_listed_after_its_other_hold() {
        let role = json!({"code":"role_disabled","role":"reviewer","message":"m"});
        let r = readiness_with(Some("active"), None, vec![role.clone()], vec![], true);
        assert_eq!(
            (r["state"].as_str(), &r["reasons"]),
            (Some("held"), &json!([role]))
        );
        let a = Uuid::new_v4();
        let recorded = task_hold(a, "finished", "lineage_retry_budget_exhausted", "x");
        let both = vec![recorded.clone(), role.clone()];
        let r = readiness_with(Some("active"), None, both.clone(), vec![], true);
        assert_eq!(r["reasons"], json!([recorded, role]));
        // Only an active task is held; a draft or closed task keeps its state.
        for state in ["draft", "completed", "canceled"] {
            let r = readiness_with(Some(state), None, both.clone(), vec![], true);
            assert_eq!(
                (r["state"].as_str(), &r["reasons"]),
                (Some(state), &json!([]))
            );
        }
    }

    #[test]
    fn revision_entries_diff_against_the_previous_revision() {
        let t = Uuid::new_v4();
        let mut edited = node(t, 3, 5, "Renamed");
        edited["metadata"]["aq:project_id"] = json!("q");
        let mut linked = edited.clone();
        linked["content_revision"] = json!(4);
        let items = [
            entry("revision", "updated", node(t, 2, 0, "Original")),
            entry("revision", "updated", edited),
            entry("revision", "links_changed", linked),
        ];
        assert_eq!(predecessor(&items, t), Some(1));
        let c = changes(&items, t, None);
        assert_eq!(c[0]["complete"], false, "revision 1 was not read");
        assert_eq!(c[0]["summary"], "updated; earlier revision unavailable");
        assert_eq!(
            c[1]["fields"],
            json!([{"field":"title","from":null,"to":null},
                {"field":"priority","from":"0","to":"5"},
                {"field":"metadata","from":null,"to":null}])
        );
        assert_eq!(c[1]["summary"], "title; priority 0 → 5; metadata");
        assert_eq!(c[2]["summary"], "links changed");
        assert_eq!(c[2]["fields"], json!([]));

        let prior = node(t, 1, 0, "Original");
        let c = changes(&items, t, Some(&prior));
        assert_eq!(c[0]["complete"], true);
        assert_eq!(c[0]["fields"], json!([]));
        let created = [entry("revision", "created", node(t, 1, 0, "New"))];
        assert_eq!(predecessor(&created, t), None);
        assert_eq!(changes(&created, t, None)[0]["summary"], "created");
    }

    #[test]
    fn unrecognized_payloads_never_escape() {
        let t = Uuid::new_v4();
        let items = [entry(
            "lifecycle",
            "set_task_state",
            json!({"secret":"must-not-escape"}),
        )];
        assert_eq!(changes(&items, t, None), vec![Value::Null]);
        let mut a = node(t, 1, 0, "A");
        let mut b = node(t, 2, 0, "B");
        a["content_type"] = json!("note");
        b["content_type"] = json!("Secret Text");
        let c = changes(&[entry("revision", "updated", b)], t, Some(&a));
        let fields = c[0]["fields"].as_array().unwrap();
        assert!(fields.contains(&json!({"field":"content_type","from":"note","to":null})));
        assert!(!c[0].to_string().contains("Secret"));
    }

    #[test]
    fn descendants_count_each_live_task_once_and_mark_truncation() {
        let (w, root, a, b) = (
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
            Uuid::new_v4(),
        );
        let n = |id: Uuid, s: &str| json!({"id":id,"workspace_id":w,"kind":"task","deleted":false,"task_state":s});
        let leaf = |id: Uuid, s: &str| json!({"node":n(id, s),"children":[],"truncated":false});
        let tree = json!({"node":n(root,"active"),"truncated":false,"children":[
            leaf(a, "completed"), {"node":n(b,"active"),"truncated":true,"children":[leaf(a, "completed")]}]});
        let (counts, total, complete) = descendants(&tree, w, root).unwrap();
        assert_eq!(
            (counts["completed"].clone(), total, complete),
            (json!("1"), 2, false)
        );
        assert!(descendants(&tree, Uuid::new_v4(), root).is_none());
    }

    #[test]
    fn counts_and_edges_use_decimal_wire_forms() {
        let mut m = buckets();
        count(&mut m, &json!("completed"));
        count(&mut m, &json!("weird"));
        let m = decimal(m);
        assert_eq!(m["completed"], "1");
        assert_eq!(m["unknown"], "1");
        let e = json!({"id":"e","source":"s","target":"t","edge_type":"blocks","description":null,
            "version":2,"source_revision":null,"target_revision":3});
        let p = edge(&e).unwrap();
        assert_eq!(p["version"], "2");
        assert_eq!(p["source_revision"], Value::Null);
        assert_eq!(p["target_revision"], "3");
        assert!(edge(&json!({"version":0})).is_none());
    }
}

mod execution;
pub use execution::{
    Execution, TaskActivate, TaskActivateSubtree, TaskComplete, TaskDelete, TaskReopen, TaskRetry,
};

mod downstream;
pub use downstream::blocked_by;

#[cfg(all(test, feature = "delivery_playbook"))]
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
