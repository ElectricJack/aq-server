//! Graph mutation bodies for the host work adapters.
use crate::{Result, digest, host_values::text, stable_id};
use serde_json::{Map, Value, json};
use uuid::Uuid;

pub fn work_ensure_body(
    namespace: Uuid,
    project: Uuid,
    task: Uuid,
    input: &Value,
) -> Result<Value> {
    let kind = text(input, "kind")?;
    let key = text(input, "logical_key")?;
    let mut title = format!("{kind}: {key}");
    if title.len() > 512 {
        let mut end = 512;
        while !title.is_char_boundary(end) {
            end -= 1;
        }
        title.truncate(end);
    }
    let mut operations = vec![json!({
        "op": "create_node",
        "id": task,
        "kind": "task",
        "title": title,
        "body": text(input, "context")?,
        "task_state": "active",
        "metadata": {
            "aq:playbook_namespace": namespace,
            "aq:project_id": project,
            "aq:logical_key": key,
            "aq:work_kind": kind,
            "aq:lifetime": text(input, "lifetime")?,
            "aq:visibility": text(input, "visibility")?,
            "aq:subject": input["subject"],
        },
    })];
    if let Ok(target) = Uuid::parse_str(text(&input["subject"], "id")?) {
        let revision = input["subject"]["revision"].as_i64().unwrap_or(0);
        operations.push(json!({
            "op": "add_edge",
            "source": task,
            "target": target,
            "edge_type": "motivated_by",
            "target_revision": if revision >= 1 { json!(revision) } else { Value::Null },
        }));
    }
    Ok(json!({
        "idempotency_key": format!("aq-playbook-ensure-{task}"),
        "reason": null,
        "operations": operations,
    }))
}

/// Closed variants only. Lifecycle (`task_state`) is never patched.
/// Returns None when the variant value is not admissible.
pub fn work_change_body(
    namespace: Uuid,
    generation: i64,
    task: Uuid,
    expected: i64,
    node: &Value,
    input: &Value,
) -> Result<Option<Value>> {
    let value = text(input, "value")?;
    let hold_id = input["hold_id"].as_str();
    let mut metadata: Map<String, Value> =
        node["metadata"].as_object().cloned().unwrap_or_default();
    let mut patch = Map::new();
    let mut extra = vec![];
    match text(input, "variant")? {
        "requirements" => {
            patch.insert("acceptance_criteria".into(), json!(value));
        }
        "priority" => {
            let Ok(priority) = value.trim().parse::<i64>() else {
                return Ok(None);
            };
            metadata.insert("aq:priority".into(), json!(priority));
        }
        "execution_constraints" => {
            metadata.insert("aq:execution_constraints".into(), json!(value));
        }
        "hold" => {
            let Some(hold) = hold_id.filter(|h| !h.is_empty()) else {
                return Ok(None);
            };
            if metadata.contains_key("aq:hold") {
                return Ok(None);
            }
            metadata.insert("aq:hold".into(), json!({"hold_id": hold, "reason": value}));
        }
        "release_hold" => {
            let Some(hold) = hold_id else {
                return Ok(None);
            };
            if metadata.get("aq:hold").and_then(|h| h["hold_id"].as_str()) != Some(hold) {
                return Ok(None);
            }
            metadata.remove("aq:hold");
        }
        "dependencies" => {
            let Ok(blocker) = Uuid::parse_str(value.trim()) else {
                return Ok(None);
            };
            if blocker == task {
                return Ok(None);
            }
            let mut deps: Vec<Value> = metadata
                .get("aq:dependencies")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            if !deps.contains(&json!(blocker)) {
                deps.push(json!(blocker));
                extra.push(json!({"op": "add_edge", "source": blocker, "target": task, "edge_type": "blocks"}));
            }
            metadata.insert("aq:dependencies".into(), Value::Array(deps));
        }
        _ => return Ok(None),
    }
    if !patch.contains_key("acceptance_criteria") {
        patch.insert("metadata".into(), Value::Object(metadata));
    }
    let mut operations = vec![
        json!({"op": "update_node", "id": task, "expected_version": expected, "patch": patch}),
    ];
    operations.extend(extra);
    let key = stable_id(
        "aq.playbook.work.change.key",
        namespace,
        generation,
        &digest(&(task, expected, input))?,
    )?;
    Ok(Some(json!({
        "idempotency_key": format!("aq-playbook-change-{key}"),
        "reason": null,
        "operations": operations,
    })))
}
