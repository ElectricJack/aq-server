//! Version-guarded Graph mutations for activating a task's draft descendants.
use serde_json::{Value, json};
use std::collections::BTreeSet;
use uuid::Uuid;

// Graph's MutationRequest accepts at most 100 operations. A guard on every
// node, including the root, makes changes to structural edges conflict.
pub const MAX_NODES: usize = 100;

#[derive(Debug, PartialEq, Eq)]
pub enum PlanError {
    Stale,
    Incomplete,
    Invalid,
}

impl PlanError {
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Stale => "stale_subject",
            Self::Incomplete => "subtree_too_large",
            Self::Invalid => "invalid_subtree",
        }
    }
}

pub fn activate_task(id: Uuid, version: u64) -> Value {
    update(id, version, json!({"task_state": "active"}))
}

fn update(id: Uuid, version: u64, patch: Value) -> Value {
    json!({"op": "update_node", "id": id, "expected_version": version, "patch": patch})
}

/// Plan one atomic Graph batch. A parent-child change checkpoints both ends,
/// so a no-op patch checks their revisions without modifying skipped tasks.
pub fn operations(
    tree: &Value,
    workspace: Uuid,
    root: Uuid,
    expected_root: u64,
) -> Result<Vec<Value>, PlanError> {
    if tree["node"]["id"] != json!(root) {
        return Err(PlanError::Invalid);
    }
    if tree["node"]["version"] != expected_root {
        return Err(PlanError::Stale);
    }
    let mut seen = BTreeSet::new();
    let mut todo = vec![tree];
    let mut operations = Vec::new();
    while let Some(branch) = todo.pop() {
        if branch["truncated"] != false || operations.len() == MAX_NODES {
            return Err(PlanError::Incomplete);
        }
        operations.push(node_operation(&branch["node"], workspace, root, &mut seen)?);
        todo.extend(branch["children"].as_array().ok_or(PlanError::Invalid)?);
    }
    Ok(operations)
}

fn node_operation(
    node: &Value,
    workspace: Uuid,
    root: Uuid,
    seen: &mut BTreeSet<Uuid>,
) -> Result<Value, PlanError> {
    let id: Uuid = node["id"]
        .as_str()
        .and_then(|id| id.parse().ok())
        .ok_or(PlanError::Invalid)?;
    let version = node["version"]
        .as_u64()
        .filter(|v| (1..=i64::MAX as u64).contains(v))
        .ok_or(PlanError::Invalid)?;
    let live_task = node["workspace_id"] == json!(workspace)
        && node["kind"] == "task"
        && node["deleted"] == false;
    if !live_task || !seen.insert(id) {
        return Err(PlanError::Invalid);
    }
    Ok(if id != root && node["task_state"] == "draft" {
        activate_task(id, version)
    } else {
        update(id, version, json!({}))
    })
}

pub struct Receipt<'a> {
    pub activated: Vec<&'a Value>,
    pub skipped: usize,
}

/// Verify that Graph confirmed exactly this batch. Empty patches return the
/// same version they checked. The first operation guards the root.
pub fn receipt<'a>(operations: &[Value], results: &'a Value) -> Option<Receipt<'a>> {
    let results = results["results"].as_array()?;
    if results.is_empty() || results.len() != operations.len() {
        return None;
    }
    let mut activated = Vec::new();
    for (op, result) in operations.iter().zip(results) {
        let changes_state = op["patch"]["task_state"] == "active";
        confirm_result(op, result, changes_state)?;
        if changes_state {
            activated.push(result);
        }
    }
    Some(Receipt {
        skipped: results.len().checked_sub(activated.len() + 1)?,
        activated,
    })
}

fn confirm_result(op: &Value, result: &Value, changes_state: bool) -> Option<()> {
    let revision = op["expected_version"]
        .as_u64()?
        .checked_add(u64::from(changes_state))?;
    let same_task = result["id"] == op["id"] && result["kind"] == "task";
    let state_confirmed = !changes_state || result["task_state"] == "active";
    (same_task && state_confirmed && result["version"] == revision).then_some(())
}
