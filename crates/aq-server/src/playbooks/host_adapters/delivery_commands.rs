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
use aq_delivery::commands::{self as delivery, Accepting, Delivery, DeliveryTarget, GitTarget};
use aq_playbook_domain::{
    delivery_store::Latest, host10::ResultReady, review_domain::DELIVERY_COMMANDS,
};

/// What the delivery commands need of the server besides the graph.
#[async_trait::async_trait]
pub trait DeliveryOps: Send + Sync {
    /// Where a project's work is delivered, or `None` when the project is not
    /// bound to a repository the server delivers to.
    async fn target(&self, project: Uuid) -> Result<Option<DeliveryTarget>>;
    /// The commands over the forge and central aq-git, or `None` when central
    /// git is not configured.
    async fn commands(&self) -> Option<Delivery>;
    /// The title and the body of the task's pull request.
    async fn pull_request_text(&self, workspace: Uuid, task: Uuid) -> Result<(String, String)>;
}

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

fn delivery_error(error: aq_delivery::DeliveryError) -> Error {
    match error {
        aq_delivery::DeliveryError::InvalidInput(_) => Error::Invalid,
        _ => Error::Unknown,
    }
}

impl Service {
    /// Bind the engine's actual fan-out run IDs to the original sealed result event.
    /// Run inputs and caller-supplied IDs cannot establish this relationship.
    pub async fn record_delivery_runs(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        event: &EventEnvelope,
        runs: &Value,
    ) -> Result<()> {
        if event.producer != "aq.host.bridge@1" || event.contract.name != "aq.work.result_ready" {
            return Err(Error::Invalid);
        }
        let ready: ResultReady = serde_json::from_value(event.payload.clone())?;
        let runs = runs.as_array().ok_or(Error::Invalid)?;
        // Receipts are bounded at 16 KiB. A delivery binding normally has one trigger.
        if runs.len() > 128
            || runs.iter().any(|run| {
                !run.as_str()
                    .is_some_and(|id| id.starts_with("event-sha256:") && id.len() == 77)
            })
        {
            return Err(Error::Invalid);
        }
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let proof = json!({"result_ready": ready, "runs": runs});
        let changed = sqlx::query(
            "UPDATE playbook_outbox SET engine_receipt=engine_receipt || $5::jsonb
             WHERE namespace_id=$1 AND generation=$2 AND producer='aq.host.bridge@1'
             AND event_id=$3 AND event_digest=$4 AND status='engine_accepted'
             AND (NOT engine_receipt ? 'result_ready' OR engine_receipt->'result_ready'=$5::jsonb->'result_ready')",
        )
        .bind(self.namespace.namespace_id)
        .bind(self.namespace.generation)
        .bind(id)
        .bind(digest(event)?)
        .bind(proof)
        .execute(&mut *tx)
        .await?;
        if changed.rows_affected() != 1 {
            return Err(Error::Conflict);
        }
        tx.commit().await?;
        Ok(())
    }

    pub(super) async fn delivery_origin(
        &self,
        db: &mut PgConnection,
        intent: &Intent,
    ) -> Result<Option<ResultReady>> {
        if !intent.run.starts_with("event-") {
            return Ok(None);
        }
        let ready: Option<Value> = sqlx::query_scalar(
            "SELECT engine_receipt->'result_ready' FROM playbook_outbox
             WHERE namespace_id=$1 AND generation=$2 AND producer='aq.host.bridge@1'
             AND (engine_receipt->'runs') ? $3",
        )
        .bind(self.namespace.namespace_id)
        .bind(self.namespace.generation)
        .bind(&intent.run)
        .fetch_optional(db)
        .await?;
        ready
            .map(serde_json::from_value)
            .transpose()
            .map_err(Into::into)
    }

    /// The command's scope and latest task result, or the catalog refusal.
    /// Tasks of other projects require `installation_routing`.
    async fn delivery_scope(
        &self,
        name: &str,
        input: &Value,
    ) -> Result<std::result::Result<(String, Uuid, Uuid, Option<(Uuid, Latest)>), &'static str>>
    {
        let own = (self.namespace.workspace_id, self.namespace.project_id);
        if name == "main.sync" {
            let Ok(project) = Uuid::parse_str(text(input, "project")?) else {
                return Ok(Err("unknown_project"));
            };
            if (project != own.1 && !self.installation_routing)
                || self.ports()?.delivery_ops.target(project).await?.is_none()
            {
                return Ok(Err("unknown_project"));
            }
            return Ok(Ok((project.to_string(), project, own.0, None)));
        }
        let Ok(task) = Uuid::parse_str(text(input, "task")?) else {
            return Ok(Err("unknown_task"));
        };
        Ok(match self.delivery.latest(task).await? {
            Some(l) if l.project_id == own.1 || self.installation_routing => Ok((
                task.to_string(),
                l.project_id,
                l.workspace_id,
                Some((task, l)),
            )),
            _ => Err("unknown_task"),
        })
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
        let (name, input) = (intent.command.name.as_str(), &intent.input);
        let refused = |reason: &str| host_result("refused", json!({"reason": reason}));
        let (subject_id, project, workspace, result) =
            match self.delivery_scope(name, input).await? {
                Ok(scope) => scope,
                // `.await_checks` and `.close` have no refusal in the catalog:
                // for a task the host does not know there is nothing to wait
                // for or to close, which the operation completes as `closed`.
                Err(_) if matches!(name, "pull_request.await_checks" | "pull_request.close") => {
                    let ns = &self.namespace;
                    let task = text(input, "task")?.to_owned();
                    (task, ns.project_id, ns.workspace_id, None)
                }
                Err(reason) => {
                    let result = refused(reason);
                    return self
                        .settle_fresh(tx, id, revision, intent, &identity, result)
                        .await;
                }
            };
        let (task, latest) = result
            .as_ref()
            .map(|(task, latest)| (*task, latest))
            .unzip();
        let origin = if name == "main.sync" || task.is_none() {
            None
        } else {
            let origin = self.delivery_origin(&mut tx, intent).await?;
            if intent.run.starts_with("event-") {
                let origin = origin.ok_or(Error::Forbidden)?;
                if task != Some(origin.task_id)
                    || origin.project_id != project
                    || !self.delivery.lock_current(&mut tx, &origin).await?
                {
                    return Err(Error::Conflict);
                }
                Some(origin)
            } else {
                return Err(Error::Forbidden);
            }
        };
        let text = match (name, task) {
            ("pull_request.open", Some(task)) => Some(
                self.ports()?
                    .delivery_ops
                    .pull_request_text(workspace, task)
                    .await?,
            ),
            _ => None,
        };
        let longest_check_wait_ms = if name == "pull_request.await_checks" {
            let (limits, _) = aq_role_settings::delivery_limits(&self.pool)
                .await
                .map_err(|_| Error::Unknown)?;
            Some(limits.longest_check_wait_ms)
        } else {
            None
        };
        let accepting = Accepting {
            command: name,
            project,
            task,
            head: latest.map_or("", |record| record.head.as_str()),
            subject: subject(&subject_id, 0),
            text,
            longest_check_wait_ms,
            merge_target: if name == "pull_request.merge" {
                self.ports()?.delivery_ops.target(project).await?
            } else {
                None
            },
            result_ids: origin.map(|event| (event.attempt_id, event.result_id)),
        };
        let mut request = match delivery::frozen_request(&self.pool, accepting, input, self.now()?)
            .await
            .map_err(delivery_error)?
        {
            Ok(request) => request,
            Err(reason) => {
                let result = refused(reason);
                return self
                    .settle_fresh(tx, id, revision, intent, &identity, result)
                    .await;
            }
        };
        if let Some(origin) = origin {
            request["result_ready"] = json!(origin);
        }
        let operation = self.operation(intent)?;
        let accepted = host_result("accepted", json!({"handle": operation.to_string()}));
        let (outcome, output) = (&accepted.outcome, &accepted.output);
        self.record(&mut tx, id, revision, intent, &identity, outcome, output)
            .await?;
        sqlx::query("INSERT INTO playbook_host_operations(operation_id,namespace_id,generation,intent_id,command_name,subject_id,subject_revision,check_id,request,status,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,0,NULL,$7,'running',$8)")
            .bind(operation)
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(&intent.id)
            .bind(name)
            .bind(&subject_id)
            .bind(&request)
            .bind(self.now()?)
            .execute(&mut *tx)
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
        let ports = self.ports()?;
        let project = op.request["project"]
            .as_str()
            .and_then(|p| Uuid::parse_str(p).ok());
        let target = ports
            .delivery_ops
            .target(project.ok_or(Error::Unknown)?)
            .await?;
        let commands = ports.delivery_ops.commands().await;
        let (request, now) = (&op.request, self.now()?);
        // Hold the same attempt/report-head fence the result writer uses until
        // this quantum's external operation has returned.
        let mut fence = self.tx().await?;
        if op.command != "main.sync"
            && (!request["task"].is_null()
                || !matches!(
                    op.command.as_str(),
                    "pull_request.await_checks" | "pull_request.close"
                ))
        {
            let current =
                match serde_json::from_value::<ResultReady>(request["result_ready"].clone()) {
                    Ok(origin) => self.delivery.lock_current(&mut fence, &origin).await?,
                    Err(_) => false,
                };
            if !current {
                fence.commit().await?;
                let answer = match op.command.as_str() {
                    "pull_request.await_checks" => host_result("timeout", json!({})),
                    "pull_request.close" => host_result("closed", json!({})),
                    "branch.publish" => host_result("refused", json!({"reason": "no_branch"})),
                    "pull_request.open" => {
                        host_result("refused", json!({"reason": "unknown_task"}))
                    }
                    _ => host_result("refused", json!({"reason": "head_changed"})),
                };
                self.complete_operation(operation, &op.command, &op.intent_id, answer)
                    .await?;
                return Ok(true);
            }
        }
        let step = delivery::run_step(
            &self.pool,
            commands.as_ref(),
            target.as_ref(),
            operation,
            &op.command,
            request,
            now,
        );
        let completion = step.await.map_err(delivery_error)?;
        fence.commit().await?;
        let Some(completion) = completion else {
            return Ok(false);
        };
        let result = host_result(completion.outcome, completion.output);
        self.complete_operation(operation, &op.command, &op.intent_id, result)
            .await?;
        Ok(true)
    }
}
