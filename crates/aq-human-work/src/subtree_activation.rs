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
        let node = &branch["node"];
        let id: Uuid = node["id"]
            .as_str()
            .and_then(|id| id.parse().ok())
            .ok_or(PlanError::Invalid)?;
        let version = node["version"]
            .as_u64()
            .filter(|v| (1..=i64::MAX as u64).contains(v))
            .ok_or(PlanError::Invalid)?;
        if node["workspace_id"] != json!(workspace)
            || node["kind"] != "task"
            || node["deleted"] != false
            || !seen.insert(id)
        {
            return Err(PlanError::Invalid);
        }
        operations.push(if id != root && node["task_state"] == "draft" {
            activate_task(id, version)
        } else {
            update(id, version, json!({}))
        });
        todo.extend(branch["children"].as_array().ok_or(PlanError::Invalid)?);
    }
    Ok(operations)
}

/// Verify that Graph confirmed exactly this batch and return the activated
/// records. Empty patches must return the same version they checked.
pub fn activated<'a>(operations: &[Value], results: &'a Value) -> Option<Vec<&'a Value>> {
    let results = results["results"].as_array()?;
    if results.len() != operations.len() {
        return None;
    }
    let mut activated = Vec::new();
    for (op, result) in operations.iter().zip(results) {
        let changes_state = op["patch"]["task_state"] == "active";
        let revision = op["expected_version"].as_u64()? + u64::from(changes_state);
        if result["id"] != op["id"]
            || result["kind"] != "task"
            || result["version"] != revision
            || (changes_state && result["task_state"] != "active")
        {
            return None;
        }
        if changes_state {
            activated.push(result);
        }
    }
    Some(activated)
}
