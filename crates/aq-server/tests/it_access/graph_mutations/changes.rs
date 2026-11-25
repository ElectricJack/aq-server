use super::*;
pub(super) struct Changes {
    pub(super) nodes: BTreeMap<String, Value>,
    pub(super) edges: BTreeMap<String, Value>,
}
impl Changes {
    pub(super) fn apply(&mut self, op: &Value) -> Result<Value, ApiError> {
        let id = op["id"].as_str().unwrap().to_owned();
        match op["op"].as_str().unwrap() {
            "update_node" => self.update(&id, op),
            "create_node" => self.create(id, op),
            "add_edge" => self.add_edge(id, op),
            "remove_edge" => self.remove_edge(&id, op),
            _ => Err(ApiError::Invalid),
        }
    }
    fn update(&mut self, id: &str, op: &Value) -> Result<Value, ApiError> {
        let node = self.nodes.get_mut(id).ok_or(ApiError::Missing)?;
        let protected_author = node["kind"] == "knowledge" && node["created_by"] != "api01-writer";
        let editable_version = node["retired"] != true && node["verification"] == "unverified";
        if !editable_version || protected_author {
            return Err(ApiError::Forbidden);
        }
        if node["version"] != op["expected_version"] {
            return Err(ApiError::Conflict);
        }
        let patch = op["patch"].as_object().unwrap();
        if !patch.is_empty() {
            for (k, v) in patch {
                node[k] = v.clone();
            }
            Self::bump(node);
        }
        Ok(node.clone())
    }
    fn bump(node: &mut Value) {
        node["version"] = json!(node["version"].as_i64().unwrap() + 1);
        node["content_revision"] = json!(node["content_revision"].as_i64().unwrap() + 1);
    }
    fn create(&mut self, id: String, op: &Value) -> Result<Value, ApiError> {
        if self.nodes.contains_key(&id) {
            return Err(ApiError::Conflict);
        }
        let prototype = if op["kind"] == "task" { uid(1) } else { uid(2) };
        let mut n = self.nodes[&prototype].clone();
        n["id"] = json!(id);
        n["version"] = json!(1);
        n["content_revision"] = json!(1);
        n["created_by"] = json!("api01-writer");
        for field in [
            "title",
            "body",
            "labels",
            "content_type",
            "acceptance_criteria",
            "task_state",
            "provenance",
        ] {
            if let Some(v) = op.get(field) {
                n[field] = v.clone();
            }
        }
        self.nodes.insert(id, n.clone());
        Ok(n)
    }
    fn add_edge(&mut self, id: String, op: &Value) -> Result<Value, ApiError> {
        if self.edges.contains_key(&id) {
            return Err(ApiError::Conflict);
        }
        let mut e = self.edges.values().next().unwrap().clone();
        e["id"] = json!(id);
        for field in [
            "source",
            "target",
            "edge_type",
            "description",
            "source_revision",
            "target_revision",
        ] {
            e[field] = op.get(field).cloned().unwrap_or(Value::Null);
        }
        e["version"] = json!(1);
        e["deleted"] = json!(false);
        self.edges.insert(id, e.clone());
        if let Some(node) = self.nodes.get_mut(op["source"].as_str().unwrap()) {
            Self::bump(node);
        }
        Ok(e)
    }
    fn remove_edge(&mut self, id: &str, op: &Value) -> Result<Value, ApiError> {
        let edge = self.edges.get_mut(id).ok_or(ApiError::Missing)?;
        if edge["version"] != op["expected_version"] {
            return Err(ApiError::Conflict);
        }
        edge["deleted"] = json!(true);
        edge["version"] = json!(edge["version"].as_i64().unwrap() + 1);
        Ok(edge.clone())
    }
}
