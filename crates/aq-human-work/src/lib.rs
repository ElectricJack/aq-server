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

mod hierarchy;
pub use hierarchy::{
    Hierarchy, MAX_HIERARCHY_CHILDREN, hierarchy, hierarchy_via, needs_ancestry, parent,
};
mod holds;
pub use holds::{
    Hold, attempt_hold, completed_without_changes, holding_executions, recorded_hold,
    show_no_change_settlement, task_hold,
};
#[cfg(feature = "delivery_playbook")]
pub use holds::{delivery_hold, result_waiting};
mod readiness;
pub use readiness::{TaskDependencies, readiness};
mod changes;
pub use changes::{changes, predecessor};
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

#[cfg(test)]
mod tests;

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
