//! Host catalog 2.3.0 asynchronous delivery commands (Epic 0, spec 0016
//! section 6.2): `main.sync`, `branch.publish`, `pull_request.open`,
//! `pull_request.await_checks`, `pull_request.merge` and `pull_request.close`.
//! Compiled in only with the cargo feature `delivery_playbook`.
//!
//! Each command records one durable intent (`playbook_effects` row `accepted`
//! plus one `playbook_host_operations` row `running`) and answers
//! `accepted {handle}`. The driver runs the operations off the dispatch path
//! (`collect_delivery_operations`), and each completes exactly once, by the
//! sealed `aq.delivery.completed` event committed with its retained completion
//! (`complete_operation`). Every effect is idempotent under the operation's id
//! (aq-git) or its expected head (GitHub). The mechanism, the frozen request
//! and the facts it records are `aq_delivery::commands`; this module only does
//! the operation bookkeeping and decides nothing about a result.
use super::*;
use aq_delivery::commands::{self as delivery, Delivery, DeliveryTarget, GitTarget};
use aq_playbook_domain::{
    delivery_runs::{self, DeliveryRuns},
    review_domain::DELIVERY_COMMANDS,
};
pub use delivery_runs::DeliveryOps;

/// Production port: the project bindings, the onboarding record, the
/// configured GitHub App and the aq-git socket.
pub struct ServerDelivery {
    state: crate::AppState,
    commands: tokio::sync::OnceCell<Option<Delivery>>,
}
impl ServerDelivery {
    pub fn new(state: crate::AppState) -> Self {
        Self {
            state,
            commands: Default::default(),
        }
    }
}
#[async_trait::async_trait]
impl DeliveryOps for ServerDelivery {
    async fn target(&self, project: Uuid) -> Result<Option<DeliveryTarget>> {
        let binding = match crate::project_bindings::current(&self.state, project).await {
            Ok(record) if record.binding.enabled => record.binding,
            Ok(_) | Err(crate::error::AppError::NotFound(_)) => return Ok(None),
            Err(_) => return Err(Error::Unknown),
        };
        let git = GitTarget {
            repository_id: binding.repository_id,
            binding_generation: binding.repository_binding_generation,
        };
        Ok(delivery::onboarded_target(&self.state.store.pool, project, git).await?)
    }

    async fn commands(&self) -> Option<Delivery> {
        let init = || async {
            let socket = &self.state.source_retention.as_ref()?.config.git_socket;
            let app = self.state.config.delivery.as_ref();
            Some(Delivery::configured(
                socket,
                app.and_then(|d| d.github.as_ref()),
            ))
        };
        self.commands.get_or_init(init).await.clone()
    }

    async fn pull_request_text(&self, workspace: Uuid, task: Uuid) -> Result<(String, String)> {
        let node = match &self.state.agent_access {
            Some(runtime) => crate::agent_access::settlement_node(runtime, workspace, task)
                .await
                .map_err(|_| Error::Unknown)?,
            None => None,
        };
        let title = node.as_ref().and_then(|n| n["title"].as_str());
        Ok(delivery::pull_request_text(&self.state.store.pool, task, title).await?)
    }
}

impl Service {
    fn delivery_runs(&self) -> Result<DeliveryRuns<'_>> {
        Ok(DeliveryRuns {
            pool: &self.pool,
            namespace: &self.namespace,
            installation_routing: self.installation_routing,
            results: self.delivery.as_ref(),
            operations: self.ports()?.delivery_ops.as_ref(),
        })
    }

    /// Bind the engine's actual fan-out run IDs to the original sealed result event.
    pub async fn record_delivery_runs(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        event: &EventEnvelope,
        runs: &Value,
    ) -> Result<()> {
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        delivery_runs::record_origins(&mut tx, &self.namespace, id, event, runs).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Dispatches one of the six delivery commands.
    pub(super) async fn delivery_command(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<HostResult> {
        let (mut tx, stage, identity) = self.begin(Some(owner), id, revision, intent).await?;
        match stage.ok_or(Error::Unknown)? {
            Stage::Retained(result) => {
                tx.commit().await?;
                return Ok(result);
            }
            Stage::Pending(_) => return Err(Error::Unknown),
            Stage::Fresh => {}
        }
        let runs = self.delivery_runs()?;
        let prepared = match runs.prepare(&mut tx, intent, || self.now()).await? {
            Ok(prepared) => prepared,
            Err(answer) => {
                return self
                    .settle_fresh(tx, id, revision, intent, &identity, answer)
                    .await;
            }
        };
        let operation = self.operation(intent)?;
        let accepted = host_result("accepted", json!({"handle": operation.to_string()}));
        let (outcome, output) = (&accepted.outcome, &accepted.output);
        self.record(&mut tx, id, revision, intent, &identity, outcome, output)
            .await?;
        runs.enqueue(&mut tx, operation, intent, prepared, self.now()?)
            .await?;
        self.commit_fresh(tx, id, intent).await?;
        Ok(accepted)
    }

    /// One bounded pass over at most `limit` running delivery operations, the
    /// least recently looked at first: each is run one step, and completed
    /// (exactly once) when the command has its answer. Returns how many
    /// completed; the others run on in a later pass.
    pub async fn collect_delivery_operations(
        &self,
        owner: &RuntimeOwner,
        limit: u32,
    ) -> Result<usize> {
        if !self.profile.is_delivery() || self.host.is_none() {
            return Ok(0);
        }
        if !(1..=8).contains(&limit) {
            return Err(Error::Invalid);
        }
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let (namespace, generation) = (self.namespace.namespace_id, self.namespace.generation);
        let now = self.now()?;
        let due = delivery::due_operations(
            &mut tx,
            namespace,
            generation,
            &DELIVERY_COMMANDS,
            limit,
            now,
        )
        .await?;
        tx.commit().await?;
        let mut completed = 0;
        for operation in due {
            match self.run_delivery(operation).await {
                Ok(true) => completed += 1,
                Err(Error::Overloaded) => return Err(Error::Overloaded),
                Ok(false) | Err(_) => {}
            }
        }
        Ok(completed)
    }

    /// Runs one step of a running delivery operation; `true` once completed.
    async fn run_delivery(&self, operation: Uuid) -> Result<bool> {
        let Some(op) = self.load_operation(&self.pool, operation).await? else {
            return Ok(false);
        };
        if !DELIVERY_COMMANDS.contains(&op.command.as_str()) {
            return Err(Error::Invalid);
        }
        if op.status != "running" {
            return Ok(true);
        }
        let mut fence = self.tx().await?;
        let completion = self
            .delivery_runs()?
            .step(&mut fence, operation, &op.command, &op.request, || {
                self.now()
            })
            .await?;
        fence.commit().await?;
        let Some(completion) = completion else {
            return Ok(false);
        };
        let result = host_result(completion.outcome, completion.output);
        self.complete_operation(
            operation,
            &op.command,
            &op.intent_id,
            result,
            completion.diagnostic,
        )
        .await?;
        Ok(true)
    }
}
