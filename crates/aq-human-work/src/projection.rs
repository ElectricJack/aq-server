//! Public Graph projections after transport and schema validation.
use crate::{buckets, count, decimal};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use uuid::Uuid;

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

/// Project the bounded Work neighborhood with the caller’s public node policy.
pub fn neighborhood(
    raw: &Value,
    id: Uuid,
    parent: Value,
    limit: u16,
    project: impl Fn(&Value) -> Option<Value>,
) -> Option<Value> {
    let root = project(&raw["node"])?;
    let upstream_truncated = raw["truncated"].as_bool()?;
    let mut truncated = upstream_truncated;
    let mut nodes = vec![root];
    let mut ids = BTreeSet::from([id.to_string()]);
    let mut hidden = BTreeSet::new();
    let mut hidden_tasks = buckets();
    let mut hidden_other = 0u64;
    for value in raw["nodes"].as_array()? {
        let vid = value["id"].as_str()?.to_owned();
        if ids.contains(&vid) || hidden.contains(&vid) {
            continue;
        }
        if value["kind"] != "task" || value["deleted"] != false {
            truncated = true;
            hidden.insert(vid);
            hidden_other += 1;
            continue;
        }
        if nodes.len() > usize::from(limit) {
            truncated = true;
            hidden.insert(vid);
            count(&mut hidden_tasks, &value["task_state"]);
            continue;
        }
        let n = project(value)?;
        ids.insert(vid);
        nodes.push(n)
    }
    let mut edges = Vec::new();
    let mut hidden_edges = 0u64;
    for edge in raw["edges"].as_array()? {
        if edges.len() >= 256
            || edge["deleted"] != false
            || !ids.contains(edge["source"].as_str()?)
            || !ids.contains(edge["target"].as_str()?)
        {
            truncated = true;
            hidden_edges += 1;
            continue;
        }
        edges.push(crate::edge(edge)?);
    }
    let hidden_task_count: u64 = hidden_tasks.values().filter_map(Value::as_u64).sum();
    let shown = (nodes.len() - 1) as u64;
    let expansion = if hidden_task_count > 0 && u64::from(limit) < 63 {
        json!({"limit": (shown + hidden_task_count).min(63).to_string()})
    } else {
        Value::Null
    };
    Some(
        json!({"root_id":id,"parent":parent,"nodes":nodes,"edges":edges,"truncated":truncated,
            "omitted":{"tasks":hidden_task_count.to_string(),
                "tasks_by_state":decimal(hidden_tasks),
                "other_nodes":hidden_other.to_string(),"edges":hidden_edges.to_string(),
                "exact":!upstream_truncated},
            "expansion":expansion}),
    )
}
