//! Resolve an agent's draft-task scope from a validated Graph context snapshot.
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use uuid::Uuid;

pub fn uuid(value: &Value) -> Option<Uuid> {
    value
        .as_str()?
        .parse::<Uuid>()
        .ok()
        .filter(|id| !id.is_nil())
}

/// Graph allows three structural levels including the root.
pub const MAX_ANCESTRY: u32 = 2;

pub struct TaskScope {
    pub workspace: Uuid,
    pub project: Uuid,
    pub own_task: Uuid,
    pub subject_task: Option<Uuid>,
}

pub struct Path {
    pub root: Uuid,
    pub depth: u32,
    /// Every structural node whose observed version must hold at mutation.
    pub versions: BTreeMap<Uuid, u64>,
}

impl TaskScope {
    pub fn path(&self, context: &Value, draft: bool) -> Option<Path> {
        let edges = context["edges"].as_array()?;
        let nodes = context["nodes"].as_array()?;
        let mut node = &context["node"];
        if draft && node["task_state"] != "draft" {
            return None;
        }
        let mut versions = BTreeMap::new();
        for depth in 0..=MAX_ANCESTRY {
            let task = uuid(&node["id"])?;
            if node["workspace_id"] != json!(self.workspace)
                || node["kind"] != "task"
                || node["deleted"] != false
                || node["retired"] != false
                || node["metadata"]["aq:project_id"] != json!(self.project)
                || versions.insert(task, node["version"].as_u64()?).is_some()
            {
                return None;
            }
            if task == self.own_task || self.subject_task == Some(task) {
                return Some(Path {
                    root: task,
                    depth,
                    versions,
                });
            }
            let parent = edges.iter().find_map(|edge| {
                self.incoming(edge, task, "parent-child")
                    .then(|| uuid(&edge["source"]))
                    .flatten()
            })?;
            node = nodes.iter().find(|n| n["id"] == json!(parent))?;
        }
        None
    }

    fn incoming(&self, edge: &Value, task: Uuid, kind: &str) -> bool {
        edge["edge_type"] == kind
            && edge["workspace_id"] == json!(self.workspace)
            && edge["deleted"] == false
            && edge["target"] == json!(task)
    }

    /// Resolve the source of an existing blocks edge incident to the subject.
    pub fn blocker(&self, context: &Value, task: Uuid, edge_id: Uuid) -> Option<Uuid> {
        let edge = context["edges"]
            .as_array()?
            .iter()
            .find(|edge| edge["id"] == json!(edge_id) && self.incoming(edge, task, "blocks"))?;
        uuid(&edge["source"])
    }
}

/// Return each new task's existing ancestor and the path length to it.
/// A cycle, an absent parent or a duplicate node makes the batch invalid.
pub fn batch_paths(tasks: &[(Uuid, Option<Uuid>)]) -> Option<BTreeMap<Uuid, (Uuid, u32)>> {
    let parents: BTreeMap<_, _> = tasks.iter().copied().collect();
    if parents.len() != tasks.len() {
        return None;
    }
    let mut paths = BTreeMap::new();
    for (task, parent) in tasks {
        let mut next = (*parent)?;
        let mut chain = BTreeSet::from([*task]);
        while let Some(parent) = parents.get(&next) {
            if !chain.insert(next) {
                return None;
            }
            next = (*parent)?;
        }
        paths.insert(*task, (next, chain.len() as u32));
    }
    Some(paths)
}
