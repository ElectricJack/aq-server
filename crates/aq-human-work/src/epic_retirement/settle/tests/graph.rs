use super::*;
#[derive(Clone, Default)]
pub(super) struct Graph {
    pub(super) nodes: BTreeMap<Uuid, Value>,
    edges: BTreeMap<Uuid, Value>,
    pub(super) revisions: BTreeMap<(Uuid, i64), Value>,
}

impl Graph {
    /// Live incident edges; every edge here is structural, so these are
    /// also the owned links of a revision snapshot.
    pub(super) fn live_edges(&self, id: Uuid) -> Vec<Value> {
        let id = json!(id);
        (self.edges.values())
            .filter(|e| e["deleted"] == false && (e["source"] == id || e["target"] == id))
            .cloned()
            .collect()
    }
    fn bump(&mut self, id: Uuid) {
        let node = self.nodes.get_mut(&id).unwrap();
        node["version"] = json!(node["version"].as_u64().unwrap() + 1);
        node["content_revision"] = json!(node["content_revision"].as_i64().unwrap() + 1);
        let snapshot =
            json!({"node":node.clone(),"links":self.live_edges(id),"relationships_available":true});
        let revision = snapshot["node"]["content_revision"].as_i64().unwrap();
        self.revisions.insert((id, revision), snapshot);
    }
    pub(super) fn get(&self, id: Uuid) -> Value {
        self.nodes[&id].clone()
    }
    pub(super) fn context(&self, id: Uuid) -> Value {
        json!({"node":self.get(id),"nodes":[],"edges":self.live_edges(id),"pinned_revisions":[],"truncated":false})
    }
    pub(super) fn tree(&self, id: Uuid) -> Value {
        let children: Vec<_> = (self.edges.values())
            .filter(|e| e["deleted"] == false && e["edge_type"] == "parent-child" && e["source"] == json!(id))
            .map(|e| json!({"node":self.get(id_of(&e["target"]).unwrap()),"children":[],"truncated":false}))
            .collect();
        json!({"node":self.get(id),"children":children,"truncated":false})
    }
    pub(super) fn revision(&self, id: Uuid, revision: i64) -> Value {
        self.revisions[&(id, revision)].clone()
    }
    fn check(&self, id: Uuid, op: &Value) -> Result<(), String> {
        let node = self.nodes.get(&id).ok_or("missing")?;
        if node["version"] != op["expected_version"] {
            return Err(format!(
                "stale {id}: {} != {}",
                node["version"], op["expected_version"]
            ));
        }
        Ok(())
    }
    pub(super) fn apply(&mut self, operations: &[Value]) -> Result<(), String> {
        assert!(!operations.is_empty() && operations.len() <= 100);
        let mut next = self.clone();
        for op in operations {
            next.one(op)?;
        }
        *self = next;
        Ok(())
    }
    fn one(&mut self, op: &Value) -> Result<(), String> {
        let id = id_of(&op["id"]).unwrap();
        match op["op"].as_str().unwrap() {
            "create_node" => self.create_node(id, op),
            "update_node" => self.update_node(id, op),
            "verify_node" => self.verify_node(id, op),
            "add_edge" => self.add_edge(id, op),
            "delete_node" => self.delete_node(id, op),
            "restore_node" => self.restore_node(id, op),
            other => panic!("unexpected {other}"),
        }
    }
    fn create_node(&mut self, id: Uuid, op: &Value) -> Result<(), String> {
        if self.nodes.contains_key(&id) {
            return Err("conflict".into());
        }
        let task = op["kind"] == "task";
        self.nodes.insert(id, json!({"id":id,"kind":op["kind"],"title":op["title"],
                    "content_type":op.get("content_type").cloned().unwrap_or(json!("note")),
                    "metadata":op.get("metadata").cloned().unwrap_or(json!({})),
                    "task_state":if task { op.get("task_state").cloned().unwrap_or(json!("draft")) } else { Value::Null },
                    "verification":"unverified","last_verified_by":null,"created_by":SERVICE,
                    "provenance":[],"deleted":false,"retired":false,"version":0,"content_revision":0}));
        self.bump(id);
        Ok(())
    }
    fn update_node(&mut self, id: Uuid, op: &Value) -> Result<(), String> {
        self.check(id, op)?;
        let patch = op["patch"].as_object().unwrap();
        if patch.is_empty() {
            return Ok(());
        }
        if patch.get("task_state") == Some(&json!("completed")) && self.open_child(id) {
            return Err("open child".into());
        }
        let node = self.nodes.get_mut(&id).unwrap();
        for (key, value) in patch {
            node[key] = value.clone();
        }
        if patch.contains_key("metadata") {
            node["verification"] = json!("unverified");
            node["last_verified_by"] = Value::Null;
        }
        self.bump(id);
        Ok(())
    }
    fn open_child(&self, id: Uuid) -> bool {
        (self.edges.values()).any(|e| {
            e["deleted"] == false
                && e["edge_type"] == "parent-child"
                && e["source"] == json!(id)
                && !matches!(
                    self.nodes[&id_of(&e["target"]).unwrap()]["task_state"].as_str(),
                    Some("completed" | "canceled")
                )
        })
    }
    fn verify_node(&mut self, id: Uuid, op: &Value) -> Result<(), String> {
        self.check(id, op)?;
        let node = self.nodes.get_mut(&id).unwrap();
        node["verification"] = op["verification"].clone();
        node["last_verified_by"] = json!(SERVICE);
        let provenance = node["provenance"].as_array_mut().unwrap();
        for item in op["evidence"].as_array().unwrap() {
            provenance.retain(|p| p["source_id"] != item["source_id"]);
            provenance.push(item.clone());
        }
        self.bump(id);
        Ok(())
    }
    fn add_edge(&mut self, id: Uuid, op: &Value) -> Result<(), String> {
        let (source, target) = (id_of(&op["source"]).unwrap(), id_of(&op["target"]).unwrap());
        if self.nodes[&source]["deleted"] == true || self.nodes[&target]["deleted"] == true {
            return Err("deleted endpoint".into());
        }
        self.edges.insert(id, json!({"id":id,"edge_type":op["edge_type"],"source":source,"target":target,"deleted":false}));
        self.bump(source);
        self.bump(target);

        Ok(())
    }
    fn delete_node(&mut self, id: Uuid, op: &Value) -> Result<(), String> {
        self.check(id, op)?;
        let incident: Vec<_> = self.live_edges(id);
        for edge in &incident {
            self.edges.get_mut(&id_of(&edge["id"]).unwrap()).unwrap()["deleted"] = json!(true);
        }
        self.nodes.get_mut(&id).unwrap()["deleted"] = json!(true);
        self.bump(id);
        for edge in incident {
            for end in ["source", "target"] {
                let other = id_of(&edge[end]).unwrap();
                if other != id {
                    self.bump(other);
                }
            }
        }

        Ok(())
    }
    fn restore_node(&mut self, id: Uuid, op: &Value) -> Result<(), String> {
        self.check(id, op)?;
        let snapshot = self.revision(id, op["revision"].as_i64().unwrap());
        let lifecycle = op["include_lifecycle"] == true;
        if self.nodes[&id]["deleted"] == true && !lifecycle {
            return Err("tombstone".into());
        }
        let others = self.restore_links(id, &snapshot["links"])?;
        let node = self.nodes.get_mut(&id).unwrap();
        for key in ["title", "metadata"] {
            node[key] = snapshot["node"][key].clone();
        }
        if lifecycle {
            for key in ["task_state", "deleted", "retired"] {
                node[key] = snapshot["node"][key].clone();
            }
        }
        node["verification"] = json!("unverified");
        node["last_verified_by"] = Value::Null;
        self.bump(id);
        for other in others {
            self.bump(other);
        }

        Ok(())
    }
    fn restore_links(&mut self, id: Uuid, links: &Value) -> Result<Vec<Uuid>, String> {
        let mut others = vec![];
        for link in links.as_array().unwrap() {
            let other = [
                id_of(&link["source"]).unwrap(),
                id_of(&link["target"]).unwrap(),
            ]
            .into_iter()
            .find(|end| *end != id)
            .unwrap();
            if self.nodes[&other]["deleted"] == true {
                return Err("deleted endpoint".into());
            }
            self.edges.insert(id_of(&link["id"]).unwrap(), link.clone());
            others.push(other);
        }
        Ok(others)
    }
}
