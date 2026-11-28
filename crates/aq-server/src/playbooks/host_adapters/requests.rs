//! D6 asynchronous host adapters for catalog profile 6 (AQ host catalog
//! 2.0.0): judgment.request, check.request, work.ensure completion and their
//! operation.get reconciliation.
//!
//! Both commands record one durable intent (`playbook_effects` row `accepted`
//! plus one `playbook_host_operations` row `running`) before any provider
//! request or sandbox start, return the operation handle, and complete exactly
//! once by a sealed trusted event (aq.judgment.completed / aq.check.receipted)
//! that commits with the retained completion. A re-dispatch or operation.get
//! only observes the original operation; nothing here ever starts a second
//! check writer or a second provider request for one intent.
//!
//! The sandbox executor and the judgment provider are injected ports. The
//! host never fabricates a receipt: every passed/failed disposition comes from
//! an executor-reported exit, and an absent port is a typed refusal.
use super::*;
use aq_playbook_domain::host_operations::{DecisionRequest, DecisionView, Operation, Operations};
use aq_playbook_host::runner::CancellationResult;

pub use aq_playbook_domain::host_requests::*;

struct Completion<'a> {
    key: OperationKey<'a>,
    result: HostResult,
    note: Option<&'a str>,
}

impl Service {
    fn completion_contract(&self, command: &str) -> Result<(Value, &'static str)> {
        Ok(match command {
            "judgment.request" => (json!("judgment"), "aq.judgment.completed"),
            "judgment.assess" => (json!("judgment"), "aq.judgment.assessed"),
            "check.request" => (json!("check"), "aq.check.receipted"),
            "work.ensure" => (json!("work"), "aq.operation.updated"),
            "decision.request" => (json!("decision"), "aq.decision.completed"),
            name if aq_playbook_domain::review_domain::DELIVERY_COMMANDS.contains(&name) => {
                (json!("delivery"), "aq.delivery.completed")
            }
            #[cfg(feature = "project_environments")]
            "environment.build" | "environment.admit" => {
                (json!("environment"), "aq.environment.completed")
            }
            _ => return Err(Error::Invalid),
        })
    }

    /// Records the durable intent (accepted effect row and running operation
    /// row) and commits it before any external effect.
    pub(super) async fn record_operation(
        &self,
        mut tx: Transaction<'_, Postgres>,
        effect: EffectIdentity<'_>,
        admission: OperationRequest<'_>,
    ) -> Result<HostResult> {
        let id = effect.grant;
        let intent = effect.intent;
        let operation = self.operation(intent)?;
        let accepted = HostResult {
            outcome: "accepted".into(),
            output: json!({"handle": operation.to_string()}),
        };
        self.record(
            &mut tx,
            effect,
            EffectReceipt::new(&accepted.outcome, &accepted.output),
        )
        .await?;
        Operations(&self.namespace)
            .record(
                &mut tx,
                operation,
                &intent.id,
                &intent.command.name,
                &intent.input["subject"],
                admission.check,
                admission.body,
                self.now()?,
            )
            .await?;
        self.commit_fresh(tx, id, intent).await?;
        Ok(accepted)
    }

    pub(super) async fn load_operation(
        &self,
        db: impl PgExecutor<'_>,
        id: Uuid,
    ) -> Result<Option<Operation>> {
        Operations(&self.namespace).load(db, id).await
    }

    /// Completes a running operation exactly once: the retained completion and
    /// its sealed completion event commit together. A completion that lost the
    /// race returns the retained one.
    pub(super) async fn complete_operation(
        &self,
        key: OperationKey<'_>,
        result: HostResult,
        note: Option<&str>,
    ) -> Result<HostResult> {
        let mut tx = self.tx().await?;
        self.namespace_locked(&mut tx).await?;
        let result = self
            .retain_completion(&mut tx, Completion { key, result, note })
            .await?;
        tx.commit().await?;
        Ok(result)
    }

    async fn retain_completion(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        completion: Completion<'_>,
    ) -> Result<HostResult> {
        let Completion { key, result, note } = completion;
        let OperationKey {
            operation,
            command,
            intent_id,
        } = key;
        let (_, contract) = self.completion_contract(command)?;
        let payload = json!({"outcome": result.outcome, "output": result.output});
        self.registry()
            .commands
            .get(command)
            .and_then(|c| c.asynchronous.as_ref())
            .ok_or(Error::Invalid)?
            .completion
            .validate(&payload)
            .map_err(|_| Error::Unknown)?;
        if !Operations(&self.namespace)
            .complete(tx, operation, &result, self.now()?, note)
            .await?
        {
            return self
                .load_operation(&mut **tx, operation)
                .await?
                .ok_or(Error::Unknown)?
                .retained();
        }
        let event_id = stable_id(
            &format!("aq.playbook.{command}.completion"),
            self.namespace.namespace_id,
            self.namespace.generation,
            intent_id,
        )?;
        self.seal_event(
            tx,
            HostEvent {
                contract,
                event_id,
                operation,
                causation: intent_id,
                subject: &result.output["subject"],
                payload,
            },
        )
        .await?;
        Ok(result)
    }
}

mod checks;
mod decisions;
mod judgments;
mod observation;
mod work;
