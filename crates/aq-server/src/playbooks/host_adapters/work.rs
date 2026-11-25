use super::*;

impl Service {
    pub(super) async fn work_ensure(
        &self,
        owner: &RuntimeOwner,
        command: HostIntent<'_>,
    ) -> Result<HostResult> {
        let intent = command.intent;
        let ports = self.ports()?;
        let workspace = self.namespace.workspace_id;
        let (stage, identity) = self.prepare_effect(owner, command, Effect::Ensure).await?;
        let plan = match stage {
            PreparedEffect::Complete(result) => return Ok(result),
            PreparedEffect::Pending(plan) => plan,
        };
        let task: Uuid = serde_json::from_value(plan["task"].clone())?;
        let result = match ports.graph.node(workspace, task).await? {
            Some(node) => ("ensured", node_version(&node)?),
            None => match ports.graph.mutate(workspace, plan["body"].clone()).await? {
                (200..=299, raw) => ("ensured", node_version(&raw["results"][0])?),
                (409, _) => match ports.graph.node(workspace, task).await? {
                    Some(node) => ("ensured", node_version(&node)?),
                    None => ("refused", 0),
                },
                (400..=499, _) => ("refused", 0),
                _ => return Err(Error::Unknown),
            },
        };
        let mut output = work_output(task, result.1);
        if result.0 == "refused" {
            output["disposition"] = json!("failed");
        }
        self.finish(
            intent,
            &identity,
            HostResult {
                outcome: result.0.into(),
                output,
            },
        )
        .await
    }

    pub(super) async fn work_change(
        &self,
        owner: &RuntimeOwner,
        command: HostIntent<'_>,
    ) -> Result<HostResult> {
        let intent = command.intent;
        let ports = self.ports()?;
        let workspace = self.namespace.workspace_id;
        let (stage, identity) = self.prepare_effect(owner, command, Effect::Change).await?;
        let plan = match stage {
            PreparedEffect::Complete(result) => return Ok(result),
            PreparedEffect::Pending(plan) => plan,
        };
        let task: Uuid = serde_json::from_value(plan["task"].clone())?;
        let expected = plan["expected"].as_i64().ok_or(Error::Unknown)?;
        let result = match ports.graph.mutate(workspace, plan["body"].clone()).await? {
            (200..=299, raw) => {
                let version = raw["results"]
                    .as_array()
                    .and_then(|r| r.iter().find(|n| n["id"] == json!(task)))
                    .ok_or(Error::Unknown)
                    .and_then(node_version)?;
                ("changed", version)
            }
            (409, _) => {
                let version = match ports.graph.node(workspace, task).await? {
                    Some(node) => node_version(&node)?,
                    None => expected,
                };
                ("conflict", version)
            }
            (400..=499, _) => ("refused", expected),
            _ => return Err(Error::Unknown),
        };
        self.finish(
            intent,
            &identity,
            HostResult {
                outcome: result.0.into(),
                output: work_output(task, result.1),
            },
        )
        .await
    }

    pub(super) async fn ensure_plan(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        intent: &Intent,
    ) -> Result<PreparedEffect> {
        let input = &intent.input;
        let workspace = self.namespace.workspace_id;
        let ports = self.ports()?;
        let task = aq_playbook_domain::work_mutations::reserve_work_key(
            tx,
            &self.namespace,
            input,
            || self.now(),
        )
        .await?;
        if let Some(node) = ports.graph.node(workspace, task).await? {
            let result = HostResult {
                outcome: "ensured".into(),
                output: work_output(task, node_version(&node)?),
            };
            return Ok(PreparedEffect::Complete(result));
        }
        let plan = json!({"task": task, "body": work_ensure_body(self.namespace.namespace_id, self.namespace.project_id, task, input)?});
        Ok(PreparedEffect::Pending(plan))
    }
    pub(super) async fn change_plan(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        intent: &Intent,
    ) -> Result<PreparedEffect> {
        let input = &intent.input;
        let workspace = self.namespace.workspace_id;
        let ports = self.ports()?;
        let subject_id = text(&input["subject"], "id")?;
        let expected = input["subject"]["revision"]
            .as_i64()
            .ok_or(Error::Invalid)?;
        let refused = |task: &str| HostResult {
            outcome: "refused".into(),
            output: json!({"reference": format!("{WORK_REF}{task}"), "subject": subject(task, expected)}),
        };
        let Ok(task) = Uuid::parse_str(subject_id) else {
            return Ok(PreparedEffect::Complete(refused(subject_id)));
        };
        if !self.namespace_work(tx, task).await? {
            return Ok(PreparedEffect::Complete(refused(subject_id)));
        }
        let Some(node) = ports.graph.node(workspace, task).await? else {
            return Ok(PreparedEffect::Complete(refused(subject_id)));
        };
        let current = node_version(&node)?;
        if current != expected {
            // Stale expected version: a version refusal, no mutation.
            let result = HostResult {
                outcome: "conflict".into(),
                output: work_output(task, current),
            };
            return Ok(PreparedEffect::Complete(result));
        }
        let Some(body) = work_change_body(
            self.namespace.namespace_id,
            self.namespace.generation,
            task,
            current,
            &node,
            input,
        )?
        else {
            return Ok(PreparedEffect::Complete(refused(subject_id)));
        };
        let plan = json!({"task": task, "expected": current, "body": body});
        Ok(PreparedEffect::Pending(plan))
    }
}
