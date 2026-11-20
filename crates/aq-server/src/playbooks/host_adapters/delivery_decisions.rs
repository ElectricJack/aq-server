//! Host catalog 2.3.0 (Epic 0, spec 0016 section 6.2): `work.complete`,
//! `work.return`, `work.hold` and `supervisor.notify`, with the pure parts
//! and the rows in `aq_playbook_domain::delivery_decisions`. Compiled in only
//! with the cargo feature `delivery_playbook`.
//!
//! `work.complete` and `work.return` are two-phase: the `pending` intent row
//! commits, then the server's settlement runs (`complete_by_playbook`,
//! `return_by_playbook`: their fences, and claim release only on a validated
//! Graph receipt), then the decision and the final intent row. A lost answer
//! leaves the intent pending; a resumed dispatch replays the settlement,
//! which is idempotent per result. `work.hold` and `supervisor.notify` have
//! no external effect: their rows commit with the final intent row.
use super::*;
use aq_playbook_domain::delivery_decisions::{self as decisions, Decision, Named, Settled};
pub use decisions::DeliverySettlement;

/// Production port: the server's automatic settlement.
pub struct ServerSettlement(pub crate::AppState);
#[async_trait::async_trait]
impl DeliverySettlement for ServerSettlement {
    async fn complete(&self, result: Uuid) -> Result<Settled> {
        use crate::profiled_execution::settlement::complete_by_playbook as complete;
        let settled = complete(&self.0, result).await;
        settled.map_err(|e| pending("work.complete", result, e))
    }
    async fn send_back(&self, result: Uuid, reason: &str) -> Result<Settled> {
        use crate::profiled_execution::settlement::return_by_playbook as send_back;
        let settled = send_back(&self.0, result, reason).await;
        settled.map_err(|e| pending("work.return", result, e))
    }
}
fn pending(command: &str, result: Uuid, error: crate::error::AppError) -> Error {
    tracing::warn!(command, %result, ?error, "playbook decision remains pending");
    Error::Unknown
}

impl Service {
    /// What `input` names, with a resultless task told apart by the Graph.
    async fn named(&self, input: &Value) -> Result<std::result::Result<(Uuid, Uuid), &str>> {
        let own = (self.namespace.workspace_id, self.namespace.project_id);
        let routing = self.installation_routing;
        Ok(
            match decisions::named(&*self.delivery, input, own, routing).await? {
                Named::Latest { task, result } => Ok((task, result)),
                Named::Refused(reason) => Err(reason),
                Named::Resultless(task) => match self.ports()?.graph.node(own.0, task).await? {
                    Some(_) => Err("unknown_result"),
                    None => Err("unknown_task"),
                },
            },
        )
    }

    /// `work.complete` and `work.return`.
    pub(super) async fn work_decide(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<HostResult> {
        let (command, input) = (intent.command.name.as_str(), &intent.input);
        let (mut tx, stage, identity) = self.begin(Some(owner), id, revision, intent).await?;
        let (task, result) = match stage.ok_or(Error::Unknown)? {
            Stage::Retained(result) => {
                tx.commit().await?;
                return Ok(result);
            }
            // The same input as the pending row's (the identity binds it).
            Stage::Pending(_) => {
                tx.commit().await?;
                let id = |field| Uuid::parse_str(text(input, field)?).map_err(|_| Error::Invalid);
                (id("task")?, id("result_id")?)
            }
            Stage::Fresh => {
                if intent.run.starts_with("event-") {
                    let origin = self
                        .delivery_origin(&mut tx, intent)
                        .await?
                        .ok_or(Error::Forbidden)?;
                    if input["task"] != origin.task_id.to_string()
                        || input["result_id"] != origin.result_id.to_string()
                    {
                        let refused = decisions::refused("stale_result");
                        return self
                            .settle_fresh(tx, id, revision, intent, &identity, refused)
                            .await;
                    }
                }
                let early = match self.named(input).await? {
                    Err(reason) => Err(decisions::refused(reason)),
                    Ok(named) => {
                        let decided = decisions::decision(&mut tx, named.1).await?;
                        decisions::already(command, decided.as_deref()).map_or(Ok(named), Err)
                    }
                };
                let named = match early {
                    Ok(named) => named,
                    Err(answer) => {
                        let settled =
                            self.settle_fresh(tx, id, revision, intent, &identity, answer);
                        return settled.await;
                    }
                };
                self.record(&mut tx, id, revision, intent, &identity, PENDING, input)
                    .await?;
                self.commit_fresh(tx, id, intent).await?;
                named
            }
        };
        let reason = input["reason"].as_str();
        let settled = match command {
            "work.complete" => self.settlement.complete(result).await?,
            _ => {
                (self.settlement)
                    .send_back(result, reason.ok_or(Error::Invalid)?)
                    .await?
            }
        };
        // No Graph answer yet: the intent stays pending for a replay.
        let (answer, decision) = decisions::settled(command, settled).ok_or(Error::Unknown)?;
        if let Some(decision) = decision {
            let d = Decision {
                task,
                result,
                decision,
                reason,
                detail: input["detail"].as_str(),
                update_from_main: input["update_from_main"].as_bool(),
            };
            let mut tx = self.tx().await?;
            decisions::record(&mut tx, &d, self.now()?).await?;
            tx.commit().await?;
        }
        self.finish(intent, &identity, answer).await
    }

    /// `work.hold`: the hold on the task's latest result, unless a decision
    /// is on it already. A stale hold fails through the host error path:
    /// reporting `held` would allow its following notification to run.
    pub(super) async fn work_hold(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<HostResult> {
        let (mut tx, stage, identity) = self.begin(Some(owner), id, revision, intent).await?;
        if let Some(retained) = Self::single_phase(stage)? {
            tx.commit().await?;
            return Ok(retained);
        }
        if intent.run.starts_with("event-") {
            let origin = self
                .delivery_origin(&mut tx, intent)
                .await?
                .ok_or(Error::Forbidden)?;
            if intent.input["task"] != origin.task_id.to_string()
                || intent.input["result_id"] != origin.result_id.to_string()
                || !self.delivery.lock_current(&mut tx, &origin).await?
            {
                return Err(Error::Conflict);
            }
        }
        match self.named(&intent.input).await? {
            Ok((task, result)) => {
                if matches!(
                    decisions::decision(&mut tx, result).await?.as_deref(),
                    Some("completed" | "returned")
                ) {
                    return Err(Error::Conflict);
                }
                let reason = Some(text(&intent.input, "reason")?);
                let (detail, update_from_main, decision) = (None, None, "held");
                let d = Decision {
                    task,
                    result,
                    decision,
                    reason,
                    detail,
                    update_from_main,
                };
                decisions::record(&mut tx, &d, self.now()?).await?;
            }
            Err("stale_result") => return Err(Error::Conflict),
            Err(_) => return Err(Error::Invalid),
        }
        let held = decisions::held();
        (self.settle_fresh(tx, id, revision, intent, &identity, held)).await
    }

    /// `supervisor.notify`: `text` to the project's supervisor conversation
    /// (the global one when the project has none), as the fixed system
    /// sender, once per (project, identity). The once-only row and the
    /// message commit with the final intent row. The project is the
    /// namespace's, or any bound project with installation routing.
    pub(super) async fn supervisor_notify(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<HostResult> {
        let (mut tx, stage, identity) = self.begin(Some(owner), id, revision, intent).await?;
        if let Some(retained) = Self::single_phase(stage)? {
            tx.commit().await?;
            return Ok(retained);
        }
        let input = &intent.input;
        let project = Uuid::parse_str(text(input, "project")?).map_err(|_| Error::Invalid)?;
        if project != self.namespace.project_id
            && !(self.installation_routing && self.delivery.bound(project).await?)
        {
            return Err(Error::Forbidden);
        }
        let key = if intent.run.starts_with("event-") {
            let origin = self
                .delivery_origin(&mut tx, intent)
                .await?
                .ok_or(Error::Forbidden)?;
            if origin.project_id != project || !self.delivery.lock_current(&mut tx, &origin).await?
            {
                return Err(Error::Conflict);
            }
            let reason: Option<String> = sqlx::query_scalar(
                "SELECT reason FROM delivery_decisions
                 WHERE result_id=$1 AND task_id=$2 AND decision='held'",
            )
            .bind(origin.result_id)
            .bind(origin.task_id)
            .fetch_optional(&mut *tx)
            .await?
            .flatten();
            let reason = reason.ok_or(Error::Conflict)?;
            decisions::notify_key(&format!("{}:{reason}", origin.result_id))?
        } else {
            decisions::notify_key(text(input, "identity")?)?
        };
        let told = decisions::told(&mut tx, project, &key).await?;
        if !told {
            let now = self.now()?;
            let at = u64::try_from(now).map_err(|_| Error::Invalid)?;
            use crate::agent_access::supervisor_conversation::system_message_in;
            let message = system_message_in(&mut tx, Some(project), text(input, "text")?, at);
            let message = message.await.map_err(|error| match error {
                crate::error::AppError::BadRequest(_) => Error::Invalid,
                crate::error::AppError::Maintenance(_) => Error::Overloaded,
                _ => Error::Unknown,
            })?;
            decisions::record_told(&mut tx, project, &key, message, now).await?;
        }
        let answer = decisions::notified(told);
        (self.settle_fresh(tx, id, revision, intent, &identity, answer)).await
    }

    /// A single-phase command's retained answer, or `None` for a fresh one.
    /// It never leaves a pending row.
    fn single_phase(stage: Option<Stage>) -> Result<Option<HostResult>> {
        match stage.ok_or(Error::Unknown)? {
            Stage::Retained(result) => Ok(Some(result)),
            Stage::Pending(_) => Err(Error::Unknown),
            Stage::Fresh => Ok(None),
        }
    }
}
