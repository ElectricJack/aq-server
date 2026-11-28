//! Host catalog 2.3.0 (Epic 0, spec 0016 section 6.2): `work.complete`,
//! `work.return`, `work.hold` and `supervisor.notify`, with the pure parts
//! and the rows in `aq_playbook_domain::delivery_decisions`.
//!
//! `work.complete` and `work.return` are two-phase: the `pending` intent row
//! commits, then the server's settlement runs (`complete_by_playbook`,
//! `return_by_playbook`: their fences, and claim release only on a validated
//! Graph receipt), then the decision and the final intent row. A lost answer
//! leaves the intent pending; a resumed dispatch replays the settlement,
//! which is idempotent per result. `work.hold` and `supervisor.notify` have
//! no external effect: their rows commit with the final intent row.
use super::*;
use aq_playbook_domain::{
    delivery_decisions::{self as decisions, Named, Settled},
    delivery_runs,
};
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
    pub(in crate::playbooks) async fn work_decide(
        &self,
        owner: &RuntimeOwner,
        command: HostIntent<'_>,
        retained_only: bool,
    ) -> Result<HostResult> {
        let id = command.grant;
        let intent = command.intent;
        let input = &intent.input;
        let (mut tx, stage, identity) = self.begin(Some(owner), command).await?;
        let subject = match stage.ok_or(Error::Unknown)? {
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
            Stage::Fresh if retained_only => return Err(Error::Unknown),
            Stage::Fresh => {
                let early = self.pending_decision(&mut tx, intent).await?;
                let named = match early {
                    Ok(named) => named,
                    Err(answer) => {
                        let settled = self.settle_fresh(tx, command.retained(&identity), answer);
                        return settled.await;
                    }
                };
                self.record(
                    &mut tx,
                    command.retained(&identity),
                    EffectReceipt::new(PENDING, input),
                )
                .await?;
                self.commit_fresh(tx, id, intent).await?;
                named
            }
        };
        let answer = decisions::settle(
            &*self.settlement,
            &self.pool,
            || self.now(),
            &intent.command.name,
            input,
            subject,
        )
        .await?;
        self.finish(intent, &identity, answer).await
    }

    async fn pending_decision(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        intent: &Intent,
    ) -> Result<std::result::Result<(Uuid, Uuid), HostResult>> {
        let input = &intent.input;
        let origin = delivery_runs::origin(tx, &self.namespace, &intent.run).await?;
        if origin.is_some_and(|o| !o.names_result(input)) {
            return Ok(Err(decisions::refused("stale_result")));
        }
        let named = match self.named(input).await? {
            Ok(named) => named,
            Err(reason) => return Ok(Err(decisions::refused(reason))),
        };
        let decided = decisions::decision(tx, named.1).await?;
        Ok(decisions::already(&intent.command.name, decided.as_deref()).map_or(Ok(named), Err))
    }

    /// `work.hold`: the hold on the task's latest result, unless a decision
    /// is on it already. A stale hold fails through the host error path:
    /// reporting `held` would allow its following notification to run.
    pub(super) async fn work_hold(
        &self,
        owner: &RuntimeOwner,
        command: HostIntent<'_>,
    ) -> Result<HostResult> {
        let intent = command.intent;
        let (mut tx, stage, identity) = self.begin(Some(owner), command).await?;
        if let Some(retained) = Self::single_phase(stage)? {
            tx.commit().await?;
            return Ok(retained);
        }
        let origin = delivery_runs::origin(&mut tx, &self.namespace, &intent.run).await?;
        decisions::fence_hold(&mut tx, self.delivery.as_ref(), origin, &intent.input).await?;
        let named = self.named(&intent.input).await?;
        decisions::hold(&mut tx, named, &intent.input, self.now()?).await?;
        let held = decisions::held();
        (self.settle_fresh(tx, command.retained(&identity), held)).await
    }

    /// `supervisor.notify`: `text` to the project's supervisor conversation
    /// (the global one when the project has none), as the fixed system
    /// sender, once per (project, identity). The once-only row and the
    /// message commit with the final intent row. The project is the
    /// namespace's, or any bound project with installation routing.
    pub(super) async fn supervisor_notify(
        &self,
        owner: &RuntimeOwner,
        command: HostIntent<'_>,
    ) -> Result<HostResult> {
        let intent = command.intent;
        let (mut tx, stage, identity) = self.begin(Some(owner), command).await?;
        if let Some(retained) = Self::single_phase(stage)? {
            tx.commit().await?;
            return Ok(retained);
        }
        let input = &intent.input;
        let project = Uuid::parse_str(text(input, "project")?).map_err(|_| Error::Invalid)?;
        self.authorize_notification_project(project).await?;
        let origin = delivery_runs::origin(&mut tx, &self.namespace, &intent.run).await?;
        let (key, body) =
            decisions::notification(&mut tx, self.delivery.as_ref(), origin, project, input)
                .await?;
        let told = decisions::told(&mut tx, project, &key).await?;
        if !told {
            let now = self.now()?;
            let at = u64::try_from(now).map_err(|_| Error::Invalid)?;
            use crate::agent_access::supervisor_conversation::system_message_in;
            let message = system_message_in(&mut tx, Some(project), &body, at);
            let message = message.await.map_err(|error| match error {
                crate::error::AppError::BadRequest(_) => Error::Invalid,
                crate::error::AppError::Maintenance(_) => Error::Overloaded,
                _ => Error::Unknown,
            })?;
            decisions::record_told(&mut tx, project, &key, message, now).await?;
        }
        let answer = decisions::notified(told);
        (self.settle_fresh(tx, command.retained(&identity), answer)).await
    }

    async fn authorize_notification_project(&self, project: Uuid) -> Result<()> {
        if project == self.namespace.project_id {
            return Ok(());
        }
        if self.installation_routing && self.delivery.bound(project).await? {
            return Ok(());
        }
        Err(Error::Forbidden)
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
