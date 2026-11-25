//! Graph mutation bodies for the host work adapters.
use crate::{Error, Result, digest, host_values::text, stable_id};
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
    let title = work_title(kind, key);
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

fn work_title(kind: &str, key: &str) -> String {
    let mut title = format!("{kind}: {key}");
    if title.len() > 512 {
        let mut end = 512;
        while !title.is_char_boundary(end) {
            end -= 1;
        }
        title.truncate(end);
    }
    title
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
    let mut patch = WorkPatch::from_node(node);
    if !patch.apply(input, task, value)? {
        return Ok(None);
    }
    let operations = patch.operations(task, expected);
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

/// Metadata updates and structural graph operations are emitted in one request.
struct WorkPatch {
    metadata: Map<String, Value>,
    fields: Map<String, Value>,
    edges: Vec<Value>,
}
impl WorkPatch {
    fn from_node(node: &Value) -> Self {
        Self {
            metadata: node["metadata"].as_object().cloned().unwrap_or_default(),
            fields: Map::new(),
            edges: Vec::new(),
        }
    }
    fn apply(&mut self, input: &Value, task: Uuid, value: &str) -> Result<bool> {
        let hold_id = input["hold_id"].as_str();
        match text(input, "variant")? {
            "requirements" => {
                self.fields
                    .insert("acceptance_criteria".into(), json!(value));
            }
            "priority" => {
                let Ok(priority) = value.trim().parse::<i64>() else {
                    return Ok(false);
                };
                self.metadata.insert("aq:priority".into(), json!(priority));
            }
            "execution_constraints" => {
                self.metadata
                    .insert("aq:execution_constraints".into(), json!(value));
            }
            "hold" => return Ok(self.hold(hold_id, value)),
            "release_hold" => return Ok(self.release_hold(hold_id)),
            "dependencies" => return Ok(self.dependency(task, value)),
            _ => return Ok(false),
        }
        Ok(true)
    }
    fn hold(&mut self, id: Option<&str>, reason: &str) -> bool {
        let Some(id) = id.filter(|h| !h.is_empty()) else {
            return false;
        };
        if self.metadata.contains_key("aq:hold") {
            return false;
        }
        self.metadata
            .insert("aq:hold".into(), json!({"hold_id":id,"reason":reason}));
        true
    }
    fn release_hold(&mut self, id: Option<&str>) -> bool {
        let Some(id) = id else {
            return false;
        };
        if self
            .metadata
            .get("aq:hold")
            .and_then(|h| h["hold_id"].as_str())
            != Some(id)
        {
            return false;
        }
        self.metadata.remove("aq:hold");
        true
    }
    fn dependency(&mut self, task: Uuid, value: &str) -> bool {
        let Ok(blocker) = Uuid::parse_str(value.trim()) else {
            return false;
        };
        if blocker == task {
            return false;
        }
        let mut ids = self
            .metadata
            .get("aq:dependencies")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if !ids.contains(&json!(blocker)) {
            ids.push(json!(blocker));
            self.edges
                .push(json!({"op":"add_edge","source":blocker,"target":task,"edge_type":"blocks"}));
        }
        self.metadata
            .insert("aq:dependencies".into(), Value::Array(ids));
        true
    }
    fn operations(mut self, task: Uuid, expected: i64) -> Vec<Value> {
        if !self.fields.contains_key("acceptance_criteria") {
            self.fields
                .insert("metadata".into(), Value::Object(self.metadata));
        }
        let mut ops = vec![
            json!({"op":"update_node","id":task,"expected_version":expected,"patch":self.fields}),
        ];
        ops.extend(self.edges);
        ops
    }
}

pub async fn reserve_work_key(
    db: &mut sqlx::PgConnection,
    namespace: &crate::Namespace,
    input: &Value,
    clock: impl FnOnce() -> Result<i64>,
) -> Result<Uuid> {
    use sqlx::Row;
    let key = text(input, "logical_key")?;
    if key.is_empty() {
        return Err(Error::Invalid);
    }
    // The logical key binds every declared field; a reuse with
    // different work is a conflict, never a silent re-point.
    let request = digest(&(
        "aq.playbook.work.key",
        text(input, "kind")?,
        text(input, "lifetime")?,
        text(input, "context")?,
        &input["subject"],
        text(input, "visibility")?,
        key,
    ))?;
    let existing = sqlx::query("SELECT task_id,request_digest FROM playbook_work_keys WHERE namespace_id=$1 AND generation=$2 AND logical_key=$3 FOR UPDATE")
    .bind(namespace.namespace_id)
    .bind(namespace.generation)
    .bind(key)
    .fetch_optional(&mut *db)
    .await?;
    let task = match existing {
        Some(row) => {
            if row.get::<String, _>("request_digest") != request {
                return Err(Error::Conflict);
            }
            row.get::<Uuid, _>("task_id")
        }
        None => {
            let task = stable_id(
                "aq.playbook.work.node",
                namespace.namespace_id,
                namespace.generation,
                key,
            )?;
            sqlx::query("INSERT INTO playbook_work_keys(namespace_id,generation,logical_key,task_id,request_digest,created_at_ms) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(namespace.namespace_id)
            .bind(namespace.generation)
            .bind(key)
            .bind(task)
            .bind(&request)
            .bind(clock()?)
            .execute(&mut *db)
            .await?;
            task
        }
    };
    Ok(task)
}
