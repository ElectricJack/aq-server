use super::*;
// Owner task lifecycle never waits for a policy gate or a dependent task.
impl GraphPayload for TaskDelete {
    const KIND: &'static str = "task.delete";
    const GRANT: &'static str = "task.delete";
    const ENTITY: &'static str = "task";
    fn operation_id(&self) -> Uuid {
        self.operation_id
    }
    fn project_id(&self) -> Uuid {
        self.project_id
    }
    fn expected(&self) -> &str {
        &self.expected_version
    }
    fn valid(&self) -> bool {
        true
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        Ok(vec![
            json!({"op":"delete_node","id":c.subject,"expected_version":c.version}),
        ])
    }
}

impl GraphPayload for aq_human_work::TaskActivate {
    const KIND: &'static str = "task.activate";
    const GRANT: &'static str = "task.activate";
    const ENTITY: &'static str = "task";
    fn operation_id(&self) -> Uuid {
        self.operation_id
    }
    fn project_id(&self) -> Uuid {
        self.project_id
    }
    fn expected(&self) -> &str {
        &self.expected_version
    }
    fn valid(&self) -> bool {
        true
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        // Only a draft: it has never had an attempt, so there is nothing to
        // reconcile. Reopening a finished task is a different question.
        if c.node["task_state"] != "draft" {
            return Err(ApiError::invalid());
        }
        Ok(vec![aq_human_work::subtree_activation::activate_task(
            c.subject, c.version,
        )])
    }
}

impl GraphPayload for aq_human_work::TaskComplete {
    const KIND: &'static str = "task.complete";
    const GRANT: &'static str = "task.complete";
    const ENTITY: &'static str = "task";
    fn operation_id(&self) -> Uuid {
        self.operation_id
    }
    fn project_id(&self) -> Uuid {
        self.project_id
    }
    fn expected(&self) -> &str {
        &self.expected_version
    }
    fn valid(&self) -> bool {
        true
    }
    fn reason_required() -> bool {
        true
    }
    fn not_held() -> bool {
        true
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        // Settlement completes work that ran. This is for work finished
        // outside AQ, so never for a task a playbook manages: the playbook
        // would read the completion as an accepted result it has no receipt for.
        if c.node["task_state"] != "active"
            || c.node["metadata"].get("aq:playbook_namespace").is_some()
        {
            return Err(ApiError::invalid());
        }
        Ok(vec![
            json!({"op":"update_node","id":c.subject,"expected_version":c.version,"patch":{"task_state":"completed"}}),
        ])
    }
}

impl GraphPayload for aq_human_work::TaskReopen {
    const KIND: &'static str = "task.reopen";
    const GRANT: &'static str = "task.reopen";
    const ENTITY: &'static str = "task";
    fn operation_id(&self) -> Uuid {
        self.operation_id
    }
    fn project_id(&self) -> Uuid {
        self.project_id
    }
    fn expected(&self) -> &str {
        &self.expected_version
    }
    fn valid(&self) -> bool {
        true
    }
    fn never_attempted() -> bool {
        true
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        if !matches!(
            c.node["task_state"].as_str(),
            Some("completed" | "canceled")
        ) {
            return Err(ApiError::invalid());
        }
        Ok(vec![
            json!({"op":"update_node","id":c.subject,"expected_version":c.version,"patch":{"task_state":"active"}}),
        ])
    }
}

impl GraphPayload for aq_human_work::Execution {
    const KIND: &'static str = "task.cancel";
    const GRANT: &'static str = "task.cancel";
    const ENTITY: &'static str = "task";
    fn operation_id(&self) -> Uuid {
        self.operation_id
    }
    fn project_id(&self) -> Uuid {
        self.project_id
    }
    fn expected(&self) -> &str {
        &self.expected_version
    }
    fn valid(&self) -> bool {
        true
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        Ok(vec![
            json!({"op":"update_node","id":c.subject,"expected_version":c.version,"patch":{"task_state":"canceled"}}),
        ])
    }
}

impl GraphPayload for TaskEdit {
    common!("task.edit", "task", aq_agent_grants::op::TASK_EDIT);
    fn valid(&self) -> bool {
        TaskEdit::valid(self)
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        Ok(update(
            c,
            self.patch(c.node).ok_or_else(ApiError::unavailable)?,
        ))
    }
}

impl GraphPayload for TaskReprioritize {
    common!("task.reprioritize", "task", aq_agent_grants::op::TASK_EDIT);
    fn valid(&self) -> bool {
        TaskReprioritize::valid(self)
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        Ok(update(
            c,
            self.patch(c.node).ok_or_else(ApiError::unavailable)?,
        ))
    }
}

impl GraphPayload for TaskRoleSet {
    common!("task.role.set", "task", aq_agent_grants::op::TASK_EDIT);
    fn valid(&self) -> bool {
        super::super::enqueue::hints_valid(None, [&self.role, &None, &None])
    }
    fn never_granted() -> bool {
        true
    }
    fn roles(&self) -> Vec<&str> {
        self.role.as_deref().into_iter().collect()
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        let role = self.role.as_ref().map(|role| json!(role));
        metadata_patch(c, aq_role_settings::tasks::ROLE_KEY, role)
    }
}

#[cfg(feature = "project_environments")]
impl GraphPayload for TaskNeedsSet {
    common!("task.needs.set", "task", aq_agent_grants::op::TASK_EDIT);
    fn valid(&self) -> bool {
        true
    }
    fn never_granted() -> bool {
        true
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        let needs = (self.needs.as_ref()).filter(|n| !n.0.is_empty());
        let key = aq_project_policy::environments::NEEDS_KEY;
        metadata_patch(c, key, needs.map(|n| json!(n.0)))
    }
}

impl GraphPayload for TaskRouteSet {
    common!("task.route.set", "task", aq_agent_grants::op::TASK_EDIT);
    fn valid(&self) -> bool {
        self.class.parse::<aq_routing::Class>().is_ok()
            && labels(&self.harnesses)
            && labels(&self.providers)
    }
    fn operations(&self, _: &Current<'_>) -> ApiResult<Vec<Value>> {
        Ok(vec![])
    }
    fn route_change(&self) -> Option<RouteChange> {
        Some(RouteChange::Set(Box::new(aq_routing::Route {
            class: self.class.parse().ok()?,
            harnesses: self.harnesses.clone().unwrap_or_default(),
            providers: self.providers.clone().unwrap_or_default(),
            prefer: vec![],
            lane: None,
            provider_intent: aq_routing::ProviderIntent::Any,
            reasons: vec![aq_routing::RouteReason {
                code: "owner_set".into(),
                detail: "Set by the owner".into(),
            }],
            classification: None,
            policy_digest: None,
            structure_revision: None,
            source: aq_routing::Source::Owner,
            ended_attempts: 0,
            judgment: None,
            environment: None,
        })))
    }
}

impl GraphPayload for TaskRouteClear {
    common!("task.route.clear", "task", aq_agent_grants::op::TASK_EDIT);
    fn valid(&self) -> bool {
        true
    }
    fn operations(&self, _: &Current<'_>) -> ApiResult<Vec<Value>> {
        Ok(vec![])
    }
    fn route_change(&self) -> Option<RouteChange> {
        Some(RouteChange::Clear)
    }
}

impl GraphPayload for DependencyAdd {
    common!(
        "task.dependency.add",
        "task",
        aq_agent_grants::op::TASK_EDIT
    );
    fn valid(&self) -> bool {
        !self.blocker_id.is_nil()
    }
    fn tasks(&self) -> Vec<Uuid> {
        vec![self.blocker_id]
    }
    fn resume_held(&self) -> Option<Uuid> {
        self.resume_held.then_some(self.blocker_id)
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        if self.blocker_id == c.subject {
            return Err(ApiError::invalid());
        }
        // Structural edges reject revision pins; a duplicate link is a Graph no-op.
        Ok(vec![
            json!({"op":"add_edge","id":self.operation_id,"source":self.blocker_id,"target":c.subject,"edge_type":"blocks","source_revision":null,"target_revision":null}),
        ])
    }
}

impl GraphPayload for DependencyRemove {
    common!(
        "task.dependency.remove",
        "task",
        aq_agent_grants::op::TASK_EDIT
    );
    fn valid(&self) -> bool {
        !self.edge_id.is_nil() && version(&self.edge_version).is_some()
    }
    fn operations(&self, _: &Current<'_>) -> ApiResult<Vec<Value>> {
        let v = version(&self.edge_version).ok_or_else(ApiError::invalid)?;
        Ok(vec![
            json!({"op":"remove_edge","id":self.edge_id,"expected_version":v}),
        ])
    }
}
