//! Host catalog 2.3.0 asynchronous delivery commands (Epic 0, spec 0016
//! section 6.2): `main.sync`, `branch.publish`, `pull_request.open`,
//! `pull_request.await_checks`, `pull_request.merge` and `pull_request.close`.
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
        let socket = &self.state.source_retention.as_ref()?.config.git_socket;
        let init = || async {
            let app = self.state.config.delivery.as_ref();
            Some(Delivery::configured(
                socket,
                app.and_then(|d| d.github.as_ref()),
            ))
        };
        let commands = self.commands.get_or_init(init).await.clone()?;
        // Read for each step, so an owner's change applies to the next aq-git call.
        let (limits, _) = aq_role_settings::delivery_limits(&self.state.store.pool)
            .await
            .ok()?;
        Some(commands.git_waiting(
            socket,
            std::time::Duration::from_millis(limits.git_transfer_timeout_ms),
        ))
    }

    async fn replay_publication(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        origin: &aq_playbook_domain::host10::ResultReady,
        command: &str,
    ) -> Result<Option<delivery::Completion>> {
        use crate::task_workspace_delivery as workspaces;
        if !workspaces::has_scope(
            &self.state,
            tx,
            origin.attempt_id,
            origin.project_id,
            origin.task_id,
        )
        .await
        .map_err(|_| Error::Unknown)?
        {
            return Ok(None);
        }
        let result =
            crate::profiled_execution::results::load_locked(&self.state, tx, origin.result_id)
                .await
                .map_err(|_| Error::Unknown)?;
        workspaces::replay_home_publication(&self.state, tx, &result, command)
            .await
            .map_err(|_| Error::Unknown)
    }

    async fn merge_readiness(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        origin: &aq_playbook_domain::host10::ResultReady,
    ) -> Result<delivery_runs::MergeReadiness> {
        use crate::task_workspace_delivery as workspaces;
        if !workspaces::has_scope(
            &self.state,
            tx,
            origin.attempt_id,
            origin.project_id,
            origin.task_id,
        )
        .await
        .map_err(|_| Error::Unknown)?
        {
            return Ok(delivery_runs::MergeReadiness::Proceed);
        }
        let result =
            crate::profiled_execution::results::load_locked(&self.state, tx, origin.result_id)
                .await
                .map_err(|_| Error::Unknown)?;
        workspaces::merge_readiness(&self.state, tx, &result)
            .await
            .map_err(|_| Error::Unknown)
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
    pub(super) fn delivery_runs(&self) -> Result<DeliveryRuns<'_>> {
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
        command: HostIntent<'_>,
    ) -> Result<HostResult> {
        let id = command.grant;
        let intent = command.intent;
        let (mut tx, stage, identity) = self.begin(Some(owner), command).await?;
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
                    .settle_fresh(tx, command.retained(&identity), answer)
                    .await;
            }
        };
        let operation = self.operation(intent)?;
        let accepted = host_result("accepted", json!({"handle": operation.to_string()}));
        let (outcome, output) = (&accepted.outcome, &accepted.output);
        self.record(
            &mut tx,
            command.retained(&identity),
            EffectReceipt::new(outcome, output),
        )
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
        #[allow(unused_mut)]
        let mut commands = DELIVERY_COMMANDS.to_vec();
        #[cfg(feature = "project_environments")]
        commands.extend(aq_playbook_domain::review_domain::ENVIRONMENT_COMMANDS);
        let due = delivery::due_operations(
            &mut tx,
            delivery::DueOperations {
                namespace,
                generation,
                commands: &commands,
                limit,
                now_ms: now,
            },
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
        if op.status != "running" {
            return Ok(true);
        }
        #[cfg(feature = "project_environments")]
        if aq_playbook_domain::review_domain::ENVIRONMENT_COMMANDS.contains(&op.command.as_str()) {
            let Some(result) = self
                .environment_step(operation, &op.command, &op.request)
                .await?
            else {
                return Ok(false);
            };
            let key = OperationKey::new(operation, &op.command, &op.intent_id);
            self.complete_operation(key, result, None).await?;
            return Ok(true);
        }
        if !DELIVERY_COMMANDS.contains(&op.command.as_str()) {
            return Err(Error::Invalid);
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
            OperationKey::new(operation, &op.command, &op.intent_id),
            result,
            completion.diagnostic,
        )
        .await?;
        Ok(true)
    }
}
