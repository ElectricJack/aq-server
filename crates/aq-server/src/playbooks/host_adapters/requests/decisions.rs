use super::*;

impl Service {
    /// D11 Host7 decision.request: one durable question (accepted effect row
    /// plus one running `playbook_host_operations` row) answered once by a
    /// listed responder through [`Service::answer_decision`], or expired at
    /// `expires_at_ms`. The completion seals `aq.decision.completed`. Nothing
    /// is ever auto-approved.
    pub(in crate::playbooks::host_adapters) async fn host_decision_request(
        &self,
        owner: &RuntimeOwner,
        command: HostIntent<'_>,
    ) -> Result<HostResult> {
        let intent = command.intent;
        let (tx, stage, identity) = self.begin(Some(owner), command).await?;
        let operation = self.operation(intent)?;
        match stage.ok_or(Error::Unknown)? {
            Stage::Retained(result) => {
                tx.commit().await?;
                return Ok(result);
            }
            Stage::Pending(_) => return Err(Error::Conflict),
            Stage::Fresh => {}
        }
        let now = self.now()?;
        let decision = serde_json::from_value::<DecisionRequest>(intent.input.clone());
        if !decision.is_ok_and(|request| request.valid(now)) {
            let result = HostResult {
                outcome: "refused".into(),
                output: json!({"reference": format!("decision:{operation}"), "subject": intent.input["subject"]}),
            };
            return self
                .settle_fresh(tx, command.retained(&identity), result)
                .await;
        }
        self.record_operation(
            tx,
            command.retained(&identity),
            OperationRequest::new(None, &intent.input),
        )
        .await
    }

    pub(super) async fn observe_decision(
        &self,
        operation: Uuid,
        op: &Operation,
    ) -> Result<Option<HostResult>> {
        let request: DecisionRequest = serde_json::from_value(op.request.clone())?;
        if self.now()? < request.expires_at_ms {
            return Ok(None);
        }
        self.complete_operation(
            OperationKey::new(operation, &op.command, &op.intent_id),
            HostResult {
                outcome: "expired".into(),
                output: request.output(operation),
            },
            None,
        )
        .await
        .map(Some)
    }

    pub(in crate::playbooks) async fn decisions(
        &self,
        actor: Uuid,
        handle: Option<Uuid>,
    ) -> Result<Vec<DecisionView>> {
        let ops = Operations(&self.namespace);
        let mut tx = self.tx().await?;
        Self::owner(&mut tx, actor).await?;
        ops.require_project_workspace(&mut *tx).await?;
        let views = ops.decisions(&mut *tx, self.now()?, handle).await?;
        tx.commit().await?;
        Ok(views)
    }

    /// Accept one answer from a listed responder and retain its completion.
    /// An answer retry returns the original outcome.
    pub async fn answer_decision(
        &self,
        actor: Uuid,
        handle: Uuid,
        choice: &str,
        note: Option<&str>,
    ) -> Result<HostResult> {
        let ops = Operations(&self.namespace);
        ops.require_project_workspace(&self.pool).await?;
        let op = self
            .load_operation(&self.pool, handle)
            .await?
            .filter(|op| self.profile.is_host2() && op.command == "decision.request")
            .ok_or(Error::Forbidden)?;
        let request: DecisionRequest = serde_json::from_value(op.request.clone())?;
        if !request.permits(actor) {
            return Err(Error::Forbidden);
        }
        if op.status != "running" {
            return op.retained();
        }
        if self.now()? >= request.expires_at_ms {
            return Err(Error::Conflict);
        }
        let answer = DecisionAnswer {
            actor,
            handle,
            choice,
            note,
        };
        answer.validate(&request)?;
        let task = self.decision_comment_target(&request, note).await?;
        let decision = PendingDecision {
            operation: op,
            request,
            comment: task,
        };
        self.save_answer(answer, decision).await
    }
    async fn save_answer(
        &self,
        answer: DecisionAnswer<'_>,
        decision: PendingDecision<'_>,
    ) -> Result<HostResult> {
        let mut tx = self.tx().await?;
        Self::owner(&mut tx, answer.actor).await?;
        Operations(&self.namespace)
            .require_project_workspace(&mut *tx)
            .await?;
        self.namespace_locked(&mut tx).await?;
        let current = self
            .load_operation(&mut *tx, answer.handle)
            .await?
            .ok_or(Error::Forbidden)?;
        if current.status != "running" {
            return current.retained();
        }
        let now = self.now()?;
        if now >= decision.request.expires_at_ms {
            return Err(Error::Conflict);
        }
        let completion = decision.completion(&answer, self.profile);
        let result = self.retain_completion(&mut tx, completion).await?;
        if let Some((task_id, body)) = decision.comment {
            let id = stable_id(
                "aq.playbook.decision.comment",
                self.namespace.namespace_id,
                self.namespace.generation,
                &decision.operation.intent_id,
            )?;
            aq_task_comments::add_in_tx(
                &mut tx,
                id,
                aq_task_comments::Thread {
                    project_id: self.namespace.project_id,
                    task_id,
                },
                &aq_task_comments::Author::Owner {
                    user_id: answer.actor,
                },
                body,
                chrono::DateTime::from_timestamp_millis(now).ok_or(Error::Unknown)?,
            )
            .await
            .map_err(|_| Error::Unknown)?;
        }
        tx.commit().await?;
        Ok(result)
    }
    async fn decision_comment_target<'a>(
        &self,
        request: &DecisionRequest,
        note: Option<&'a str>,
    ) -> Result<Option<(Uuid, &'a str)>> {
        Ok(
            match (request.subject_id(), note.filter(|s| !s.trim().is_empty())) {
                (Some(id), Some(body))
                    if self
                        .ports()?
                        .graph
                        .node(self.namespace.workspace_id, id)
                        .await?
                        .is_some_and(|node| node["kind"] == "task" && node["deleted"] != true) =>
                {
                    Some((id, body))
                }
                _ => None,
            },
        )
    }
}

struct DecisionAnswer<'a> {
    actor: Uuid,
    handle: Uuid,
    choice: &'a str,
    note: Option<&'a str>,
}
struct PendingDecision<'a> {
    operation: Operation,
    request: DecisionRequest,
    comment: Option<(Uuid, &'a str)>,
}
impl PendingDecision<'_> {
    fn completion<'a>(
        &'a self,
        answer: &DecisionAnswer<'a>,
        profile: aq_playbook_domain::review_domain::CatalogProfile,
    ) -> Completion<'a> {
        let mut output = self.request.output(answer.handle);
        if profile.has_notes() {
            output["note"] = json!(answer.note.unwrap_or_default());
        }
        Completion {
            key: OperationKey::new(
                answer.handle,
                &self.operation.command,
                &self.operation.intent_id,
            ),
            result: HostResult {
                outcome: answer.choice.into(),
                output,
            },
            note: answer.note,
        }
    }
}
impl DecisionAnswer<'_> {
    fn validate(&self, request: &DecisionRequest) -> Result<()> {
        let Self { choice, note, .. } = *self;
        if !request.choices.iter().any(|c| c == choice)
            || note.is_some_and(|s| {
                s.len() > 2048
                    || (!s.trim().is_empty() && aq_task_comments::validate_body(s).is_err())
                    || s.contains('\0')
            })
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}
