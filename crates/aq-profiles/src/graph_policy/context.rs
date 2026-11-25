use super::*;

impl GraphAccessPolicy {
    /// Current native mode grants the finite observed record projection for
    /// task/knowledge reads. Source writes remain a separate repository grant.
    pub fn validate_context(&self, context: &serde_json::Value) -> Result<()> {
        self.validate()?;
        ensure(
            aq_agent_api::valid("ContextSnapshot", context)
                && digest(context)? == self.context_digest,
            "graph policy context digest/shape",
        )?;
        ensure(
            context["root_ids"] == serde_json::json!([self.scope.task_id])
                && context["coverage"]["status"] == "complete",
            "graph policy required context coverage",
        )?;
        let mut actual = BTreeSet::new();
        let resource = format!("project:{}", self.project_binding_id);
        for key in ["nodes", "pinned_revisions"] {
            for node in context[key].as_array().expect("validated context") {
                let operation = if node["kind"] == "task" {
                    "task.read"
                } else {
                    "knowledge.read"
                };
                ensure(
                    self.effective_permissions
                        .iter()
                        .any(|p| p.operation == operation && p.scope_reference == resource),
                    "required context record lacks typed read permission",
                )?;
                actual.insert(node["id"].as_str().expect("validated UUID").to_string());
            }
        }
        ensure(
            actual.contains(&self.scope.task_id.to_string())
                && actual == self.scope.record_ids.iter().map(Uuid::to_string).collect(),
            "graph policy exact context record scope",
        )?;
        self.validate_edges(context, &actual)
    }
    fn validate_edges(&self, context: &serde_json::Value, actual: &BTreeSet<String>) -> Result<()> {
        let allowed = self
            .scope
            .edge_types
            .iter()
            .map(|e| serde_json::to_value(e).unwrap())
            .collect::<Vec<_>>();
        for edge in context["edges"].as_array().expect("validated context") {
            ensure(
                actual.contains(edge["source"].as_str().unwrap())
                    && actual.contains(edge["target"].as_str().unwrap())
                    && allowed.contains(&edge["edge_type"]),
                "graph policy context edge scope",
            )?;
        }
        Ok(())
    }
}
