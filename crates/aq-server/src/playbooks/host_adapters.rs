//! D5 host adapters for catalog profile 5 (vendored D4 host catalog 1.0.0):
//! context.read, work.ensure, work.change and operation.cancel.
//!
//! Every adapter persists the AQ intent (namespace, Intent.id, contract
//! fingerprint, payload digest, grant and revision; see `Service::identity`)
//! before any external effect. The same identity with different bytes is a
//! conflict. External effects (graph mutation, B3 cancel) are two-phase: a
//! `pending` row commits first, then the effect runs with a stable idempotency
//! key, then the final row and its outbox event commit together. Reconcile
//! may read a retained answer or close an expired admission without performing the effect.
use super::{domain::*, service::*};
use aq_playbook_domain::work_mutations::{work_change_body, work_ensure_body};
pub use aq_playbook_domain::{
    host_values::*,
    review_domain::{HOST5_COMMANDS, HOST7_COMMANDS},
};
use aq_playbook_host::{
    model::{ContractRef, EventEnvelope, canonical_bytes},
    runner::Intent,
};
use serde_json::{Value, json};
use sqlx::{PgConnection, PgExecutor, Postgres, Row, Transaction};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use uuid::Uuid;

fn host_result(outcome: &str, output: Value) -> HostResult {
    HostResult {
        outcome: outcome.into(),
        output,
    }
}

mod activate;
mod assess;
mod cancellation;
mod context;
mod delivery_commands;
mod delivery_decisions;
mod delivery_port;
mod effects;
mod events;
use events::HostEvent;
pub(super) use events::OutboxEntry;
#[cfg(feature = "project_environments")]
mod environment_commands;
mod integration;
mod messages;
mod requests;
mod routing;
mod work;
pub use delivery_commands::{DeliveryOps, ServerDelivery};
pub use delivery_decisions::{DeliverySettlement, ServerSettlement};
pub use delivery_port::*;
#[cfg(feature = "project_environments")]
pub use environment_commands::ServerEnvironments;
pub use integration::*;
pub use requests::*;
pub use routing::{RoutingPort, ServerRouting};

/// External ports. Authority never comes from here: it comes from the grant.
pub struct HostPorts {
    pub state: crate::AppState,
    pub graph: Arc<dyn WorkGraph>,
    /// D6 sandbox check executor; `None` refuses check.request `unavailable`.
    pub checks: Option<Arc<dyn CheckExecutor>>,
    /// D6 judgment provider; `None` refuses `no_permitted_profile`.
    pub judgments: Option<Arc<dyn JudgmentProvider>>,
    /// D7 integration port (D1 delivery); `None` refuses `unavailable`.
    pub integration: Option<Arc<dyn Integration>>,
    /// Host9 routing commands: routes, bindings and attempt history.
    pub routing: Arc<dyn RoutingPort>,
    /// Catalog 2.3.0 delivery commands (Epic 0): where a project's work is
    /// delivered, and the commands over the forge and central aq-git.
    pub delivery_ops: Arc<dyn DeliveryOps>,
    /// Catalog 2.5.0: the project's recipes on aq-git and its binding.
    #[cfg(feature = "project_environments")]
    pub environment_ops: Arc<dyn aq_environments::commands::EnvironmentOps>,
}

/// Typed graph gateway port. Only node reads by UUID and validated
/// `MutationRequest` bodies cross it; no caller-chosen route or SQL.
#[async_trait::async_trait]
pub trait WorkGraph: Send + Sync {
    async fn node(&self, workspace: Uuid, id: Uuid) -> Result<Option<Value>>;
    /// A versioned read of the task's structural descendants.
    async fn activation_tree(&self, _workspace: Uuid, _id: Uuid) -> Result<Value> {
        Err(Error::Unknown)
    }
    /// Returns the HTTP status and the decoded body.
    async fn mutate(&self, workspace: Uuid, body: Value) -> Result<(u16, Value)>;
}

/// Production port: the agent-access graph gateway.
pub struct GatewayGraph(pub Arc<crate::agent_access::Runtime>);
const GRAPH_CALL: Duration = Duration::from_secs(3);
#[async_trait::async_trait]
impl WorkGraph for GatewayGraph {
    async fn node(&self, workspace: Uuid, id: Uuid) -> Result<Option<Value>> {
        tokio::time::timeout(
            GRAPH_CALL,
            crate::agent_access::settlement_node(&self.0, workspace, id),
        )
        .await
        .map_err(|_| Error::Unknown)?
        .map_err(|_| Error::Unknown)
    }
    async fn activation_tree(&self, workspace: Uuid, task: Uuid) -> Result<Value> {
        tokio::time::timeout(
            GRAPH_CALL,
            crate::agent_access::activation_tree(&self.0, workspace, task),
        )
        .await
        .map_err(|_| Error::Unknown)?
        .map_err(|_| Error::Unknown)
    }
    async fn mutate(&self, workspace: Uuid, body: Value) -> Result<(u16, Value)> {
        if !crate::agent_access::settlement_request_valid(&body) {
            return Err(Error::Invalid);
        }
        let bytes = serde_json::to_vec(&body)?;
        let (status, raw) = tokio::time::timeout(
            GRAPH_CALL,
            crate::agent_access::settlement_post(&self.0, workspace, bytes),
        )
        .await
        .map_err(|_| Error::Unknown)?
        .map_err(|_| Error::Unknown)?;
        if status.is_success() && !crate::agent_access::settlement_response_valid(&raw) {
            return Err(Error::Unknown);
        }
        Ok((status.as_u16(), raw))
    }
}

#[path = "findings.rs"]
mod findings;
mod plugins;

use aq_playbook_domain::effect_store::{
    EffectIdentity, EffectRecord, Effects, Receipt as EffectReceipt, Retained,
};
use aq_playbook_domain::host_operations::{OperationKey, OperationRequest};
use effects::{Effect, PreparedEffect};
const ATTEMPT_REF: &str = "aq.execution.attempt/";
const PENDING: &str = "pending";
const PLUGIN_COMPLETED: &str = "plugin_completed";

#[derive(Clone, Copy)]
pub(super) struct HostIntent<'a> {
    grant: Uuid,
    revision: i64,
    intent: &'a Intent,
}
impl<'a> HostIntent<'a> {
    pub(super) fn new(grant: Uuid, revision: i64, intent: &'a Intent) -> Self {
        Self {
            grant,
            revision,
            intent,
        }
    }
}

impl<'a> HostIntent<'a> {
    fn retained(self, identity: &'a str) -> EffectIdentity<'a> {
        EffectIdentity {
            identity,
            grant: self.grant,
            revision: self.revision,
            intent: self.intent,
        }
    }
}

struct SettledEffect {
    result: HostResult,
    work: Option<Value>,
}

enum Stage {
    Retained(HostResult),
    Pending(Value),
    Fresh,
}

impl Stage {
    fn restore(row: Retained, intent: &Intent, identity: &str) -> Result<Self> {
        if row.request_digest != identity || row.command_name != intent.command.name {
            return Err(Error::Conflict);
        }
        match row.status.as_str() {
            PENDING => Ok(Self::Pending(row.receipt)),
            PLUGIN_COMPLETED => Ok(Self::Retained(serde_json::from_value(row.receipt)?)),
            _ => Ok(Self::Retained(HostResult {
                outcome: row.status,
                output: row.receipt,
            })),
        }
    }
}
impl Service {
    fn ports(&self) -> Result<&HostPorts> {
        if !self.profile.is_host() {
            return Err(Error::Forbidden);
        }
        self.host.as_deref().ok_or(Error::Forbidden)
    }

    /// Dispatch one profile-3 command. `Error::Unknown` means the effect may
    /// be pending; re-dispatch of the same intent resumes it idempotently.
    pub async fn host_dispatch(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<HostResult> {
        let command = HostIntent::new(id, revision, intent);
        match intent.command.name.as_str() {
            name if aq_playbook_host::plugins::command_plugin(name).is_some() => {
                self.plugin_dispatch(owner, command, true).await
            }
            "findings.reconcile" => self.findings_reconcile(owner, command).await,
            "work.read" | "work.route" | "work.unroutable" => {
                self.work_routing(owner, command).await
            }
            "context.read" => self.context_read(owner, command).await,
            "work.ensure" => self.work_ensure(owner, command).await,
            "work.change" => self.work_change(owner, command).await,
            super::review_domain::WORK_ACTIVATE => self.work_activate(owner, command).await,
            "work.complete" | "work.return" => self.work_decide(owner, command, false).await,
            "work.hold" => self.work_hold(owner, command).await,
            "supervisor.notify" => self.supervisor_notify(owner, command).await,
            "operation.cancel" => self.operation_cancel(owner, command).await,
            "judgment.request" => self.judgment_request(owner, command).await,
            "judgment.assess" => self.judgment_assess(owner, command).await,
            "check.request" => self.check_request(owner, command).await,
            "operation.get" => self.host_operation_get(owner, command).await,
            "candidate.prepare" => self.candidate_prepare(owner, command).await,
            "candidate.publish" => self.candidate_publish(owner, command).await,
            "message.send" => self.host_message_send(owner, command).await,
            "decision.request" => self.host_decision_request(owner, command).await,
            #[cfg(feature = "project_environments")]
            name if self.profile.has_environments()
                && aq_playbook_domain::review_domain::ENVIRONMENT_COMMANDS.contains(&name) =>
            {
                self.environment_command(owner, id, revision, intent).await
            }
            name if self.profile.is_delivery()
                && aq_playbook_domain::review_domain::DELIVERY_COMMANDS.contains(&name) =>
            {
                self.delivery_command(owner, command).await
            }
            _ => Err(Error::Invalid),
        }
    }

    /// Read a retained reply, or close an expired delivery intent whose effect never began.
    pub async fn host_retained(
        &self,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<Option<HostResult>> {
        let command = HostIntent::new(id, revision, intent);
        let (mut tx, stage, identity) = self.begin(None, command).await?;
        let reply = match stage {
            Some(Stage::Retained(result)) => Some(result),
            None if self.profile.is_delivery() => {
                aq_playbook_domain::host_operations::close_unadmitted_delivery(
                    &mut tx,
                    &self.namespace,
                    (id, revision),
                    intent,
                    &identity,
                    self.operation(intent)?,
                    self.now()?,
                )
                .await?
            }
            _ => None,
        };
        tx.commit().await?;
        Ok(reply)
    }

    /// Validates the intent, locks the namespace, and resolves the retained
    /// row. A fresh intent requires a live grant and an open deadline. With no
    /// owner (reconcile) no fresh stage is returned.
    async fn begin(
        &self,
        owner: Option<&RuntimeOwner>,
        command: HostIntent<'_>,
    ) -> Result<(Transaction<'_, Postgres>, Option<Stage>, String)> {
        let intent = command.intent;
        self.validate_host_input(intent)?;
        let identity = self.identity(intent)?;
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        if let Some(owner) = owner {
            self.runtime(&ns, owner)?;
        }
        let retained = Effects(&self.namespace).lock(&mut tx, intent).await?;
        let live = owner.is_some() && retained.as_ref().is_none_or(|r| r.status == PENDING);
        let resumed = match &retained {
            Some(row)
                if live
                    && matches!(
                        intent.command.name.as_str(),
                        "work.complete" | "work.return"
                    ) =>
            {
                aq_playbook_domain::grants::delivery_resume_current(
                    &mut tx,
                    row.operation_id,
                    self.now()?,
                )
                .await?
            }
            _ => false,
        };
        let live = live && !resumed;
        let scope = self
            .grant_locked(
                &mut tx,
                GrantRevision::new(command.grant, command.revision),
                live,
            )
            .await?;
        self.intent_authority(
            intent,
            GrantRevision::new(command.grant, command.revision),
            &scope,
        )?;
        let stage = match retained {
            Some(row) => Some(Stage::restore(row, intent, &identity)?),
            None if owner.is_none() => None,
            None if intent.deadline_at_ms <= self.now()? => return Err(Error::DeadlineExpired),
            None => Some(Stage::Fresh),
        };
        Ok((tx, stage, identity))
    }

    fn validate_host_input(&self, intent: &Intent) -> Result<()> {
        if aq_playbook_host::plugins::command_plugin(&intent.command.name).is_none() {
            self.ports()?;
        }
        if !self.known_contract(&intent.command) {
            return Err(Error::Invalid);
        }
        intent
            .command
            .input
            .validate(&intent.input)
            .map_err(|_| Error::Invalid)?;
        if canonical_bytes(&intent.input)
            .map_err(|_| Error::Invalid)?
            .len()
            > intent.command.max_payload_bytes
            || digest(&intent.input)? != intent.input_digest
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }

    fn validated(&self, intent: &Intent, outcome: &str, output: &Value) -> Result<()> {
        intent
            .command
            .outcomes
            .get(outcome)
            .ok_or(Error::Unknown)?
            .validate(output)
            .map_err(|_| Error::Unknown)
    }

    fn operation(&self, intent: &Intent) -> Result<Uuid> {
        Effects(&self.namespace).operation(intent)
    }

    /// Inserts the intent row (pending or final). Commit happens in the caller
    /// after expiry and deadline are rechecked under the namespace lock.
    async fn record(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        effect: EffectIdentity<'_>,
        receipt: EffectReceipt<'_>,
    ) -> Result<()> {
        if receipt.status != PENDING && receipt.status != PLUGIN_COMPLETED {
            self.validated(effect.intent, receipt.status, receipt.value)?;
        }
        Effects(&self.namespace)
            .insert(tx, EffectRecord { effect, receipt }, || self.now())
            .await
    }

    /// Fresh-stage commit: recheck expiry and deadline after lock waits.
    async fn commit_fresh(
        &self,
        mut tx: Transaction<'_, Postgres>,
        id: Uuid,
        intent: &Intent,
    ) -> Result<()> {
        self.check_expiry(&mut tx, id).await?;
        if intent.deadline_at_ms <= self.now()? {
            return Err(Error::Forbidden);
        }
        tx.commit().await?;
        Ok(())
    }

    /// Settles a pending row: the final outcome and its outbox event commit
    /// together. A row already settled by another resume is returned as is.
    async fn finish(
        &self,
        intent: &Intent,
        identity: &str,
        result: HostResult,
    ) -> Result<HostResult> {
        let settled = self.work_accepted(intent, result)?;
        let result = &settled.result;
        self.validated(intent, &result.outcome, &result.output)?;
        let mut tx = self.tx().await?;
        self.namespace_locked(&mut tx).await?;
        let effects = Effects(&self.namespace);
        if let Some(retained) = effects.completed(&mut tx, intent, identity).await? {
            tx.commit().await?;
            return Ok(retained);
        }
        effects.complete(&mut tx, intent, result).await?;
        self.effect_event(&mut tx, intent, &settled).await?;
        tx.commit().await?;
        Ok(settled.result)
    }

    /// Single-phase final row (no external effect remains), committed with
    /// its event.
    async fn settle_fresh(
        &self,
        mut tx: Transaction<'_, Postgres>,
        effect: EffectIdentity<'_>,
        result: HostResult,
    ) -> Result<HostResult> {
        let id = effect.grant;
        let intent = effect.intent;
        let settled = self.work_accepted(intent, result)?;
        self.record(
            &mut tx,
            effect,
            EffectReceipt::new(&settled.result.outcome, &settled.result.output),
        )
        .await?;
        self.effect_event(&mut tx, intent, &settled).await?;
        self.commit_fresh(tx, id, intent).await?;
        Ok(settled.result)
    }

    /// Catalog profile 7 work.ensure is asynchronous: an ensured node is
    /// returned as `accepted` with the durable operation handle, and the
    /// ensured work output is carried separately to be tracked. Every other
    /// profile and outcome is unchanged.
    fn work_accepted(&self, intent: &Intent, result: HostResult) -> Result<SettledEffect> {
        let ensured = intent.command.name == "work.ensure" && result.outcome == "ensured";
        if !(self.profile.is_host2() && ensured) {
            return Ok(SettledEffect { result, work: None });
        }
        let accepted = HostResult {
            outcome: "accepted".into(),
            output: json!({"handle": self.operation(intent)?.to_string()}),
        };
        Ok(SettledEffect {
            result: accepted,
            work: Some(result.output),
        })
    }

    /// The outbox event (and, for accepted Host7 work, the running operation
    /// row) that commits with a final effect row.
    async fn effect_event(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        intent: &Intent,
        settled: &SettledEffect,
    ) -> Result<()> {
        let result = &settled.result;
        if let Some(work) = settled.work.as_ref() {
            self.track_work(tx, intent, work).await?;
            return self
                .work_event(tx, intent, EffectReceipt::new("ensured", work))
                .await;
        }
        if matches!(result.outcome.as_str(), "ensured" | "changed") {
            self.work_event(
                tx,
                intent,
                EffectReceipt::new(&result.outcome, &result.output),
            )
            .await?;
        }
        Ok(())
    }

    async fn namespace_work(&self, tx: &mut Transaction<'_, Postgres>, task: Uuid) -> Result<bool> {
        Ok(sqlx::query_scalar::<_, Uuid>("SELECT task_id FROM playbook_work_keys WHERE namespace_id=$1 AND generation=$2 AND task_id=$3")
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(task)
            .fetch_optional(&mut **tx)
            .await?
            .is_some())
    }
}
