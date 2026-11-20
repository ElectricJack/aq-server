//! Trusted result-ready provenance and the preparation and dispatch of durable delivery operations.
use crate::{
    Error, Namespace, Result, delivery_decisions,
    delivery_store::DeliveryPort,
    digest,
    host_values::{HostResult, text},
    host10::ResultReady,
};
use aq_delivery::commands::{self as delivery, Accepting, Completion, Delivery, DeliveryTarget};
use aq_playbook_host::{model::EventEnvelope, runner::Intent};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use uuid::Uuid;

const RESULT_READY: &str = "aq.work.result_ready";
const BRIDGE: &str = "aq.host.bridge@1";

/// Bind the engine's actual fan-out run IDs to the original sealed result event.
/// Call only under the namespace's runtime lock.
pub async fn record_origins(
    db: &mut PgConnection,
    namespace: &Namespace,
    id: Uuid,
    event: &EventEnvelope,
    runs: &Value,
) -> Result<()> {
    if event.producer != BRIDGE || event.contract.name != RESULT_READY {
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
    let proof = json!({"result_ready": ready, "runs": runs});
    let changed = sqlx::query(
        "UPDATE playbook_outbox SET engine_receipt=engine_receipt || $5::jsonb
         WHERE namespace_id=$1 AND generation=$2 AND producer='aq.host.bridge@1'
         AND event_id=$3 AND event_digest=$4 AND status='engine_accepted'
         AND (NOT engine_receipt ? 'result_ready' OR engine_receipt->'result_ready'=$5::jsonb->'result_ready')",
    )
    .bind(namespace.namespace_id)
    .bind(namespace.generation)
    .bind(id)
    .bind(digest(event)?)
    .bind(proof)
    .execute(db)
    .await?;
    if changed.rows_affected() != 1 {
        return Err(Error::Conflict);
    }
    Ok(())
}

/// Manual runs have no event. An event run without a trusted delivery origin is refused.
pub async fn origin(
    db: &mut PgConnection,
    namespace: &Namespace,
    run: &str,
) -> Result<Option<ResultReady>> {
    if !run.starts_with("event-") {
        return Ok(None);
    }
    let ready: Option<Value> = sqlx::query_scalar(
        "SELECT engine_receipt->'result_ready' FROM playbook_outbox
         WHERE namespace_id=$1 AND generation=$2 AND producer='aq.host.bridge@1'
         AND (engine_receipt->'runs') ? $3",
    )
    .bind(namespace.namespace_id)
    .bind(namespace.generation)
    .bind(run)
    .fetch_optional(db)
    .await?;
    Ok(Some(serde_json::from_value(
        ready.ok_or(Error::Forbidden)?,
    )?))
}

/// What the delivery commands need of the server besides the recorded results.
#[async_trait::async_trait]
pub trait DeliveryOps: Send + Sync {
    /// Where a project delivers, if it is bound to a repository that AQ delivers to.
    async fn target(&self, project: Uuid) -> Result<Option<DeliveryTarget>>;
    /// Operations over the forge and central aq-git, if configured.
    async fn commands(&self) -> Option<Delivery>;
    /// The title and body of the task's pull request.
    async fn pull_request_text(&self, workspace: Uuid, task: Uuid) -> Result<(String, String)>;
}

pub struct DeliveryRuns<'a> {
    pub pool: &'a PgPool,
    pub namespace: &'a Namespace,
    pub installation_routing: bool,
    pub results: &'a dyn DeliveryPort,
    pub operations: &'a dyn DeliveryOps,
}

struct Subject {
    id: String,
    project: Uuid,
    workspace: Uuid,
    result: Option<(Uuid, crate::delivery_store::Latest)>,
}

pub struct Prepared {
    subject_id: String,
    request: Value,
}

impl DeliveryRuns<'_> {
    /// Resolve the subject, fence it to the initiating event and freeze the operation request.
    /// A catalog refusal needs no operation row.
    pub async fn prepare(
        &self,
        db: &mut PgConnection,
        intent: &Intent,
        now: impl FnOnce() -> Result<i64>,
    ) -> Result<std::result::Result<Prepared, HostResult>> {
        let (name, input) = (intent.command.name.as_str(), &intent.input);
        let subject = match self.resolve(name, input).await? {
            Ok(subject) => subject,
            Err(reason) => return Ok(Err(delivery_decisions::refused(reason))),
        };
        let (task, latest) = subject
            .result
            .as_ref()
            .map(|(task, facts)| (*task, facts))
            .unzip();
        let origin = match task {
            Some(task) => {
                let event = origin(db, self.namespace, &intent.run)
                    .await?
                    .ok_or(Error::Forbidden)?;
                if task != event.task_id
                    || subject.project != event.project_id
                    || !self.results.lock_current(db, &event).await?
                {
                    return Err(Error::Conflict);
                }
                Some(event)
            }
            None => None,
        };
        let text = match (name, task) {
            ("pull_request.open", Some(task)) => Some(
                self.operations
                    .pull_request_text(subject.workspace, task)
                    .await?,
            ),
            _ => None,
        };
        let longest_check_wait_ms = if name == "pull_request.await_checks" {
            let (limits, _) = aq_role_settings::delivery_limits(self.pool)
                .await
                .map_err(|_| Error::Unknown)?;
            Some(limits.longest_check_wait_ms)
        } else {
            None
        };
        let accepting = Accepting {
            command: name,
            project: subject.project,
            task,
            head: latest.map_or("", |record| record.head.as_str()),
            subject: crate::host_values::subject(&subject.id, 0),
            text,
            longest_check_wait_ms,
            merge_target: if name == "pull_request.merge" {
                self.operations.target(subject.project).await?
            } else {
                None
            },
            result_ids: origin.map(|event| (event.attempt_id, event.result_id)),
        };
        let mut request = match delivery::frozen_request(self.pool, accepting, input, now()?)
            .await
            .map_err(delivery_error)?
        {
            Ok(request) => request,
            Err(reason) => return Ok(Err(delivery_decisions::refused(reason))),
        };
        if let Some(origin) = origin {
            request["result_ready"] = json!(origin);
        }
        Ok(Ok(Prepared {
            subject_id: subject.id,
            request,
        }))
    }

    async fn resolve(
        &self,
        name: &str,
        input: &Value,
    ) -> Result<std::result::Result<Subject, &'static str>> {
        if name == "main.sync" {
            let Ok(project) = Uuid::parse_str(text(input, "project")?) else {
                return Ok(Err("unknown_project"));
            };
            if (project != self.namespace.project_id && !self.installation_routing)
                || self.operations.target(project).await?.is_none()
            {
                return Ok(Err("unknown_project"));
            }
            return Ok(Ok(Subject {
                id: project.to_string(),
                project,
                workspace: self.namespace.workspace_id,
                result: None,
            }));
        }
        let id = text(input, "task")?;
        if let Ok(task) = Uuid::parse_str(id)
            && let Some(result) = self.results.latest(task).await?
            && (result.project_id == self.namespace.project_id || self.installation_routing)
        {
            return Ok(Ok(Subject {
                id: task.to_string(),
                project: result.project_id,
                workspace: result.workspace_id,
                result: Some((task, result)),
            }));
        }
        // These commands lack a refusal outcome. With no known result they do
        // no forge work and complete as closed.
        if matches!(name, "pull_request.await_checks" | "pull_request.close") {
            return Ok(Ok(Subject {
                id: id.to_owned(),
                project: self.namespace.project_id,
                workspace: self.namespace.workspace_id,
                result: None,
            }));
        }
        Ok(Err("unknown_task"))
    }

    /// Queue a prepared operation in the same transaction as its accepted intent.
    pub async fn enqueue(
        &self,
        db: &mut PgConnection,
        operation: Uuid,
        intent: &Intent,
        prepared: Prepared,
        now: i64,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO playbook_host_operations
             (operation_id,namespace_id,generation,intent_id,command_name,subject_id,subject_revision,check_id,request,status,created_at_ms)
             VALUES($1,$2,$3,$4,$5,$6,0,NULL,$7,'running',$8)",
        )
        .bind(operation)
        .bind(self.namespace.namespace_id)
        .bind(self.namespace.generation)
        .bind(&intent.id)
        .bind(&intent.command.name)
        .bind(prepared.subject_id)
        .bind(prepared.request)
        .bind(now)
        .execute(db)
        .await?;
        Ok(())
    }

    /// Check the frozen provenance under the attempt lock the result writer uses.
    /// The caller holds the transaction open until this quantum's operation returns.
    pub async fn step(
        &self,
        db: &mut PgConnection,
        operation: Uuid,
        command: &str,
        request: &Value,
        now: impl FnOnce() -> Result<i64>,
    ) -> Result<Option<Completion>> {
        let project = request["project"]
            .as_str()
            .and_then(|p| Uuid::parse_str(p).ok())
            .ok_or(Error::Unknown)?;
        let target = self.operations.target(project).await?;
        let commands = self.operations.commands().await;
        let now = now()?;

        let result_required = command != "main.sync"
            && !(request["task"].is_null()
                && matches!(command, "pull_request.await_checks" | "pull_request.close"));
        if result_required {
            let current =
                match serde_json::from_value::<ResultReady>(request["result_ready"].clone()) {
                    Ok(origin) => self.results.lock_current(db, &origin).await?,
                    Err(_) => false,
                };
            if !current {
                return Ok(Some(stale_completion(command)));
            }
        }

        delivery::run_step(
            self.pool,
            commands.as_ref(),
            target.as_ref(),
            operation,
            command,
            request,
            now,
        )
        .await
        .map_err(delivery_error)
    }
}

fn stale_completion(command: &str) -> Completion {
    match command {
        "pull_request.await_checks" => Completion::new("timeout", json!({})),
        "pull_request.close" => Completion::closed(),
        "branch.publish" => Completion::refused("no_branch"),
        "pull_request.open" => Completion::refused("unknown_task"),
        _ => Completion::refused("head_changed"),
    }
}

fn delivery_error(error: aq_delivery::DeliveryError) -> Error {
    match error {
        aq_delivery::DeliveryError::InvalidInput(_) => Error::Invalid,
        _ => Error::Unknown,
    }
}
