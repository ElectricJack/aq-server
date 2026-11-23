//! Validate the graph records supporting a ready page or a task context.
use super::ReadyTask;
use aq_admission::TaskRevision;
use serde_json::Value;
use std::collections::BTreeSet;
use uuid::Uuid;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ObservationError {
    InvalidResponse,
    StaleTask,
}

type Result<T> = std::result::Result<T, ObservationError>;
use ObservationError::InvalidResponse;

fn id(value: &Value) -> Result<Uuid> {
    value
        .as_str()
        .and_then(|s| Uuid::parse_str(s).ok())
        .filter(|id| !id.is_nil())
        .ok_or(InvalidResponse)
}

fn version(value: &Value) -> Result<u64> {
    value
        .as_u64()
        .filter(|v| (1..=i64::MAX as u64).contains(v))
        .ok_or(InvalidResponse)
}

fn task_revision(node: &Value, workspace: Uuid) -> Result<TaskRevision> {
    if node["workspace_id"] != workspace.to_string() || node["kind"] != "task" {
        return Err(InvalidResponse);
    }
    let revision = TaskRevision {
        workspace_id: workspace,
        task_id: id(&node["id"])?,
        version: node["version"].as_u64().ok_or(InvalidResponse)?,
        content_revision: node["content_revision"].as_u64().ok_or(InvalidResponse)?,
    };
    revision.validate().map_err(|_| InvalidResponse)?;
    Ok(revision)
}

fn active(node: &Value) -> bool {
    let present = node["retired"] == false && node["deleted"] == false;
    present && node["task_state"] == "active"
}

pub fn ready_tasks(value: &Value, workspace: Uuid, limit: u16) -> Result<Vec<ReadyTask>> {
    let nodes = value["items"].as_array().ok_or(InvalidResponse)?;
    if nodes.len() > usize::from(limit) {
        return Err(InvalidResponse);
    }
    let mut tasks = Vec::with_capacity(nodes.len());
    let mut ids = BTreeSet::new();
    for node in nodes {
        let revision = task_revision(node, workspace)?;
        if !active(node) || !ids.insert(revision.task_id) {
            return Err(InvalidResponse);
        }
        tasks.push(ReadyTask {
            revision,
            node: node.clone(),
        });
    }
    Ok(tasks)
}

struct Context<'a> {
    nodes: &'a [Value],
    edges: &'a [Value],
    pins: &'a [Value],
}
impl<'a> Context<'a> {
    fn bounded(value: &'a Value, limit: u16) -> Result<Self> {
        let array = |key: &str| {
            value[key]
                .as_array()
                .map(Vec::as_slice)
                .ok_or(InvalidResponse)
        };
        let read = Self {
            nodes: array("nodes")?,
            edges: array("edges")?,
            pins: array("pinned_revisions")?,
        };
        if read.nodes.len() >= usize::from(limit) {
            return Err(InvalidResponse);
        }
        if read.edges.len() > 4 * usize::from(limit) || read.pins.len() > 2 * read.edges.len() {
            return Err(InvalidResponse);
        }
        Ok(read)
    }

    fn validate(&self, root: &TaskRevision) -> Result<()> {
        let workspace = root.workspace_id.to_string();
        let ids = self.node_ids(&workspace, root.task_id)?;
        let actual = self.record_pins(&workspace)?;
        let expected = self.edge_pins(&workspace, &ids)?;
        if expected != actual {
            return Err(InvalidResponse);
        }
        Ok(())
    }
    fn node_ids(&self, workspace: &str, root: Uuid) -> Result<BTreeSet<Uuid>> {
        let mut ids = BTreeSet::from([root]);
        for record in self.nodes {
            let (id, _) = record_revision(record, workspace)?;
            if !ids.insert(id) {
                return Err(InvalidResponse);
            }
        }
        Ok(ids)
    }
    fn record_pins(&self, workspace: &str) -> Result<BTreeSet<(Uuid, u64)>> {
        let mut actual = BTreeSet::new();
        for record in self.pins {
            if !actual.insert(record_revision(record, workspace)?) {
                return Err(InvalidResponse);
            }
        }
        Ok(actual)
    }
    fn edge_pins(&self, workspace: &str, ids: &BTreeSet<Uuid>) -> Result<BTreeSet<(Uuid, u64)>> {
        let mut edges = BTreeSet::new();
        let mut expected = BTreeSet::new();
        for edge in self.edges {
            let edge_id = validate_edge(edge, workspace)?;
            if !edges.insert(edge_id) {
                return Err(InvalidResponse);
            }
            record_endpoints(edge, ids, &mut expected)?;
        }
        Ok(expected)
    }
}

fn record_endpoints(
    edge: &Value,
    ids: &BTreeSet<Uuid>,
    expected: &mut BTreeSet<(Uuid, u64)>,
) -> Result<()> {
    for (node, pin) in [("source", "source_revision"), ("target", "target_revision")] {
        let endpoint = id(&edge[node])?;
        if !ids.contains(&endpoint) {
            return Err(InvalidResponse);
        }
        if !edge[pin].is_null() {
            expected.insert((endpoint, version(&edge[pin])?));
        }
    }
    Ok(())
}

fn record_revision(record: &Value, workspace: &str) -> Result<(Uuid, u64)> {
    let record_id = id(&record["id"])?;
    if record["workspace_id"] != workspace {
        return Err(InvalidResponse);
    }
    version(&record["version"])?;
    Ok((record_id, version(&record["content_revision"])?))
}

fn validate_edge(edge: &Value, workspace: &str) -> Result<Uuid> {
    let edge_id = id(&edge["id"])?;
    if edge["workspace_id"] != workspace || edge["deleted"] != false {
        return Err(InvalidResponse);
    }
    version(&edge["version"])?;
    Ok(edge_id)
}

pub fn context_revision(
    value: &Value,
    expected: &TaskRevision,
    limit: u16,
) -> Result<TaskRevision> {
    let node = &value["node"];
    let revision = task_revision(node, expected.workspace_id)?;
    if revision != *expected || !active(node) {
        return Err(ObservationError::StaleTask);
    }
    Context::bounded(value, limit)?.validate(expected)?;
    Ok(revision)
}
