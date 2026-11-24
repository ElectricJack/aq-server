use super::*;
impl GraphPayload for KnowledgeAuthor {
    const KIND: &'static str = "knowledge.author";
    const GRANT: &'static str = aq_agent_grants::op::KNOWLEDGE_AUTHOR;
    const TARGET: aq_command_registry::TargetRule =
        aq_command_registry::TargetRule::Project("project_id");
    const ENTITY: &'static str = "project";
    fn operation_id(&self) -> Uuid {
        self.operation_id
    }
    fn project_id(&self) -> Uuid {
        self.project_id
    }
    fn expected(&self) -> &str {
        &self.expected_project_revision
    }
    fn valid(&self) -> bool {
        text(&self.title, 1, 512)
            && !self.title.trim().is_empty()
            && text(&self.body, 0, 32768)
            && labels(&self.labels)
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        // Authoring never asserts verification; review is a separate admin verify.
        Ok(vec![json!({
            "op":"create_node","id":self.operation_id,"kind":"knowledge",
            "title":self.title,"body":self.body,
            "labels":self.labels.clone().unwrap_or_default(),
            "content_type":self.content_type.unwrap_or(ContentType::Note),
            "verification":"unverified",
            "metadata":{"aq:human_actor_id":c.actor,"aq:human_operation_id":self.operation_id,"aq:project_id":c.project}
        })])
    }
}

impl GraphPayload for KnowledgeCorrect {
    common!(
        "knowledge.correct",
        "knowledge",
        aq_agent_grants::op::KNOWLEDGE_AUTHOR
    );
    fn valid(&self) -> bool {
        (self.title.is_some()
            || self.body.is_some()
            || self.labels.is_some()
            || self.content_type.is_some())
            && self
                .title
                .as_deref()
                .is_none_or(|t| text(t, 1, 512) && !t.trim().is_empty())
            && self.body.as_deref().is_none_or(|t| text(t, 0, 32768))
            && labels(&self.labels)
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        let mut patch = serde_json::Map::new();
        put(&mut patch, "title", self.title.as_ref());
        put(&mut patch, "body", self.body.as_ref());
        put(&mut patch, "labels", self.labels.as_ref());
        put(&mut patch, "content_type", self.content_type);
        Ok(update(c, patch))
    }
}

impl GraphPayload for KnowledgeVerify {
    common!("knowledge.verify", "knowledge", "knowledge.verify");
    fn reason_required() -> bool {
        true
    }
    fn valid(&self) -> bool {
        (1..=64).contains(&self.evidence.len())
            && self
                .evidence
                .iter()
                .all(|s| contract::upstream_valid("Source", &Value::Object(s.clone())))
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        Ok(vec![
            json!({"op":"verify_node","id":c.subject,"expected_version":c.version,"verification":self.verification,"evidence":self.evidence}),
        ])
    }
}

impl GraphPayload for KnowledgeRetire {
    common!("knowledge.retire", "knowledge", "knowledge.retire");
    fn valid(&self) -> bool {
        text(&self.retirement_reason, 1, 4096)
            && !self.retirement_reason.trim().is_empty()
            && self.successor_id.is_none_or(|s| !s.is_nil())
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        // Retiring again would append a revision and overwrite the reason (N-03).
        if self.successor_id == Some(c.subject) || c.node["retired"] != false {
            return Err(ApiError::invalid());
        }
        let mut patch = serde_json::Map::new();
        patch.insert("retired".into(), json!(true));
        patch.insert("retirement_reason".into(), json!(self.retirement_reason));
        put(&mut patch, "successor_id", self.successor_id);
        Ok(update(c, patch))
    }
}

impl GraphPayload for KnowledgeRestore {
    common!("knowledge.restore", "knowledge", "knowledge.restore");
    fn valid(&self) -> bool {
        self.revision >= 1 && self.revision <= i64::MAX as u64
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        let mut op = json!({"op":"restore_node","id":c.subject,"expected_version":c.version,"revision":self.revision});
        if let Some(lifecycle) = self.include_lifecycle {
            op["include_lifecycle"] = json!(lifecycle);
        }
        Ok(vec![op])
    }
}
