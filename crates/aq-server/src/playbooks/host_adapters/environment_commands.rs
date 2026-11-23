//! Host catalog 2.5.0 `environment.build` and `environment.admit` (spec 0017
//! §6.3-6.4), on the `delivery_commands` pattern: one accepted intent and
//! one running `playbook_host_operations` row, completed once by the sealed
//! `aq.environment.completed`. What each answer is, is `aq_environments::commands`.
use super::*;
use aq_environments::commands::{self as environments, Bound, EnvironmentOps, Target};
use aq_playbook_domain::delivery_runs::Prepared;

/// Production port: the project's repository on aq-git, and its binding.
pub struct ServerEnvironments(pub crate::AppState);
#[async_trait::async_trait]
impl EnvironmentOps for ServerEnvironments {
    async fn recipes(&self, project: Uuid, commit: &str) -> Option<Vec<(String, String)>> {
        let b = crate::project_bindings::current(&self.0, project)
            .await
            .ok()?
            .binding;
        let socket = &self.0.source_retention.as_ref()?.config.git_socket;
        let generation = b.repository_binding_generation;
        environments::recipes_at(socket, b.repository_id, generation, commit).await
    }
    async fn bind(
        &self,
        actor: Uuid,
        project: Uuid,
        add: &[String],
        default: Option<&str>,
        op: Uuid,
    ) -> Bound {
        use crate::error::AppError;
        let Ok(record) = crate::project_bindings::current(&self.0, project).await else {
            return Bound::Refused;
        };
        let mut binding = record.binding;
        let new: Vec<String> = add
            .iter()
            .filter(|id| !binding.allowed_environments.contains(id))
            .cloned()
            .collect();
        let moved = default.is_some_and(|d| binding.default_environment.as_deref() != Some(d));
        if new.is_empty() && !moved {
            return Bound::Done;
        }
        binding.allowed_environments.extend(new);
        binding.default_environment = default.map(Into::into).or(binding.default_environment);
        let key = json!({"environment.admit": op, "revision": record.revision});
        let operation =
            aq_playbook_domain::stable_id("aq.environment.bind", op, 0, &key.to_string());
        let Ok(operation) = operation else {
            return Bound::Retry;
        };
        match crate::project_bindings::put_binding(
            &self.0,
            actor,
            operation,
            record.revision,
            binding,
        )
        .await
        {
            Ok(_) => Bound::Done,
            Err(AppError::BadRequest(_) | AppError::NotFound(_)) => Bound::Refused,
            // A binding naming an absent or revoked worker never validates;
            // other conflicts (a concurrent change, a peer down) pass.
            Err(AppError::Conflict(m)) if m.contains("absent or revoked worker identity") => {
                Bound::Refused
            }
            Err(_) => Bound::Retry,
        }
    }
}

impl Service {
    /// Dispatches `environment.build` or `environment.admit`.
    pub(super) async fn environment_command(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<HostResult> {
        let (input, build) = (&intent.input, intent.command.name == "environment.build");
        let ours = |p: Uuid| p == self.namespace.project_id || self.installation_routing;
        // Exactly one of `subject` and `project`, which the schema cannot say.
        let found = match (input.get("subject"), input.get("project")) {
            (Some(subject), None) => {
                let task = Uuid::parse_str(text(subject, "id")?).ok();
                let latest = match task {
                    Some(task) => self.delivery.latest(task).await?,
                    None => None,
                };
                match latest.filter(|l| ours(l.project_id)) {
                    Some(l) => Ok((
                        Target::Task {
                            project: l.project_id,
                            head: l.head,
                        },
                        task,
                    )),
                    None => Err("task"),
                }
            }
            (None, Some(project)) => match project.as_str().and_then(|p| Uuid::parse_str(p).ok()) {
                Some(project) if ours(project) => Ok((Target::Project(project), None)),
                _ => Err("project"),
            },
            _ => return Err(Error::Invalid),
        };
        // aq-git is read before the namespace is locked.
        let ops = self.ports()?.environment_ops.as_ref();
        let listing = match (&found, build) {
            (Ok((target, _)), true) => Some(
                environments::listing(&self.pool, ops, target)
                    .await?
                    .map_err(|_| Error::Unknown)?,
            ),
            _ => None,
        };
        let (mut tx, stage, identity) = self.begin(Some(owner), id, revision, intent).await?;
        match stage.ok_or(Error::Unknown)? {
            Stage::Retained(result) => {
                tx.commit().await?;
                return Ok(result);
            }
            Stage::Pending(_) => return Err(Error::Unknown),
            Stage::Fresh => {}
        }
        let (request, task) = match (found, build) {
            (Err(what), false) => {
                let refused = host_result(
                    "refused",
                    json!({"reason": format!("unknown_{what}"), "step": "subject"}),
                );
                return self
                    .settle_fresh(tx, id, revision, intent, &identity, refused)
                    .await;
            }
            // A build answers by its completion event only.
            (Err(what), true) => {
                let log = format!("unknown {what}");
                (
                    json!({"answer": {"outcome": "failed", "output": {"recipe": "", "step": "subject", "log": log}}}),
                    None,
                )
            }
            (Ok((target, task)), true) => {
                let listing = listing.ok_or(Error::Unknown)?;
                (environments::build(&mut tx, &target, listing).await?, task)
            }
            (Ok((Target::Task { project, .. } | Target::Project(project), task)), false) => (
                json!({"project": project, "task": task, "builds": input["builds"]}),
                task,
            ),
        };
        let operation = self.operation(intent)?;
        let accepted = host_result("accepted", json!({"handle": operation.to_string()}));
        let (outcome, output) = (&accepted.outcome, &accepted.output);
        self.record(&mut tx, id, revision, intent, &identity, outcome, output)
            .await?;
        let project = request["project"].as_str().unwrap_or("unknown");
        let subject = task.map_or_else(|| project.to_owned(), |t| t.to_string());
        let prepared = Prepared::new(subject, request);
        (self
            .delivery_runs()?
            .enqueue(&mut tx, operation, intent, prepared, self.now()?))
        .await?;
        self.commit_fresh(tx, id, intent).await?;
        Ok(accepted)
    }

    /// One step of a running environment operation: its completion, if any.
    pub(super) async fn environment_step(
        &self,
        operation: Uuid,
        command: &str,
        request: &Value,
    ) -> Result<Option<HostResult>> {
        let answer = match command {
            "environment.build" => environments::build_step(&self.pool, request).await?,
            _ => {
                let ops = self.ports()?.environment_ops.as_ref();
                let actor = self.namespace.owner_user_id;
                environments::admit(&self.pool, ops, actor, operation, request).await?
            }
        };
        Ok(answer.map(|(outcome, output)| host_result(outcome, output)))
    }
}
