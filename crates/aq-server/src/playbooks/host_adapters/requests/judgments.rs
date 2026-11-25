use super::*;
use aq_playbook_domain::judgment_admission::JudgmentAdmission;

impl Service {
    pub(in crate::playbooks::host_adapters) async fn judgment_request(
        &self,
        owner: &RuntimeOwner,
        command: HostIntent<'_>,
    ) -> Result<HostResult> {
        let intent = command.intent;
        let ports = self.ports()?;
        let (tx, stage, identity) = self.begin(Some(owner), command).await?;
        let operation = self.operation(intent)?;
        match stage.ok_or(Error::Unknown)? {
            Stage::Retained(result) => {
                tx.commit().await?;
                // Lost response: the original request is never re-sent.
                return Ok(result);
            }
            Stage::Pending(_) => return Err(Error::Conflict),
            Stage::Fresh => {}
        }
        let admission = JudgmentAdmission::new(operation, intent)?;
        let stale = self
            .judgment_subject_stale(ports, admission.subject())
            .await?;
        let profile = ports.judgments.as_ref().and_then(|provider| {
            provider
                .credential_profile(self.namespace.workspace_id, self.namespace.project_id)
                .map(|profile| (provider.clone(), profile))
        });
        if let Some(refusal) = admission.refused(profile.is_some(), stale)? {
            return self
                .settle_fresh(tx, command.retained(&identity), refusal)
                .await;
        }
        let (provider, credential_profile) = profile.ok_or(Error::Unknown)?;
        let request = admission.request(credential_profile)?;
        let estimated_input_tokens = provider.estimate_input_tokens(&request);
        if estimated_input_tokens > request.max_input_tokens {
            return self
                .settle_fresh(
                    tx,
                    command.retained(&identity),
                    admission.refusal("budget_exceeded"),
                )
                .await;
        }
        let job = admission.job(request)?;
        let record = job.admission_record(estimated_input_tokens, self.now()?);
        let accepted = self
            .record_operation(
                tx,
                command.retained(&identity),
                OperationRequest::new(None, &record),
            )
            .await?;
        // A committed request runs once, even if the caller loses the handle.
        let service = self.clone();
        tokio::spawn(async move {
            let _ = service.run_judgment(provider, job).await;
        });
        Ok(accepted)
    }

    async fn judgment_subject_stale(&self, ports: &HostPorts, subject: &Value) -> Result<bool> {
        let Ok(task) = Uuid::parse_str(text(subject, "id")?) else {
            return Ok(false);
        };
        let Some(node) = ports.graph.node(self.namespace.workspace_id, task).await? else {
            return Ok(false);
        };
        Ok(node_version(&node)? != revision_of(subject)?)
    }
    /// Sends one accepted judgment's provider request, bounded by its budget
    /// deadline, and completes the operation exactly once.
    async fn run_judgment(
        &self,
        provider: Arc<dyn JudgmentProvider>,
        job: JudgmentJob,
    ) -> Result<HostResult> {
        let reply = tokio::time::timeout(
            Duration::from_millis(job.request.deadline_ms),
            provider.complete(&job.request),
        )
        .await;
        let result = job.completion(reply.ok())?;
        self.complete_operation(
            OperationKey::new(job.operation, &job.command, &job.intent_id),
            result,
            None,
        )
        .await
    }
}
