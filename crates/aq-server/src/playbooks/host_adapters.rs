//! D5 host adapters for catalog profile 5 (vendored D4 host catalog 1.0.0):
//! context.read, work.ensure, work.change and operation.cancel.
//!
//! Every adapter persists the AQ intent (namespace, Intent.id, contract
//! fingerprint, payload digest, grant and revision; see `Service::identity`)
//! before any external effect. The same identity with different bytes is a
//! conflict. External effects (graph mutation, B3 cancel) are two-phase: a
//! `pending` row commits first, then the effect runs with a stable idempotency
//! key, then the final row and its outbox event commit together. Reconcile
//! only reads a retained final row and never performs an effect.
use super::{domain::*, review_domain::CatalogProfile, service::*};
use aq_playbook_host::{
    model::{ContractRef, EventEnvelope, canonical_bytes},
    runner::Intent,
};
use serde_json::{Map, Value, json};
use sqlx::{Postgres, Row, Transaction};
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use uuid::Uuid;

mod delivery_port;
mod integration;
mod messages;
mod requests;
pub use delivery_port::*;
pub use integration::*;
pub use requests::*;

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
}

/// Typed graph gateway port. Only node reads by UUID and validated
/// `MutationRequest` bodies cross it; no caller-chosen route or SQL.
#[async_trait::async_trait]
pub trait WorkGraph: Send + Sync {
    async fn node(&self, workspace: Uuid, id: Uuid) -> Result<Option<Value>>;
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

/// A catalog outcome and its schema-validated output.
#[derive(Clone, Debug, PartialEq)]
pub struct HostResult {
    pub outcome: String,
    pub output: Value,
}

#[path = "findings.rs"]
mod findings;

pub const HOST5_COMMANDS: [&str; 5] = [
    "context.read",
    "work.ensure",
    "work.change",
    "operation.cancel",
    "findings.reconcile",
];
/// Catalog 2.0.0 adds asynchronous judgments/checks and operation.get. D11
/// registers every catalog command: exactly the registry's twelve.
pub const HOST7_COMMANDS: [&str; 12] = [
    "context.read",
    "work.ensure",
    "work.change",
    "operation.cancel",
    "judgment.request",
    "check.request",
    "operation.get",
    "candidate.prepare",
    "candidate.publish",
    "findings.reconcile",
    "message.send",
    "decision.request",
];
const WORK_REF: &str = "aq.playbook.work/";
const ATTEMPT_REF: &str = "aq.execution.attempt/";
const PENDING: &str = "pending";

enum Stage {
    Retained(HostResult),
    Pending(Value),
    Fresh,
}

fn subject(id: impl ToString, revision: i64) -> Value {
    json!({"id": id.to_string(), "revision": revision.max(0)})
}
fn text<'a>(input: &'a Value, field: &str) -> Result<&'a str> {
    input[field].as_str().ok_or(Error::Invalid)
}
fn work_output(task: Uuid, revision: i64) -> Value {
    json!({"reference": format!("{WORK_REF}{task}"), "subject": subject(task, revision)})
}
fn node_version(node: &Value) -> Result<i64> {
    node["version"]
        .as_i64()
        .filter(|v| *v >= 1)
        .ok_or(Error::Unknown)
}

impl Service {
    fn ports(&self) -> Result<&HostPorts> {
        if !matches!(
            self.profile,
            CatalogProfile::Host5
                | CatalogProfile::Monitoring6
                | CatalogProfile::Host7
                | CatalogProfile::Host8
        ) {
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
        match intent.command.name.as_str() {
            "findings.reconcile" => self.findings_reconcile(owner, id, revision, intent).await,
            "context.read" => self.context_read(owner, id, revision, intent).await,
            "work.ensure" => self.work_ensure(owner, id, revision, intent).await,
            "work.change" => self.work_change(owner, id, revision, intent).await,
            "operation.cancel" => self.operation_cancel(owner, id, revision, intent).await,
            "judgment.request" => self.judgment_request(owner, id, revision, intent).await,
            "check.request" => self.check_request(owner, id, revision, intent).await,
            "operation.get" => self.host_operation_get(owner, id, revision, intent).await,
            "candidate.prepare" => self.candidate_prepare(owner, id, revision, intent).await,
            "candidate.publish" => self.candidate_publish(owner, id, revision, intent).await,
            "message.send" => self.host_message_send(owner, id, revision, intent).await,
            "decision.request" => {
                self.host_decision_request(owner, id, revision, intent)
                    .await
            }
            _ => Err(Error::Invalid),
        }
    }

    /// Reconcile reads only the retained final result; it never performs an
    /// effect and never refreshes write authority.
    pub async fn host_retained(
        &self,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<Option<HostResult>> {
        let (tx, stage, _) = self.begin(None, id, revision, intent).await?;
        tx.commit().await?;
        Ok(match stage {
            Some(Stage::Retained(result)) => Some(result),
            _ => None,
        })
    }

    /// Validates the intent, locks the namespace, and resolves the retained
    /// row. A fresh intent requires a live grant and an open deadline. With no
    /// owner (reconcile) no fresh stage is returned.
    async fn begin(
        &self,
        owner: Option<&RuntimeOwner>,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<(Transaction<'_, Postgres>, Option<Stage>, String)> {
        self.ports()?;
        let commands: &[&str] = if self.profile.is_host2() {
            &HOST7_COMMANDS
        } else {
            &HOST5_COMMANDS
        };
        if !commands.contains(&intent.command.name.as_str())
            || self.profile.registry().commands.get(&intent.command.name) != Some(&intent.command)
        {
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
        let identity = self.identity(intent)?;
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        if let Some(owner) = owner {
            self.runtime(&ns, owner)?;
        }
        let row = sqlx::query("SELECT command_name,request_digest,status,receipt FROM playbook_effects WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3 FOR UPDATE")
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(&intent.id)
            .fetch_optional(&mut *tx)
            .await?;
        if let Some(row) = row {
            let status: String = row.get("status");
            // Resuming a pending effect performs it, so it needs live authority.
            let live = status == PENDING && owner.is_some();
            let scope = self.grant_locked(&mut tx, id, revision, live).await?;
            self.intent_authority(intent, id, revision, &scope)?;
            if row.get::<String, _>("request_digest") != identity
                || row.get::<String, _>("command_name") != intent.command.name
            {
                return Err(Error::Conflict);
            }
            let receipt: Value = row.get("receipt");
            let stage = if status == PENDING {
                Stage::Pending(receipt)
            } else {
                Stage::Retained(HostResult {
                    outcome: status,
                    output: receipt,
                })
            };
            return Ok((tx, Some(stage), identity));
        }
        if owner.is_none() {
            let scope = self.grant_locked(&mut tx, id, revision, false).await?;
            self.intent_authority(intent, id, revision, &scope)?;
            return Ok((tx, None, identity));
        }
        let scope = self.grant_locked(&mut tx, id, revision, true).await?;
        self.intent_authority(intent, id, revision, &scope)?;
        if intent.deadline_at_ms <= self.now()? {
            return Err(Error::Forbidden);
        }
        Ok((tx, Some(Stage::Fresh), identity))
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
        stable_id(
            &format!("aq.playbook.{}.operation", intent.command.name),
            self.namespace.namespace_id,
            self.namespace.generation,
            &intent.id,
        )
    }

    /// Inserts the intent row (pending or final). Commit happens in the caller
    /// after expiry and deadline are rechecked under the namespace lock.
    #[allow(clippy::too_many_arguments)]
    async fn record(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        id: Uuid,
        revision: i64,
        intent: &Intent,
        identity: &str,
        status: &str,
        receipt: &Value,
    ) -> Result<()> {
        if status != PENDING {
            self.validated(intent, status, receipt)?;
        }
        let bytes = canonical_bytes(receipt).map_err(|_| Error::Invalid)?.len() as i64 + 4096;
        self.budget(tx, 0, 1, 0, 0, bytes).await?;
        sqlx::query("INSERT INTO playbook_effects(namespace_id,generation,intent_id,run_id,command_name,request_digest,command_fingerprint,input_digest,grant_id,grant_revision,operation_id,status,receipt,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)")
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(&intent.id)
            .bind(&intent.run)
            .bind(&intent.command.name)
            .bind(identity)
            .bind(intent.command.fingerprint().map_err(|_| Error::Invalid)?)
            .bind(&intent.input_digest)
            .bind(id)
            .bind(revision)
            .bind(self.operation(intent)?)
            .bind(status)
            .bind(receipt)
            .bind(self.now()?)
            .execute(&mut **tx)
            .await?;
        Ok(())
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

    /// Seals one aq.work.changed event into the outbox in the caller's tx.
    async fn work_event(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        intent: &Intent,
        outcome: &str,
        output: &Value,
    ) -> Result<()> {
        let event_id = stable_id(
            &format!("aq.playbook.{}.event", intent.command.name),
            self.namespace.namespace_id,
            self.namespace.generation,
            &intent.id,
        )?;
        self.seal_event(
            tx,
            "aq.work.changed",
            event_id,
            self.operation(intent)?,
            &intent.id,
            &output["subject"],
            json!({"outcome": outcome, "reference": output["reference"], "subject": output["subject"]}),
        )
        .await
    }

    /// Seals one trusted host-bridge event into the outbox in the caller's
    /// tx, validated against the pinned catalog event contract.
    #[allow(clippy::too_many_arguments)]
    async fn seal_event(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        contract: &str,
        event_id: Uuid,
        operation: Uuid,
        causation: &str,
        subject: &Value,
        payload: Value,
    ) -> Result<()> {
        let registry = self.profile.registry();
        let c = registry.events.get(contract).ok_or(Error::Invalid)?;
        let at = self.now()?;
        let event = EventEnvelope {
            producer: "aq.host.bridge@1".into(),
            event_id: event_id.to_string(),
            contract: ContractRef {
                name: c.name.clone(),
                fingerprint: c.fingerprint().map_err(|_| Error::Invalid)?,
            },
            scope: "project".into(),
            correlation: BTreeMap::from([("operation".into(), operation.to_string())]),
            subject: Some(text(subject, "id")?.to_string()),
            revision: Some(subject["revision"].as_u64().ok_or(Error::Invalid)?),
            causation: Some(causation.to_string()),
            occurred_at_ms: Some(at),
            observed_at_ms: Some(at),
            payload,
        };
        aq_playbook_host::model::validate_event(&event, &registry).map_err(|_| Error::Invalid)?;
        let event_digest = digest(&event)?;
        let bytes = canonical_bytes(&event).map_err(|_| Error::Invalid)?;
        let aad = self.aad("aq.playbooks.outbox", event_id, &event_digest)?;
        let sealed = self.keys.seal(&aad, &bytes).map_err(|_| Error::Unknown)?;
        self.budget(tx, 0, 0, 0, 1, sealed.ciphertext.len() as i64 + 1024)
            .await?;
        sqlx::query("INSERT INTO playbook_outbox(namespace_id,generation,producer,epoch,event_id,event_digest,sealed_version,key_id,nonce,ciphertext,created_at_ms,status) VALUES($1,$2,'aq.host.bridge@1',0,$3,$4,$5,$6,$7,$8,$9,'pending')")
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(event_id)
            .bind(event_digest)
            .bind(sealed.version as i32)
            .bind(sealed.key_id)
            .bind(sealed.nonce)
            .bind(sealed.ciphertext)
            .bind(at)
            .execute(&mut **tx)
            .await?;
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
        let (result, work) = self.work_accepted(intent, result)?;
        self.validated(intent, &result.outcome, &result.output)?;
        let mut tx = self.tx().await?;
        self.namespace_locked(&mut tx).await?;
        let row = sqlx::query("SELECT request_digest,status,receipt FROM playbook_effects WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3 FOR UPDATE")
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(&intent.id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(Error::Unknown)?;
        if row.get::<String, _>("request_digest") != identity {
            return Err(Error::Conflict);
        }
        let status: String = row.get("status");
        if status != PENDING {
            let retained = HostResult {
                outcome: status,
                output: row.get("receipt"),
            };
            tx.commit().await?;
            return Ok(retained);
        }
        sqlx::query("UPDATE playbook_effects SET status=$4,receipt=$5 WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3 AND status='pending'")
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(&intent.id)
            .bind(&result.outcome)
            .bind(&result.output)
            .execute(&mut *tx)
            .await?;
        self.effect_event(&mut tx, intent, &result, work.as_ref())
            .await?;
        tx.commit().await?;
        Ok(result)
    }

    /// Single-phase final row (no external effect remains), committed with
    /// its event.
    #[allow(clippy::too_many_arguments)]
    async fn settle_fresh(
        &self,
        mut tx: Transaction<'_, Postgres>,
        id: Uuid,
        revision: i64,
        intent: &Intent,
        identity: &str,
        result: HostResult,
    ) -> Result<HostResult> {
        let (result, work) = self.work_accepted(intent, result)?;
        self.record(
            &mut tx,
            id,
            revision,
            intent,
            identity,
            &result.outcome,
            &result.output,
        )
        .await?;
        self.effect_event(&mut tx, intent, &result, work.as_ref())
            .await?;
        self.commit_fresh(tx, id, intent).await?;
        Ok(result)
    }

    /// Catalog profile 7 work.ensure is asynchronous: an ensured node is
    /// returned as `accepted` with the durable operation handle, and the
    /// ensured work output is carried separately to be tracked. Every other
    /// profile and outcome is unchanged.
    fn work_accepted(
        &self,
        intent: &Intent,
        result: HostResult,
    ) -> Result<(HostResult, Option<Value>)> {
        if !self.profile.is_host2()
            || intent.command.name != "work.ensure"
            || result.outcome != "ensured"
        {
            return Ok((result, None));
        }
        let accepted = HostResult {
            outcome: "accepted".into(),
            output: json!({"handle": self.operation(intent)?.to_string()}),
        };
        Ok((accepted, Some(result.output)))
    }

    /// The outbox event (and, for accepted Host7 work, the running operation
    /// row) that commits with a final effect row.
    async fn effect_event(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        intent: &Intent,
        result: &HostResult,
        work: Option<&Value>,
    ) -> Result<()> {
        if let Some(work) = work {
            self.track_work(tx, intent, work).await?;
            return self.work_event(tx, intent, "ensured", work).await;
        }
        if matches!(result.outcome.as_str(), "ensured" | "changed") {
            self.work_event(tx, intent, &result.outcome, &result.output)
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

    // ---------------------------------------------------------------- work.ensure

    async fn work_ensure(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<HostResult> {
        let ports = self.ports()?;
        let workspace = self.namespace.workspace_id;
        let (mut tx, stage, identity) = self.begin(Some(owner), id, revision, intent).await?;
        let plan = match stage.ok_or(Error::Unknown)? {
            Stage::Retained(result) => {
                tx.commit().await?;
                return Ok(result);
            }
            Stage::Pending(plan) => {
                tx.commit().await?;
                plan
            }
            Stage::Fresh => {
                let input = &intent.input;
                let key = text(input, "logical_key")?;
                if key.is_empty() {
                    return Err(Error::Invalid);
                }
                // The logical key binds every declared field; a reuse with
                // different work is a conflict, never a silent re-point.
                let request = digest(&(
                    "aq.playbook.work.key",
                    text(input, "kind")?,
                    text(input, "lifetime")?,
                    text(input, "context")?,
                    &input["subject"],
                    text(input, "visibility")?,
                    key,
                ))?;
                let existing = sqlx::query("SELECT task_id,request_digest FROM playbook_work_keys WHERE namespace_id=$1 AND generation=$2 AND logical_key=$3 FOR UPDATE")
                    .bind(self.namespace.namespace_id)
                    .bind(self.namespace.generation)
                    .bind(key)
                    .fetch_optional(&mut *tx)
                    .await?;
                let task = match existing {
                    Some(row) => {
                        if row.get::<String, _>("request_digest") != request {
                            return Err(Error::Conflict);
                        }
                        row.get::<Uuid, _>("task_id")
                    }
                    None => {
                        let task = stable_id(
                            "aq.playbook.work.node",
                            self.namespace.namespace_id,
                            self.namespace.generation,
                            key,
                        )?;
                        sqlx::query("INSERT INTO playbook_work_keys(namespace_id,generation,logical_key,task_id,request_digest,created_at_ms) VALUES($1,$2,$3,$4,$5,$6)")
                            .bind(self.namespace.namespace_id)
                            .bind(self.namespace.generation)
                            .bind(key)
                            .bind(task)
                            .bind(&request)
                            .bind(self.now()?)
                            .execute(&mut *tx)
                            .await?;
                        task
                    }
                };
                if let Some(node) = ports.graph.node(workspace, task).await? {
                    let result = HostResult {
                        outcome: "ensured".into(),
                        output: work_output(task, node_version(&node)?),
                    };
                    return self
                        .settle_fresh(tx, id, revision, intent, &identity, result)
                        .await;
                }
                let plan = json!({"task": task, "body": self.create_body(task, input)?});
                self.record(&mut tx, id, revision, intent, &identity, PENDING, &plan)
                    .await?;
                self.commit_fresh(tx, id, intent).await?;
                plan
            }
        };
        let task: Uuid = serde_json::from_value(plan["task"].clone())?;
        let result = match ports.graph.node(workspace, task).await? {
            Some(node) => ("ensured", node_version(&node)?),
            None => match ports.graph.mutate(workspace, plan["body"].clone()).await? {
                (200..=299, raw) => ("ensured", node_version(&raw["results"][0])?),
                (409, _) => match ports.graph.node(workspace, task).await? {
                    Some(node) => ("ensured", node_version(&node)?),
                    None => ("refused", 0),
                },
                (400..=499, _) => ("refused", 0),
                _ => return Err(Error::Unknown),
            },
        };
        let mut output = work_output(task, result.1);
        if result.0 == "refused" {
            output["disposition"] = json!("failed");
        }
        self.finish(
            intent,
            &identity,
            HostResult {
                outcome: result.0.into(),
                output,
            },
        )
        .await
    }

    fn create_body(&self, task: Uuid, input: &Value) -> Result<Value> {
        let kind = text(input, "kind")?;
        let key = text(input, "logical_key")?;
        let mut title = format!("{kind}: {key}");
        if title.len() > 512 {
            let mut end = 512;
            while !title.is_char_boundary(end) {
                end -= 1;
            }
            title.truncate(end);
        }
        let mut operations = vec![json!({
            "op": "create_node",
            "id": task,
            "kind": "task",
            "title": title,
            "body": text(input, "context")?,
            "task_state": "active",
            "metadata": {
                "aq:playbook_namespace": self.namespace.namespace_id,
                "aq:project_id": self.namespace.project_id,
                "aq:logical_key": key,
                "aq:work_kind": kind,
                "aq:lifetime": text(input, "lifetime")?,
                "aq:visibility": text(input, "visibility")?,
                "aq:subject": input["subject"],
            },
        })];
        if let Ok(target) = Uuid::parse_str(text(&input["subject"], "id")?) {
            let revision = input["subject"]["revision"].as_i64().unwrap_or(0);
            operations.push(json!({
                "op": "add_edge",
                "source": task,
                "target": target,
                "edge_type": "motivated_by",
                "target_revision": if revision >= 1 { json!(revision) } else { Value::Null },
            }));
        }
        Ok(json!({
            "idempotency_key": format!("aq-playbook-ensure-{task}"),
            "reason": null,
            "operations": operations,
        }))
    }

    // ---------------------------------------------------------------- work.change

    async fn work_change(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<HostResult> {
        let ports = self.ports()?;
        let workspace = self.namespace.workspace_id;
        let (mut tx, stage, identity) = self.begin(Some(owner), id, revision, intent).await?;
        let plan = match stage.ok_or(Error::Unknown)? {
            Stage::Retained(result) => {
                tx.commit().await?;
                return Ok(result);
            }
            Stage::Pending(plan) => {
                tx.commit().await?;
                plan
            }
            Stage::Fresh => {
                let input = &intent.input;
                let subject_id = text(&input["subject"], "id")?;
                let expected = input["subject"]["revision"]
                    .as_i64()
                    .ok_or(Error::Invalid)?;
                let refused = |task: &str| HostResult {
                    outcome: "refused".into(),
                    output: json!({"reference": format!("{WORK_REF}{task}"), "subject": subject(task, expected)}),
                };
                let Ok(task) = Uuid::parse_str(subject_id) else {
                    return self
                        .settle_fresh(tx, id, revision, intent, &identity, refused(subject_id))
                        .await;
                };
                if !self.namespace_work(&mut tx, task).await? {
                    return self
                        .settle_fresh(tx, id, revision, intent, &identity, refused(subject_id))
                        .await;
                }
                let Some(node) = ports.graph.node(workspace, task).await? else {
                    return self
                        .settle_fresh(tx, id, revision, intent, &identity, refused(subject_id))
                        .await;
                };
                let current = node_version(&node)?;
                if current != expected {
                    // Stale expected version: a version refusal, no mutation.
                    let result = HostResult {
                        outcome: "conflict".into(),
                        output: work_output(task, current),
                    };
                    return self
                        .settle_fresh(tx, id, revision, intent, &identity, result)
                        .await;
                }
                let Some(body) = self.change_body(task, current, &node, input)? else {
                    return self
                        .settle_fresh(tx, id, revision, intent, &identity, refused(subject_id))
                        .await;
                };
                let plan = json!({"task": task, "expected": current, "body": body});
                self.record(&mut tx, id, revision, intent, &identity, PENDING, &plan)
                    .await?;
                self.commit_fresh(tx, id, intent).await?;
                plan
            }
        };
        let task: Uuid = serde_json::from_value(plan["task"].clone())?;
        let expected = plan["expected"].as_i64().ok_or(Error::Unknown)?;
        let result = match ports.graph.mutate(workspace, plan["body"].clone()).await? {
            (200..=299, raw) => {
                let version = raw["results"]
                    .as_array()
                    .and_then(|r| r.iter().find(|n| n["id"] == json!(task)))
                    .ok_or(Error::Unknown)
                    .and_then(node_version)?;
                ("changed", version)
            }
            (409, _) => {
                let version = match ports.graph.node(workspace, task).await? {
                    Some(node) => node_version(&node)?,
                    None => expected,
                };
                ("conflict", version)
            }
            (400..=499, _) => ("refused", expected),
            _ => return Err(Error::Unknown),
        };
        self.finish(
            intent,
            &identity,
            HostResult {
                outcome: result.0.into(),
                output: work_output(task, result.1),
            },
        )
        .await
    }

    /// Closed variants only. Lifecycle (`task_state`) is never patched.
    /// Returns None when the variant value is not admissible.
    fn change_body(
        &self,
        task: Uuid,
        expected: i64,
        node: &Value,
        input: &Value,
    ) -> Result<Option<Value>> {
        let value = text(input, "value")?;
        let hold_id = input["hold_id"].as_str();
        let mut metadata: Map<String, Value> =
            node["metadata"].as_object().cloned().unwrap_or_default();
        let mut patch = Map::new();
        let mut extra = vec![];
        match text(input, "variant")? {
            "requirements" => {
                patch.insert("acceptance_criteria".into(), json!(value));
            }
            "priority" => {
                let Ok(priority) = value.trim().parse::<i64>() else {
                    return Ok(None);
                };
                metadata.insert("aq:priority".into(), json!(priority));
            }
            "execution_constraints" => {
                metadata.insert("aq:execution_constraints".into(), json!(value));
            }
            "hold" => {
                let Some(hold) = hold_id.filter(|h| !h.is_empty()) else {
                    return Ok(None);
                };
                if metadata.contains_key("aq:hold") {
                    return Ok(None);
                }
                metadata.insert("aq:hold".into(), json!({"hold_id": hold, "reason": value}));
            }
            "release_hold" => {
                let Some(hold) = hold_id else {
                    return Ok(None);
                };
                if metadata.get("aq:hold").and_then(|h| h["hold_id"].as_str()) != Some(hold) {
                    return Ok(None);
                }
                metadata.remove("aq:hold");
            }
            "dependencies" => {
                let Ok(blocker) = Uuid::parse_str(value.trim()) else {
                    return Ok(None);
                };
                if blocker == task {
                    return Ok(None);
                }
                let mut deps: Vec<Value> = metadata
                    .get("aq:dependencies")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                if !deps.contains(&json!(blocker)) {
                    deps.push(json!(blocker));
                    extra.push(json!({"op": "add_edge", "source": blocker, "target": task, "edge_type": "blocks"}));
                }
                metadata.insert("aq:dependencies".into(), Value::Array(deps));
            }
            _ => return Ok(None),
        }
        if !patch.contains_key("acceptance_criteria") {
            patch.insert("metadata".into(), Value::Object(metadata));
        }
        let mut operations = vec![
            json!({"op": "update_node", "id": task, "expected_version": expected, "patch": patch}),
        ];
        operations.extend(extra);
        let key = stable_id(
            "aq.playbook.work.change.key",
            self.namespace.namespace_id,
            self.namespace.generation,
            &digest(&(task, expected, input))?,
        )?;
        Ok(Some(json!({
            "idempotency_key": format!("aq-playbook-change-{key}"),
            "reason": null,
            "operations": operations,
        })))
    }

    // ---------------------------------------------------------------- operation.cancel

    async fn operation_cancel(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<HostResult> {
        let ports = self.ports()?;
        let handle = text(&intent.input, "handle")?.to_string();
        let (mut tx, stage, identity) = self.begin(Some(owner), id, revision, intent).await?;
        let refused = |revision: i64| HostResult {
            outcome: "refused".into(),
            output: json!({"reference": handle, "subject": subject(&handle, revision)}),
        };
        let plan = match stage.ok_or(Error::Unknown)? {
            Stage::Retained(result) => {
                tx.commit().await?;
                return Ok(result);
            }
            Stage::Pending(plan) => {
                tx.commit().await?;
                plan
            }
            Stage::Fresh => {
                // A catalog 2.0.0 check operation handle (judgments are not
                // cancellable and fall through to refused).
                if self.profile.is_host2()
                    && let Some((check, subject)) = self.check_target(&handle).await?
                {
                    let plan = json!({"check": check, "subject": subject});
                    self.record(&mut tx, id, revision, intent, &identity, PENDING, &plan)
                        .await?;
                    self.commit_fresh(tx, id, intent).await?;
                    plan
                } else {
                    // Dispatch only by server-owned handle; never a caller route.
                    let attempt = if let Some(raw) = handle.strip_prefix(ATTEMPT_REF) {
                        match Uuid::parse_str(raw) {
                            Ok(attempt) => {
                                let project: Option<Uuid> = sqlx::query_scalar("SELECT o.project_id FROM execution_attempts a JOIN profiled_launches l USING(attempt_id) JOIN admission_source_observations o ON o.observation_id=l.source_observation_id WHERE a.attempt_id=$1 AND a.kind='profiled_v1'")
                                .bind(attempt)
                                .fetch_optional(&mut *tx)
                                .await?;
                                (project == Some(self.namespace.project_id)).then_some(attempt)
                            }
                            Err(_) => None,
                        }
                    } else if let Some(raw) = handle.strip_prefix(WORK_REF) {
                        match Uuid::parse_str(raw) {
                        Ok(task) if self.namespace_work(&mut tx, task).await? => {
                            sqlx::query_scalar::<_, Uuid>("SELECT attempt_id FROM execution_attempts WHERE task_id=$1 AND holds_task AND kind='profiled_v1'")
                                .bind(task)
                                .fetch_optional(&mut *tx)
                                .await?
                        }
                        _ => None,
                    }
                    } else {
                        None
                    };
                    let Some(attempt) = attempt else {
                        return self
                            .settle_fresh(tx, id, revision, intent, &identity, refused(0))
                            .await;
                    };
                    let plan = json!({"attempt": attempt, "operation": self.operation(intent)?});
                    self.record(&mut tx, id, revision, intent, &identity, PENDING, &plan)
                        .await?;
                    self.commit_fresh(tx, id, intent).await?;
                    plan
                }
            }
        };
        if !plan["check"].is_null() {
            // The executor stop alone completes the check; an ambiguous
            // executor cancel leaves the plan pending for a resumed dispatch.
            let check: Uuid = serde_json::from_value(plan["check"].clone())?;
            let outcome = self.cancel_check(check).await?;
            return self
                .finish(
                    intent,
                    &identity,
                    HostResult {
                        outcome: outcome.into(),
                        output: json!({"reference": handle, "subject": plan["subject"]}),
                    },
                )
                .await;
        }
        let attempt: Uuid = serde_json::from_value(plan["attempt"].clone())?;
        let operation: Uuid = serde_json::from_value(plan["operation"].clone())?;
        use crate::profiled_execution::cancellation::{Disposition, cancel};
        let reason = format!("playbook operation.cancel {}", self.namespace.namespace_id);
        // B3 owns cancellation; completed effects are never undone here.
        let (outcome, authority) = match cancel(&ports.state, attempt, operation, &reason).await {
            Ok(receipt) => (
                match receipt.disposition {
                    Disposition::Requested => "accepted",
                    Disposition::NeverGranted => "cancelled",
                    Disposition::AlreadyAccepted => "too_late",
                },
                i64::try_from(receipt.authority_revision).map_err(|_| Error::Unknown)?,
            ),
            Err(crate::error::AppError::NotFound(_) | crate::error::AppError::BadRequest(_)) => {
                ("refused", 0)
            }
            Err(crate::error::AppError::Conflict(_)) => ("indeterminate", 0),
            Err(_) => return Err(Error::Unknown),
        };
        self.finish(
            intent,
            &identity,
            HostResult {
                outcome: outcome.into(),
                output: json!({"reference": handle, "subject": subject(attempt, authority)}),
            },
        )
        .await
    }

    // ---------------------------------------------------------------- context.read

    /// Typed views only, observed in one transaction and retained per intent.
    async fn context_read(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<HostResult> {
        let ports = self.ports()?;
        let (mut tx, stage, identity) = self.begin(Some(owner), id, revision, intent).await?;
        match stage.ok_or(Error::Unknown)? {
            Stage::Retained(result) => {
                tx.commit().await?;
                return Ok(result);
            }
            Stage::Pending(_) => return Err(Error::Unknown),
            Stage::Fresh => {}
        }
        let input = &intent.input;
        let subject_id = text(&input["subject"], "id")?.to_string();
        let asked = text(input, "snapshot")?.to_string();
        let now = self.now()?;
        let project = self.namespace.project_id;
        // (outcome, reference, subject revision, snapshot, complete)
        let view: (&str, String, i64, String, bool) = match text(input, "view")? {
            "work" => match Uuid::parse_str(&subject_id) {
                Ok(task) if self.namespace_work(&mut tx, task).await? => {
                    match ports.graph.node(self.namespace.workspace_id, task).await? {
                        Some(node) => {
                            let v = node_version(&node)?;
                            (
                                "observed",
                                format!("{WORK_REF}{task}"),
                                v,
                                format!("v{v}"),
                                true,
                            )
                        }
                        None => (
                            "refused",
                            format!("{WORK_REF}{task}"),
                            0,
                            String::new(),
                            false,
                        ),
                    }
                }
                _ => ("refused", subject_id.clone(), 0, String::new(), false),
            },
            "project" => {
                let row: Option<(i64, String)> = sqlx::query_as("SELECT revision,md5(binding::text) FROM project_execution_bindings WHERE project_id=$1")
                    .bind(project)
                    .fetch_optional(&mut *tx)
                    .await?;
                match row {
                    Some((r, d)) => ("observed", format!("aq.project/{project}"), r, d, true),
                    None => (
                        "incomplete",
                        format!("aq.project/{project}"),
                        0,
                        String::new(),
                        false,
                    ),
                }
            }
            "execution_options" => {
                let row: Option<(Uuid, String)> = match Uuid::parse_str(&subject_id) {
                    Ok(attempt) => sqlx::query_as("SELECT s.attempt_id,s.selection_digest FROM intelligence_attempt_selections s JOIN profiled_launches l USING(attempt_id) JOIN admission_source_observations o ON o.observation_id=l.source_observation_id WHERE s.attempt_id=$1 AND o.project_id=$2")
                        .bind(attempt)
                        .bind(project)
                        .fetch_optional(&mut *tx)
                        .await?,
                    Err(_) => None,
                };
                match row {
                    Some((a, d)) => ("observed", format!("{ATTEMPT_REF}{a}"), 0, d, true),
                    None => ("incomplete", subject_id.clone(), 0, String::new(), false),
                }
            }
            "evidence" => {
                let row: Option<(Uuid, String)> = sqlx::query_as("SELECT result_id,report_revision FROM result_revision_pins WHERE project_id=$1 AND report_id::text=$2")
                    .bind(project)
                    .bind(&subject_id)
                    .fetch_optional(&mut *tx)
                    .await?;
                match row {
                    Some((r, rev)) => ("observed", format!("aq.result/{r}"), 0, rev, true),
                    None => ("incomplete", subject_id.clone(), 0, String::new(), false),
                }
            }
            "health_observations"
                if matches!(
                    self.profile,
                    CatalogProfile::Monitoring6 | CatalogProfile::Host7 | CatalogProfile::Host8
                ) =>
            {
                if input["page"].as_u64() != Some(0) {
                    ("incomplete", subject_id.clone(), 0, String::new(), false)
                } else {
                    let reference = format!("aq.health/{}", self.operation(intent)?);
                    let baseline = self
                        .capture_health_scan(&mut tx, &reference, &subject_id, 60_000)
                        .await?;
                    let snapshot = serde_json::to_string(&baseline)?;
                    if snapshot.len() > 4096 {
                        ("incomplete", reference, 0, String::new(), false)
                    } else {
                        ("observed", reference, 0, snapshot, true)
                    }
                }
            }
            // Historical profile 5 keeps its original incomplete health view.
            _ => ("incomplete", subject_id.clone(), 0, String::new(), false),
        };
        let outcome = if view.0 == "observed" && !asked.is_empty() && asked != view.3 {
            "expired"
        } else {
            view.0
        };
        let output = json!({
            "complete": view.4 && outcome == "observed",
            "expires_at_ms": now + 60_000,
            "next_page": null,
            "observed_at_ms": now,
            "reference": view.1,
            "snapshot": view.3,
            "subject": subject(&subject_id, view.2),
            "visibility": "project",
        });
        self.settle_fresh(
            tx,
            id,
            revision,
            intent,
            &identity,
            HostResult {
                outcome: outcome.into(),
                output,
            },
        )
        .await
    }
}
