use super::*;

impl Service {
    /// Observation only: reads (and, for a stopped check, completes) the
    /// operation named by the handle in this namespace generation.
    pub(in crate::playbooks::host_adapters) async fn host_operation_get(
        &self,
        owner: &RuntimeOwner,
        command: HostIntent<'_>,
    ) -> Result<HostResult> {
        let intent = command.intent;
        let (tx, stage, _) = self.begin(Some(owner), command).await?;
        tx.commit().await?;
        if !matches!(stage, Some(Stage::Fresh)) {
            return Err(Error::Conflict);
        }
        let handle = text(&intent.input, "handle")?;
        let missing = || HostResult {
            outcome: "refused".into(),
            output: json!({"reference": handle, "subject": subject(handle, 0)}),
        };
        if let Some(result) = self.integration_get(handle).await? {
            self.validated(intent, &result.outcome, &result.output)?;
            return Ok(result);
        }
        let Ok(operation) = Uuid::parse_str(handle) else {
            return Ok(missing());
        };
        let Some(op) = self.load_operation(&self.pool, operation).await? else {
            return Ok(missing());
        };
        let result = self.operation_status(operation, &op).await?;
        self.validated(intent, &result.outcome, &result.output)?;
        Ok(result)
    }

    async fn operation_status(&self, operation: Uuid, op: &Operation) -> Result<HostResult> {
        // The contract of `operation.get` has no `completed` shape for the
        // delivery outcomes (`synced`, `merged`, ...): a delivery operation is
        // reported while it runs, and once it has completed its answer is the
        // one `aq.delivery.completed` event, which the join already holds.
        if aq_playbook_domain::review_domain::DELIVERY_COMMANDS.contains(&op.command.as_str()) {
            let result = HostResult {
                outcome: if op.status == "running" {
                    "running"
                } else {
                    "refused"
                }
                .into(),
                output: json!({"reference": format!("delivery:{operation}"), "subject": op.request["subject"]}),
            };
            return Ok(result);
        }
        let completion = self.observe_operation(operation, op).await?;
        let result = match completion {
            Some(done) => HostResult {
                outcome: "completed".into(),
                output: json!({"outcome": done.outcome, "output": done.output}),
            },
            None => {
                let (kind, _) = self.completion_contract(&op.command)?;
                HostResult {
                    outcome: "running".into(),
                    output: json!({"reference": format!("{}:{operation}", kind.as_str().unwrap_or("operation")),
                        "subject": op.request["subject"]}),
                }
            }
        };
        Ok(result)
    }

    async fn observe_operation(
        &self,
        operation: Uuid,
        op: &Operation,
    ) -> Result<Option<HostResult>> {
        if op.status != "running" {
            return Ok(Some(HostResult {
                outcome: op.status.clone(),
                output: op.output.clone().ok_or(Error::Unknown)?,
            }));
        }
        match op.command.as_str() {
            "check.request" => self.observe_check(operation).await,
            "work.ensure" => self.observe_work(operation).await,
            "decision.request" => self.observe_decision(operation, op).await,
            // An assessment is answered through its join or expires on its own.
            "judgment.assess" => Ok(None),
            _ => self.observe_judgment(operation, op).await,
        }
    }
    async fn observe_judgment(
        &self,
        operation: Uuid,
        op: &Operation,
    ) -> Result<Option<HostResult>> {
        let now = self.now()?;
        let deadline = op.request["deadline_at_ms"]
            .as_i64()
            .ok_or(Error::Unknown)?
            .saturating_add(JUDGMENT_RECONCILE_GRACE_MS);
        if now < deadline {
            return Ok(None);
        }
        // The provider outlived its deadline and grace. Reconcile never sends it again.
        let output = json!({"provenance":null,"reason":"indeterminate","reference":format!("judgment:{operation}"),"subject":op.request["subject"]});
        self.complete_operation(
            OperationKey::new(operation, &op.command, &op.intent_id),
            host_result("failed", output),
            None,
        )
        .await
        .map(Some)
    }
}
