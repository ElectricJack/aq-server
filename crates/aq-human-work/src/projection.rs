//! Public Graph projections after transport and schema validation.
use crate::{buckets, count, decimal};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use uuid::Uuid;

#[derive(Clone, serde::Serialize)]
pub struct Project {
    pub id: Uuid,
    pub name: String,
    pub revision: String,
    pub workspace_id: Uuid,
}

#[derive(serde::Serialize)]
pub struct Envelope {
    contract: &'static str,
    project: Project,
    observed_at: String,
    pub data: Value,
}

impl Envelope {
    pub fn bounded(project: Project, observed_at_ms: i64, data: Value) -> Option<Self> {
        let reply = Self {
            contract: "human-graph-1",
            project,
            observed_at: chrono::DateTime::from_timestamp_millis(observed_at_ms)?.to_rfc3339(),
            data,
        };
        (serde_json::to_vec(&reply).ok()?.len() <= 1_048_576).then_some(reply)
    }
}

/// Project the explicitly public fields of a validated node.
pub fn node(raw: &Value, kind: &str) -> Option<Value> {
    let mut output = serde_json::Map::new();
    for key in [
        "id",
        "kind",
        "title",
        "body",
        "acceptance_criteria",
        "labels",
        "task_state",
        "verification",
        "content_type",
        "retired",
        "deleted",
        "created_at",
        "updated_at",
        "created_by",
        "provenance",
        "last_verified_by",
        "last_verified_at",
        "retirement_reason",
        "valid_from",
        "valid_until",
        "review_at",
        "successor_id",
        "summary",
    ] {
        output.insert(key.into(), raw[key].clone());
    }
    for key in ["version", "content_revision", "summary_of_revision"] {
        output.insert(
            key.into(),
            if raw[key].is_null() && key == "summary_of_revision" {
                Value::Null
            } else {
                json!(raw[key].as_u64().filter(|n| *n > 0)?.to_string())
            },
        );
    }
    if kind == "task" {
        output.insert("story_points".into(), json!(crate::story_points(raw)));
    }
    Some(Value::Object(output))
}

pub struct Neighborhood {
    pub id: Uuid,
    pub parent: Value,
    pub limit: u16,
}

/// Project the bounded Work neighborhood with the caller’s public node policy.
pub fn neighborhood(
    raw: &Value,
    view: Neighborhood,
    project: impl Fn(&Value) -> Option<Value>,
) -> Option<Value> {
    let root = project(&raw["node"])?;
    let upstream_truncated = raw["truncated"].as_bool()?;
    let mut nodes = VisibleNodes::new(root, view.id);
    for node in raw["nodes"].as_array()? {
        nodes.consider(node, view.limit, &project)?;
    }
    let (edges, hidden_edges) = nodes.edges(raw["edges"].as_array()?)?;
    let hidden_tasks: u64 = nodes.hidden_tasks.values().filter_map(Value::as_u64).sum();
    let shown = (nodes.shown.len() - 1) as u64;
    let expansion = if hidden_tasks > 0 && view.limit < 63 {
        json!({"limit": (shown + hidden_tasks).min(63).to_string()})
    } else {
        Value::Null
    };
    let truncated = upstream_truncated || !nodes.hidden.is_empty() || hidden_edges > 0;
    Some(
        json!({"root_id":view.id,"parent":view.parent,"nodes":nodes.shown,"edges":edges,"truncated":truncated,
        "omitted":{"tasks":hidden_tasks.to_string(),"tasks_by_state":decimal(nodes.hidden_tasks),
            "other_nodes":nodes.hidden_other.to_string(),"edges":hidden_edges.to_string(),"exact":!upstream_truncated},
        "expansion":expansion}),
    )
}

struct VisibleNodes {
    shown: Vec<Value>,
    ids: BTreeSet<String>,
    hidden: BTreeSet<String>,
    hidden_tasks: serde_json::Map<String, Value>,
    hidden_other: u64,
}

impl VisibleNodes {
    fn new(root: Value, id: Uuid) -> Self {
        Self {
            shown: vec![root],
            ids: BTreeSet::from([id.to_string()]),
            hidden: BTreeSet::new(),
            hidden_tasks: buckets(),
            hidden_other: 0,
        }
    }

    fn consider(
        &mut self,
        node: &Value,
        limit: u16,
        project: &impl Fn(&Value) -> Option<Value>,
    ) -> Option<()> {
        let id = node["id"].as_str()?;
        if self.ids.contains(id) || self.hidden.contains(id) {
            return Some(());
        }
        if node["kind"] != "task" || node["deleted"] != false {
            self.hidden_other += 1;
        } else if self.shown.len() > usize::from(limit) {
            count(&mut self.hidden_tasks, &node["task_state"]);
        } else {
            self.shown.push(project(node)?);
            self.ids.insert(id.to_owned());
            return Some(());
        }
        self.hidden.insert(id.to_owned());
        Some(())
    }

    fn edges(&self, raw: &[Value]) -> Option<(Vec<Value>, u64)> {
        let mut shown = Vec::new();
        for edge in raw {
            if shown.len() >= 256 {
                break;
            }
            if edge["deleted"] == false && self.contains_endpoints(edge)? {
                shown.push(crate::edge(edge)?);
            }
        }
        let hidden = (raw.len() - shown.len()) as u64;
        Some((shown, hidden))
    }

    fn contains_endpoints(&self, edge: &Value) -> Option<bool> {
        Some(
            self.ids.contains(edge["source"].as_str()?)
                && self.ids.contains(edge["target"].as_str()?),
        )
    }
}
