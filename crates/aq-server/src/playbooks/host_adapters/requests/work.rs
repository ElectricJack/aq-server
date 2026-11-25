use super::*;

impl Service {
    /// Tracks one catalog profile 7 ensured work node as a running operation,
    /// in the tx of its accepted effect row. It completes exactly once, by
    /// aq.operation.updated, when the graph task reaches a terminal state.
    pub(in crate::playbooks::host_adapters) async fn track_work(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        intent: &Intent,
        work: &Value,
    ) -> Result<()> {
        let subject = &work["subject"];
        // `motivation` is the ensure input subject (for review work, the
        // reviewed candidate); D11 publication evidence binds to it.
        let request = json!({"kind": text(&intent.input, "kind")?,
            "reference": work["reference"], "subject": subject,
            "motivation": intent.input["subject"]});
        Operations(&self.namespace)
            .record(
                tx,
                self.operation(intent)?,
                &intent.id,
                "work.ensure",
                subject,
                None,
                &request,
                self.now()?,
            )
            .await
    }

    /// Reads the tracked work node and, once terminal, completes the
    /// operation (exactly once; a lost race returns the retained completion).
    /// `None` while the work is still open.
    pub(in crate::playbooks::host_adapters) async fn observe_work(
        &self,
        operation: Uuid,
    ) -> Result<Option<HostResult>> {
        let Some(op) = self.load_operation(&self.pool, operation).await? else {
            return Ok(None);
        };
        if op.command != "work.ensure" {
            return Err(Error::Invalid);
        }
        if op.status != "running" {
            return op.retained().map(Some);
        }
        let task =
            Uuid::parse_str(text(&op.request["subject"], "id")?).map_err(|_| Error::Unknown)?;
        let node = self
            .ports()?
            .graph
            .node(self.namespace.workspace_id, task)
            .await?;
        let Some(result) = work_completion(&op.request, node.as_ref())? else {
            return Ok(None);
        };
        self.complete_operation(
            OperationKey::new(operation, &op.command, &op.intent_id),
            result,
            None,
        )
        .await
        .map(Some)
    }

    /// Bounded round-robin over running catalog profile 7 work operations:
    /// each pass observes the least recently observed ones and completes those
    /// whose graph task is terminal. Returns the number completed. A graph
    /// read failure leaves that operation running for a later pass.
    pub async fn collect_work_completions(
        &self,
        owner: &RuntimeOwner,
        limit: u32,
    ) -> Result<usize> {
        if !self.profile.is_host2() || self.host.is_none() {
            return Ok(0);
        }
        if !(1..=2).contains(&limit) {
            return Err(Error::Invalid);
        }
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let operations = Operations(&self.namespace)
            .observe_work(&mut tx, limit, self.now()?)
            .await?;
        tx.commit().await?;
        let mut completed = 0;
        for operation in operations {
            match self.observe_work(operation).await {
                Ok(Some(_)) => completed += 1,
                Ok(None) => {}
                Err(Error::Overloaded) => return Err(Error::Overloaded),
                Err(_) => {}
            }
        }
        Ok(completed)
    }
}
