//! Host catalog 2.3.0 asynchronous delivery commands (Epic 0, spec 0016
//! section 6.2): `main.sync`, `branch.publish`, `pull_request.open`,
//! `pull_request.await_checks`, `pull_request.merge` and `pull_request.close`.
//! Compiled in only with the cargo feature `delivery_playbook`.
//!
//! Each command records one durable intent (`playbook_effects` row `accepted`
//! plus one `playbook_host_operations` row `running`) and answers
//! `accepted {handle}`. The driver then runs the operation off the dispatch
//! path (`collect_delivery_operations`): one bounded pass over the running
//! operations, oldest look first. An operation completes exactly once, by the
//! sealed `aq.delivery.completed` event that commits with its retained
//! completion (`complete_operation`), so a lost response, a restart or a
//! repeated pass never completes it twice and never starts a second effect:
//! every effect is idempotent under the operation's id (aq-git) or its
//! expected head (GitHub).
//!
//! The mechanism is `aq_delivery::commands`; this module only records, looks
//! up where a project's work is delivered, and decides nothing about a result.
use super::*;
use aq_delivery::{
    GitHubAdapter, GitHubRepository, UnconfiguredGitHub,
    commands::{
        Completion, Delivery, DeliveryTarget, GitTarget, PullRequestObserved, SocketGit, Step,
        bounded_text, bounded_wait_ms, without_delivery, without_target,
    },
    github::GitHubApp,
};
use aq_playbook_domain::review_domain::DELIVERY_COMMANDS;

/// The longest title and body the host sends with a pull request.
const TITLE_BYTES: usize = 256;
const BODY_BYTES: usize = 16 * 1024;

/// What the delivery commands need of the server besides the graph.
#[async_trait::async_trait]
pub trait DeliveryOps: Send + Sync {
    /// Where a project's work is delivered, or `None` when the project is not
    /// bound to a repository the server delivers to.
    async fn target(&self, project: Uuid) -> Result<Option<DeliveryTarget>>;
    /// The commands over the forge and central aq-git, or `None` when either
    /// is not configured.
    async fn commands(&self) -> Option<Delivery>;
    /// The title and the body of the task's pull request: the task's own, and
    /// its latest report's conclusion.
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
            commands: tokio::sync::OnceCell::new(),
        }
    }
}
#[async_trait::async_trait]
impl DeliveryOps for ServerDelivery {
    async fn target(&self, project: Uuid) -> Result<Option<DeliveryTarget>> {
        let binding = match crate::project_bindings::current(&self.state, project).await {
            Ok(record) => record.binding,
            Err(crate::error::AppError::NotFound(_)) => return Ok(None),
            Err(_) => return Err(Error::Unknown),
        };
        if !binding.enabled {
            return Ok(None);
        }
        // Where the repository lives on the forge is recorded by onboarding.
        let onboarded: Option<Value> = sqlx::query_scalar("SELECT state FROM repository_onboardings WHERE project_id=$1 AND repository_id=$2 AND step='bound' ORDER BY updated_at_ms DESC LIMIT 1")
            .bind(project)
            .bind(binding.repository_id)
            .fetch_optional(&self.state.store.pool)
            .await?;
        let Some(state) = onboarded else {
            return Ok(None);
        };
        let (github, branch) = (
            &state["request"]["github"],
            state["default_branch"].as_str(),
        );
        let (Some(owner), Some(name), Some(branch)) =
            (github["owner"].as_str(), github["name"].as_str(), branch)
        else {
            return Ok(None);
        };
        Ok(Some(DeliveryTarget {
            git: GitTarget {
                repository_id: binding.repository_id,
                binding_generation: binding.repository_binding_generation,
            },
            github: GitHubRepository {
                owner: owner.into(),
                name: name.into(),
            },
            default_branch: branch.into(),
        }))
    }

    async fn commands(&self) -> Option<Delivery> {
        self.commands
            .get_or_init(|| async {
                let socket = &self.state.source_retention.as_ref()?.config.git_socket;
                let github: Arc<dyn GitHubAdapter> = match GitHubApp::from_config(
                    self.state
                        .config
                        .delivery
                        .as_ref()
                        .and_then(|d| d.github.as_ref()),
                ) {
                    Ok(app) => Arc::new(app),
                    // No App: every forge call refuses, and says why.
                    Err(_) => Arc::new(UnconfiguredGitHub),
                };
                Some(Delivery::new(Arc::new(SocketGit::new(socket)), github))
            })
            .await
            .clone()
    }

    async fn pull_request_text(&self, workspace: Uuid, task: Uuid) -> Result<(String, String)> {
        let node = match &self.state.agent_access {
            Some(runtime) => crate::agent_access::settlement_node(runtime, workspace, task)
                .await
                .map_err(|_| Error::Unknown)?,
            None => None,
        };
        let title = node
            .as_ref()
            .and_then(|n| n["title"].as_str())
            .filter(|t| !t.trim().is_empty())
            .map_or_else(|| format!("AQ task {task}"), str::to_owned);
        let conclusion: Option<String> = sqlx::query_scalar(
            "SELECT convert_from(r.report_bytes,'UTF8')::jsonb->>'conclusion' \
             FROM execution_attempts a JOIN profiled_result_heads h ON h.attempt_id=a.attempt_id \
             JOIN profiled_result_reports r ON r.report_id=h.report_id \
             WHERE a.task_id=$1 \
             ORDER BY (convert_from(r.receipt_bytes,'UTF8')::jsonb->>'recorded_at_ms')::bigint DESC,r.report_id DESC LIMIT 1",
        )
        .bind(task)
        .fetch_optional(&self.state.store.pool)
        .await?
        .flatten();
        Ok((
            bounded_text(&title, TITLE_BYTES).replace(['\n', '\r'], " "),
            bounded_text(conclusion.as_deref().unwrap_or(""), BODY_BYTES).to_owned(),
        ))
    }
}

fn refusal(reason: &'static str) -> HostResult {
    HostResult {
        outcome: "refused".into(),
        output: json!({"reason": reason}),
    }
}

/// Whom a delivery command is about.
struct Scope {
    /// The id the operation is recorded under: the task, or the project.
    subject: String,
    project: Uuid,
    workspace: Uuid,
    task: Option<Uuid>,
    /// The commit the task's result was retained at, or `""`.
    head: String,
}

impl Service {
    /// Resolves the task or project a command names, or the catalog's refusal
    /// reason. A task of another project is served only with
    /// `installation_routing`.
    async fn delivery_scope(
        &self,
        name: &str,
        input: &Value,
    ) -> Result<std::result::Result<Scope, &'static str>> {
        let ports = self.ports()?;
        let own = (self.namespace.workspace_id, self.namespace.project_id);
        if name == "main.sync" {
            let Ok(project) = Uuid::parse_str(text(input, "project")?) else {
                return Ok(Err("unknown_project"));
            };
            if (project != own.1 && !self.installation_routing)
                || ports.delivery_ops.target(project).await?.is_none()
            {
                return Ok(Err("unknown_project"));
            }
            return Ok(Ok(Scope {
                subject: project.to_string(),
                project,
                workspace: own.0,
                task: None,
                head: String::new(),
            }));
        }
        let Ok(task) = Uuid::parse_str(text(input, "task")?) else {
            return Ok(Err("unknown_task"));
        };
        let Some(latest) = self.delivery.latest(task).await? else {
            return Ok(Err("unknown_task"));
        };
        if latest.project_id != own.1 && !self.installation_routing {
            return Ok(Err("unknown_task"));
        }
        Ok(Ok(Scope {
            subject: task.to_string(),
            project: latest.project_id,
            workspace: latest.workspace_id,
            task: Some(task),
            head: latest.head,
        }))
    }

    /// The pull request the host knows for `task`: its number and state.
    async fn known_pull_request(&self, task: Uuid) -> Result<Option<(u64, String)>> {
        let row: Option<(i64, String)> =
            sqlx::query_as("SELECT number,state FROM delivery_pull_requests WHERE task_id=$1")
                .bind(task)
                .fetch_optional(&self.pool)
                .await?;
        Ok(row.map(|(n, state)| (u64::try_from(n).unwrap_or(0), state)))
    }

    /// The head `branch.publish` last put on the forge for `task`: the lease
    /// of the next publication.
    async fn last_published(&self, task: Uuid) -> Result<Option<String>> {
        Ok(sqlx::query_scalar("SELECT output->>'head' FROM playbook_host_operations WHERE command_name='branch.publish' AND status='published' AND subject_id=$1 ORDER BY completed_at_ms DESC LIMIT 1")
            .bind(task.to_string())
            .fetch_optional(&self.pool)
            .await?
            .flatten())
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
        let scope = match self.delivery_scope(name, input).await? {
            Ok(scope) => scope,
            // `pull_request.await_checks` and `.close` have no refusal in the
            // catalog: for a task the host does not know there is nothing to
            // wait for or to close, which the operation completes as `closed`.
            Err(_) if matches!(name, "pull_request.await_checks" | "pull_request.close") => Scope {
                subject: text(input, "task")?.to_owned(),
                project: self.namespace.project_id,
                workspace: self.namespace.workspace_id,
                task: None,
                head: String::new(),
            },
            Err(reason) => {
                return self
                    .settle_fresh(tx, id, revision, intent, &identity, refusal(reason))
                    .await;
            }
        };
        let mut request = json!({
            "command": name,
            "project": scope.project.to_string(),
            "task": scope.task.map(|t| t.to_string()),
            "subject": subject(&scope.subject, 0),
        });
        let pull_request = match scope.task {
            Some(task) => self.known_pull_request(task).await?,
            None => None,
        };
        match name {
            "branch.publish" => {
                let Some(task) = scope.task.filter(|_| !scope.head.is_empty()) else {
                    return self
                        .settle_fresh(tx, id, revision, intent, &identity, refusal("no_branch"))
                        .await;
                };
                request["expected_old"] = json!(self.last_published(task).await?);
            }
            "pull_request.open" => {
                let task = scope.task.ok_or(Error::Unknown)?;
                let (title, body) = self
                    .ports()?
                    .delivery_ops
                    .pull_request_text(scope.workspace, task)
                    .await?;
                request["title"] = json!(title);
                request["body"] = json!(body);
            }
            "pull_request.merge" => {
                let Some((number, _)) = pull_request.filter(|(_, state)| state == "open") else {
                    return self
                        .settle_fresh(
                            tx,
                            id,
                            revision,
                            intent,
                            &identity,
                            refusal("no_pull_request"),
                        )
                        .await;
                };
                request["pull_request"] = json!(number);
                request["method"] = input["method"].clone();
                request["expected_head_sha"] = input["expected_head_sha"].clone();
            }
            "pull_request.await_checks" | "pull_request.close" => {
                request["pull_request"] = json!(pull_request.map(|(n, _)| n));
                if name == "pull_request.await_checks" {
                    let (limits, _) = aq_role_settings::delivery_limits(&self.pool)
                        .await
                        .map_err(|_| Error::Unknown)?;
                    let asked = input["deadline_ms"].as_u64().ok_or(Error::Invalid)?;
                    let wait = bounded_wait_ms(asked, limits.longest_check_wait_ms);
                    request["deadline_at_ms"] = json!(
                        self.now()?
                            .saturating_add(i64::try_from(wait).unwrap_or(i64::MAX))
                    );
                    request["required"] = input["required"].clone();
                }
            }
            _ => {}
        }
        let operation = self.operation(intent)?;
        let accepted = HostResult {
            outcome: "accepted".into(),
            output: json!({"handle": operation.to_string()}),
        };
        self.record(
            &mut tx,
            id,
            revision,
            intent,
            &identity,
            &accepted.outcome,
            &accepted.output,
        )
        .await?;
        sqlx::query("INSERT INTO playbook_host_operations(operation_id,namespace_id,generation,intent_id,command_name,subject_id,subject_revision,check_id,request,status,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,0,NULL,$7,'running',$8)")
            .bind(operation)
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(&intent.id)
            .bind(name)
            .bind(&scope.subject)
            .bind(&request)
            .bind(self.now()?)
            .execute(&mut *tx)
            .await?;
        self.commit_fresh(tx, id, intent).await?;
        Ok(accepted)
    }

    /// One bounded pass over the running delivery operations, the least
    /// recently looked at first: each is run one step. Returns how many
    /// completed. A step that cannot finish leaves the operation running for
    /// a later pass.
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
        let now = self.now()?;
        let names: Vec<String> = DELIVERY_COMMANDS.iter().map(|c| c.to_string()).collect();
        let operations: Vec<Uuid> = sqlx::query_scalar("UPDATE playbook_host_operations SET observed_at_ms=$4 WHERE operation_id IN (SELECT operation_id FROM playbook_host_operations WHERE namespace_id=$1 AND generation=$2 AND command_name=ANY($5) AND status='running' ORDER BY COALESCE(observed_at_ms,created_at_ms),operation_id LIMIT $3) RETURNING operation_id")
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(limit as i64)
            .bind(now)
            .bind(&names)
            .fetch_all(&mut *tx)
            .await?;
        tx.commit().await?;
        let mut completed = 0;
        for operation in operations {
            match self.run_delivery(operation).await {
                Ok(Some(_)) => completed += 1,
                Ok(None) => {}
                Err(Error::Overloaded) => return Err(Error::Overloaded),
                Err(_) => {}
            }
        }
        Ok(completed)
    }

    /// Runs one step of a running delivery operation and, when the command
    /// has its answer, completes it (exactly once). `None` while it runs.
    async fn run_delivery(&self, operation: Uuid) -> Result<Option<HostResult>> {
        let Some(op) = self.load_operation(&self.pool, operation).await? else {
            return Ok(None);
        };
        if !DELIVERY_COMMANDS.contains(&op.command.as_str()) {
            return Err(Error::Invalid);
        }
        if op.status != "running" {
            return Ok(Some(HostResult {
                outcome: op.status,
                output: op.output.ok_or(Error::Unknown)?,
            }));
        }
        let ports = self.ports()?;
        let request = &op.request;
        let uuid = |field: &str| {
            request[field]
                .as_str()
                .and_then(|v| Uuid::parse_str(v).ok())
        };
        let project = uuid("project").ok_or(Error::Unknown)?;
        let task = uuid("task");
        let (target, commands) = (
            ports.delivery_ops.target(project).await?,
            ports.delivery_ops.commands().await,
        );
        let now = self.now()?;
        let command = op.command.as_str();
        // `None`: nothing decided yet.
        let step: Option<Step> = match (&target, &commands) {
            (None, _) => Some(without_target(command)),
            (Some(_), None) => without_delivery(command),
            (Some(target), Some(delivery)) => {
                let run = delivery
                    .step(operation, command, request, target, now)
                    .await
                    .ok_or(Error::Unknown)?;
                if let (Some(task), Some(seen)) = (task, &run.observed) {
                    self.record_pull_request(task, seen).await?;
                }
                run.step
            }
        };
        // `await_checks` with no forge keeps waiting until its deadline.
        let step = match step {
            Some(step) => step,
            None => match request["deadline_at_ms"].as_i64() {
                Some(at) if command == "pull_request.await_checks" && now >= at => {
                    Step::Done(Completion::new("timeout", json!({})))
                }
                _ => return Ok(None),
            },
        };
        let Step::Done(completion) = step else {
            return Ok(None);
        };
        if let (Some(task), Some(seen)) = (task, &completion.pull_request) {
            self.record_pull_request(task, seen).await?;
        }
        if command == "main.sync" && completion.outcome == "synced" {
            self.record_main_head(project, &completion.output).await?;
        }
        self.complete_operation(
            operation,
            &op.command,
            &op.intent_id,
            HostResult {
                outcome: completion.outcome.into(),
                output: completion.output,
            },
        )
        .await
        .map(Some)
    }

    /// The pull request as the last command left it (`work.read`
    /// `pull_request`). A command that did not read the url leaves the stored
    /// one.
    async fn record_pull_request(&self, task: Uuid, seen: &PullRequestObserved) -> Result<()> {
        let (now, number) = (
            self.now()?,
            i64::try_from(seen.number).map_err(|_| Error::Invalid)?,
        );
        let failed = json!(seen.checks_failed);
        let query = match &seen.url {
            Some(url) => sqlx::query("INSERT INTO delivery_pull_requests(task_id,number,url,state,mergeable,checks_state,checks_failed,updated_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT(task_id) DO UPDATE SET number=$2,url=$3,state=$4,mergeable=$5,checks_state=$6,checks_failed=$7,updated_at_ms=$8")
                .bind(task).bind(number).bind(url.clone()).bind(seen.state).bind(seen.mergeable).bind(seen.checks_state).bind(failed).bind(now),
            None => sqlx::query("UPDATE delivery_pull_requests SET state=$3,mergeable=$4,checks_state=$5,checks_failed=$6,updated_at_ms=$7 WHERE task_id=$1 AND number=$2")
                .bind(task).bind(number).bind(seen.state).bind(seen.mergeable).bind(seen.checks_state).bind(failed).bind(now),
        };
        query.execute(&self.pool).await?;
        Ok(())
    }

    /// The commit main points at after a sync (`work.read` `main_head`).
    async fn record_main_head(&self, project: Uuid, output: &Value) -> Result<()> {
        let Some(head) = output["head"].as_str().filter(|h| {
            h.len() == 40
                && h.bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        }) else {
            return Ok(());
        };
        sqlx::query("INSERT INTO delivery_main_heads(project_id,head,observed_at_ms) VALUES($1,$2,$3) ON CONFLICT(project_id) DO UPDATE SET head=$2,observed_at_ms=$3")
            .bind(project)
            .bind(head)
            .bind(self.now()?)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
