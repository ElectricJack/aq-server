//! D7 host adapters candidate.prepare and candidate.publish (catalog 2.0.0).
//!
//! The D1 delivery service sits behind the injected [`Integration`] port. The
//! intent (playbook_effects row) and a `pending` receipt commit before the
//! port is called; the final effect row, the settled receipt and the sealed
//! `aq.integration.changed` event commit together.
//!
//! Publication is guarded: every evidence item the project's policy names
//! must resolve in its owning store (D6 check/judgment operations, the review
//! service), come from an authorized issuer and be bound to this exact
//! candidate and its OID. Anything else refuses with a typed reason and the
//! port is never called. A publish whose reply was lost is `ambiguous` and
//! only ever settles by reconciliation, never by a second publish.
use super::*;
use crate::delivery::PublicationPolicy;
pub use aq_playbook_domain::publication_evidence::{EvidencePolicy, ReviewEvidence};
use aq_playbook_domain::publication_evidence::{EvidenceRef, EvidenceSet};
use aq_worker_protocol::{ObjectAlgorithm, ObjectId};
use serde::{Deserialize, Serialize};

use aq_playbook_domain::integration_receipts::{
    CANDIDATE_REF, IntegrationReceipts, PUBLICATION_REF,
};
const SOURCE_REF: &str = "aq.source/";
const MAX_SOURCES: usize = 64;
const MAX_CONFLICT_PATHS: usize = 32;
const MAX_RECONCILE: i32 = 64;

/// The project's revisioned repository binding and policies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrationBinding {
    pub repository_id: Uuid,
    pub binding_generation: u64,
    pub publication: PublicationPolicy,
    pub evidence: EvidencePolicy,
}

impl IntegrationBinding {
    fn prepare_call(&self, operation: Uuid, base: ObjectId, sources: Vec<Uuid>) -> PrepareCall {
        PrepareCall {
            operation,
            repository_id: self.repository_id,
            binding_generation: self.binding_generation,
            base,
            sources,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PrepareCall {
    /// Stable per intent; also the D1 candidate operation id.
    pub operation: Uuid,
    pub repository_id: Uuid,
    pub binding_generation: u64,
    pub base: ObjectId,
    /// Retained source receipt ids, in input order.
    pub sources: Vec<Uuid>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishCall {
    /// Stable per intent; the D1 dispatch fence key.
    pub operation: Uuid,
    pub repository_id: Uuid,
    pub binding_generation: u64,
    pub candidate_operation: Uuid,
    pub candidate: ObjectId,
    pub publication: PublicationPolicy,
    pub expected_old: ObjectId,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileCall {
    /// A fresh operation per attempt; never the publication operation.
    pub operation: Uuid,
    pub repository_id: Uuid,
    pub binding_generation: u64,
    pub target_operation: Uuid,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PrepareReply {
    Prepared {
        candidate: ObjectId,
    },
    Conflict {
        step: Option<u64>,
        paths: Vec<String>,
        truncated: bool,
        count: u64,
    },
    Refused {
        reason: String,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublishReply {
    Published {
        old: ObjectId,
        new: ObjectId,
    },
    /// The target already holds the candidate (an earlier attempt landed).
    ObservedPresent {
        observed: ObjectId,
    },
    Refused {
        reason: String,
    },
    /// Dispatched without an acknowledgement.
    Ambiguous,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ReconcileReply {
    Present { observed: ObjectId },
    Resolved { disposition: String },
    Unresolved,
}

/// D1 delivery port. Authority never comes from here: it comes from the grant.
#[async_trait::async_trait]
pub trait Integration: Send + Sync {
    async fn binding(&self, workspace: Uuid, project: Uuid) -> Result<Option<IntegrationBinding>>;
    async fn prepare(&self, call: &PrepareCall) -> Result<PrepareReply>;
    async fn publish(&self, call: &PublishCall) -> Result<PublishReply>;
    async fn reconcile(&self, call: &ReconcileCall) -> Result<ReconcileReply>;
    async fn review(
        &self,
        workspace: Uuid,
        project: Uuid,
        decision: Uuid,
    ) -> Result<Option<ReviewEvidence>>;
}

/// `sha1:<40 hex>`; D1 OIDs are sha1-only.
fn oid(text: &str) -> Option<ObjectId> {
    let hex = text.strip_prefix("sha1:")?;
    (hex.len() == 40
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then(|| ObjectId {
        algorithm: ObjectAlgorithm::Sha1,
        hex: hex.into(),
    })
}
/// The catalog form of an OID (`{format, hex}`).
fn catalog_oid(oid: &ObjectId) -> Value {
    json!({"format": "sha1", "hex": oid.hex})
}
/// `[<ref>@]<oid>`.
fn base_oid(id: &str) -> Option<ObjectId> {
    oid(id.rsplit_once('@').map_or(id, |(_, o)| o))
}
/// `refs/heads/<branch>@<oid>`: target and expected old OID.
fn destination(id: &str) -> Option<(&str, ObjectId)> {
    let (target, old) = id.rsplit_once('@')?;
    (target.len() > "refs/heads/".len() && target.starts_with("refs/heads/"))
        .then_some(())
        .and_then(|_| Some((target, oid(old)?)))
}

/// What a receipt row records, besides the intent identity.
struct SettledIntegration {
    result: HostResult,
    reason: Option<String>,
    detail: Value,
}
struct RefusedIntegration<'a> {
    candidate_operation: Uuid,
    binding: Option<&'a IntegrationBinding>,
    reason: &'a str,
    output: &'a Value,
}
struct Publication<'a> {
    port: &'a dyn Integration,
    call: &'a PublishCall,
    output: Value,
}
struct PublicationResume<'a> {
    intent: &'a Intent,
    identity: &'a str,
    operation: Uuid,
    plan: Value,
    output: Value,
}
struct IntegrationAttempt<'a> {
    effect: EffectIdentity<'a>,
    operation: Uuid,
    output: Value,
}
struct CandidateReceipt<'a> {
    operation: Uuid,
    output: &'a Value,
}
impl<'a> CandidateReceipt<'a> {
    fn refused(
        &self,
        binding: Option<&'a IntegrationBinding>,
        reason: &'a str,
    ) -> RefusedIntegration<'a> {
        RefusedIntegration {
            candidate_operation: self.operation,
            binding,
            reason,
            output: self.output,
        }
    }
    fn pending(
        &self,
        operation: Uuid,
        binding: &'a IntegrationBinding,
        request: Value,
    ) -> Receipt<'a> {
        Receipt {
            operation,
            candidate_operation: self.operation,
            binding: Some(binding),
            state: PENDING,
            reason: None,
            request: merged(self.output, request),
            detail: Value::Null,
        }
    }
}
struct PublicationCandidate<'a> {
    id: &'a str,
    operation: Uuid,
    binding: &'a IntegrationBinding,
    input: &'a Value,
}
struct VerifiedPublication {
    candidate: ObjectId,
    expected_old: ObjectId,
    evidence: Vec<String>,
}
impl PublicationCandidate<'_> {
    fn publish_call(
        &self,
        operation: Uuid,
        verified: VerifiedPublication,
    ) -> (PublishCall, Vec<String>) {
        let call = PublishCall {
            operation,
            repository_id: self.binding.repository_id,
            binding_generation: self.binding.binding_generation,
            candidate_operation: self.operation,
            candidate: verified.candidate,
            publication: self.binding.publication.clone(),
            expected_old: verified.expected_old,
        };
        (call, verified.evidence)
    }
}
struct Receipt<'a> {
    operation: Uuid,
    candidate_operation: Uuid,
    binding: Option<&'a IntegrationBinding>,
    state: &'a str,
    reason: Option<&'a str>,
    request: Value,
    detail: Value,
}

/// How check.request binds its source for a subject (D9 follow-up).
pub(super) enum CheckSource {
    /// Not a candidate subject: the run input's source is used.
    Input,
    /// A prepared candidate: its OID in catalog form, replacing input.source.
    Candidate(Value),
    /// A candidate reference with no prepared candidate behind it.
    NotPrepared,
}

impl Service {
    /// The source a check on `subject_id` runs on. Catalog 2.0.0's
    /// candidate.prepare output carries no OID, so for `aq.candidate/<op>`
    /// subjects the host binds the prepared candidate's OID from its receipt
    /// and ignores the caller's input.source; checks then run on, record and
    /// echo the exact candidate bytes publication verifies against.
    pub(super) async fn check_source(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        subject_id: &str,
    ) -> Result<CheckSource> {
        if !subject_id.starts_with(CANDIDATE_REF) {
            return Ok(CheckSource::Input);
        }
        let Some(op) = handle(subject_id, CANDIDATE_REF) else {
            return Ok(CheckSource::NotPrepared);
        };
        let detail: Option<Option<Value>> = sqlx::query_scalar("SELECT detail FROM playbook_integration_receipts WHERE operation_id=$1 AND namespace_id=$2 AND generation=$3 AND command_name='candidate.prepare' AND state='prepared'")
            .bind(op)
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .fetch_optional(&mut **tx)
            .await?;
        let Some(detail) = detail.flatten() else {
            return Ok(CheckSource::NotPrepared);
        };
        let candidate: ObjectId =
            serde_json::from_value(detail["candidate"].clone()).map_err(|_| Error::Unknown)?;
        Ok(CheckSource::Candidate(catalog_oid(&candidate)))
    }

    /// The retained source receipt a candidate source names: an explicit
    /// `aq.source/<receipt>`, or (D11) a completed work item of this
    /// namespace whose task lifecycle recorded `aq:source_receipt` on the
    /// node. Anything else is `None` (refused `invalid_request`).
    async fn source_receipt(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ports: &HostPorts,
        item: &Value,
    ) -> Result<Option<Uuid>> {
        let Some(id) = item["id"].as_str() else {
            return Ok(None);
        };
        if let Some(receipt) = handle(id, SOURCE_REF) {
            return Ok(Some(receipt));
        }
        let Ok(task) = Uuid::parse_str(id) else {
            return Ok(None);
        };
        if !self.namespace_work(tx, task).await? {
            return Ok(None);
        }
        let Some(node) = ports.graph.node(self.namespace.workspace_id, task).await? else {
            return Ok(None);
        };
        if node["task_state"] != "completed" {
            return Ok(None);
        }
        Ok(node["metadata"]["aq:source_receipt"]
            .as_str()
            .and_then(|r| Uuid::parse_str(r).ok()))
    }

    async fn insert_receipt(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        intent: &Intent,
        r: Receipt<'_>,
    ) -> Result<()> {
        let now = self.now()?;
        let settled = (!matches!(r.state, PENDING | "ambiguous")).then_some(now);
        sqlx::query("INSERT INTO playbook_integration_receipts(operation_id,namespace_id,generation,intent_id,command_name,candidate_operation_id,repository_id,binding_generation,state,reason,request,detail,created_at_ms,settled_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)")
            .bind(r.operation)
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(&intent.id)
            .bind(&intent.command.name)
            .bind(r.candidate_operation)
            .bind(r.binding.map(|b| b.repository_id))
            .bind(r.binding.map(|b| b.binding_generation as i64))
            .bind(r.state)
            .bind(r.reason)
            .bind(&r.request)
            .bind((!r.detail.is_null()).then_some(&r.detail))
            .bind(now)
            .bind(settled)
            .execute(&mut **tx)
            .await?;
        Ok(())
    }

    /// Seals one aq.integration.changed event in the caller's tx.
    async fn integration_event(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        intent: &Intent,
        result: &HostResult,
    ) -> Result<()> {
        let event_id = stable_id(
            &format!("aq.playbook.{}.event", intent.command.name),
            self.namespace.namespace_id,
            self.namespace.generation,
            &intent.id,
        )?;
        let output = &result.output;
        self.seal_event(tx, HostEvent { contract: "aq.integration.changed", event_id, operation: self.operation(intent)?, causation: &intent.id, subject: &output["subject"], payload: json!({"outcome": result.outcome, "reference": output["reference"], "subject": output["subject"]}) })
        .await
    }

    /// Single-phase refusal: final effect row, refused receipt and event.
    async fn refuse_fresh(
        &self,
        mut tx: Transaction<'_, Postgres>,
        effect: EffectIdentity<'_>,
        refusal: RefusedIntegration<'_>,
    ) -> Result<HostResult> {
        let RefusedIntegration {
            candidate_operation,
            binding,
            reason,
            output,
        } = refusal;
        let (id, intent) = (effect.grant, effect.intent);
        let result = HostResult {
            outcome: "refused".into(),
            output: output.clone(),
        };
        self.record(
            &mut tx,
            effect,
            EffectReceipt::new(&result.outcome, &result.output),
        )
        .await?;
        self.insert_receipt(
            &mut tx,
            intent,
            Receipt {
                operation: self.operation(intent)?,
                candidate_operation,
                binding,
                state: "refused",
                reason: Some(reason),
                request: merged(output, json!({"input": intent.input})),
                detail: Value::Null,
            },
        )
        .await?;
        self.integration_event(&mut tx, intent, &result).await?;
        self.commit_fresh(tx, id, intent).await?;
        Ok(result)
    }

    /// Settles a pending intent and its receipt with the event, exactly once.
    /// A row already settled by another resume is returned as is.
    async fn settle_integration(
        &self,
        intent: &Intent,
        identity: &str,
        settlement: SettledIntegration,
    ) -> Result<HostResult> {
        let SettledIntegration {
            result,
            reason,
            detail,
        } = settlement;
        self.validated(intent, &result.outcome, &result.output)?;
        let mut tx = self.tx().await?;
        self.namespace_locked(&mut tx).await?;
        let effects = Effects(&self.namespace);
        if let Some(retained) = effects.completed(&mut tx, intent, identity).await? {
            tx.commit().await?;
            return Ok(retained);
        }
        effects.complete(&mut tx, intent, &result).await?;
        let updated = sqlx::query("UPDATE playbook_integration_receipts SET state=$2,reason=$3,detail=$4,settled_at_ms=$5 WHERE operation_id=$1 AND state IN ('pending','ambiguous')")
            .bind(self.operation(intent)?)
            .bind(&result.outcome)
            .bind(reason)
            .bind((!detail.is_null()).then_some(&detail))
            .bind(self.now()?)
            .execute(&mut *tx)
            .await?;
        if updated.rows_affected() != 1 {
            return Err(Error::Unknown);
        }
        self.integration_event(&mut tx, intent, &result).await?;
        tx.commit().await?;
        Ok(result)
    }

    async fn record_integration_plan(
        &self,
        mut tx: Transaction<'_, Postgres>,
        effect: EffectIdentity<'_>,
        receipt: Receipt<'_>,
    ) -> Result<()> {
        self.record(
            &mut tx,
            effect,
            EffectReceipt::new(PENDING, &receipt.request["call"]),
        )
        .await?;
        self.insert_receipt(&mut tx, effect.intent, receipt).await?;
        self.commit_fresh(tx, effect.grant, effect.intent).await
    }

    async fn candidate_sources(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        ports: &HostPorts,
        items: &Value,
    ) -> Result<Option<Vec<Uuid>>> {
        let mut receipts = Vec::new();
        for item in items.as_array().into_iter().flatten() {
            receipts.push(self.source_receipt(tx, ports, item).await?);
        }
        let Some(sources) = receipts.into_iter().collect::<Option<Vec<_>>>() else {
            return Ok(None);
        };
        let unique: std::collections::BTreeSet<_> = sources.iter().collect();
        let valid_length = (1..=MAX_SOURCES).contains(&sources.len());
        Ok((valid_length && unique.len() == sources.len()).then_some(sources))
    }

    // --------------------------------------------------------- candidate.prepare

    pub(super) async fn candidate_prepare(
        &self,
        owner: &RuntimeOwner,
        command: HostIntent<'_>,
    ) -> Result<HostResult> {
        let intent = command.intent;
        let ports = self.ports()?;
        let (mut tx, stage, identity) = self.begin(Some(owner), command).await?;
        let operation = self.operation(intent)?;
        let reference = format!("{CANDIDATE_REF}{operation}");
        // A candidate is immutable: revision 1 for its whole life.
        let output = json!({"reference": reference, "subject": subject(&reference, 1)});
        let call = match stage.ok_or(Error::Unknown)? {
            Stage::Retained(result) => {
                tx.commit().await?;
                return Ok(result);
            }
            // Keyed-idempotent resume: the same operation id is re-sent.
            Stage::Pending(plan) => {
                tx.commit().await?;
                serde_json::from_value(plan).map_err(|_| Error::Unknown)?
            }
            Stage::Fresh => {
                let receipt = CandidateReceipt {
                    operation,
                    output: &output,
                };
                let refuse = |tx, binding, reason| {
                    self.refuse_fresh(
                        tx,
                        command.retained(&identity),
                        receipt.refused(binding, reason),
                    )
                };
                let Some(port) = &ports.integration else {
                    return refuse(tx, None, "unavailable").await;
                };
                let workspace = self.namespace.workspace_id;
                let Some(binding) = port.binding(workspace, self.namespace.project_id).await?
                else {
                    return refuse(tx, None, "policy_unavailable").await;
                };
                let input = &intent.input;
                let base = text(&input["base"], "id").ok().and_then(base_oid);
                let sources = self
                    .candidate_sources(&mut tx, ports, &input["sources"])
                    .await?;
                let (Some(base), Some(sources)) = (base, sources) else {
                    return refuse(tx, Some(&binding), "invalid_request").await;
                };
                let call = binding.prepare_call(operation, base, sources);
                let plan = serde_json::to_value(&call)?;
                self.record_integration_plan(
                    tx,
                    command.retained(&identity),
                    receipt.pending(operation, &binding, json!({"call": plan})),
                )
                .await?;

                call
            }
        };
        let port = ports.integration.as_ref().ok_or(Error::Unknown)?;
        // A failed call leaves the intent pending; re-dispatch re-sends it.
        let reply = port.prepare(&call).await.map_err(|_| Error::Unknown)?;
        let settlement = reply.settlement(output, self.profile.has_prepared_source())?;
        self.settle_integration(intent, &identity, settlement).await
    }

    // --------------------------------------------------------- candidate.publish

    pub(super) async fn candidate_publish(
        &self,
        owner: &RuntimeOwner,
        command: HostIntent<'_>,
    ) -> Result<HostResult> {
        let intent = command.intent;
        let ports = self.ports()?;
        let (tx, stage, identity) = self.begin(Some(owner), command).await?;
        let operation = self.operation(intent)?;
        let reference = format!("{PUBLICATION_REF}{operation}");
        let input = &intent.input;
        let output = json!({"reference": reference, "subject": input["candidate"]});
        match stage.ok_or(Error::Unknown)? {
            Stage::Retained(result) => {
                tx.commit().await?;
                return Ok(result);
            }
            Stage::Pending(plan) => {
                return self
                    .resume_publication(
                        tx,
                        PublicationResume {
                            intent,
                            identity: &identity,
                            operation,
                            plan,
                            output,
                        },
                        ports,
                    )
                    .await;
            }
            Stage::Fresh => {}
        }
        self.fresh_publication(
            tx,
            IntegrationAttempt {
                effect: command.retained(&identity),
                operation,
                output,
            },
            ports,
        )
        .await
    }

    async fn fresh_publication(
        &self,
        mut tx: Transaction<'_, Postgres>,
        attempt: IntegrationAttempt<'_>,
        ports: &HostPorts,
    ) -> Result<HostResult> {
        let IntegrationAttempt {
            effect,
            operation,
            output,
        } = attempt;
        let input = &effect.intent.input;
        let candidate_id = text(&input["candidate"], "id")?;
        let candidate_op = handle(candidate_id, CANDIDATE_REF);
        let receipt = CandidateReceipt {
            operation: candidate_op.unwrap_or(Uuid::nil()),
            output: &output,
        };
        let refuse =
            |tx, binding, reason| self.refuse_fresh(tx, effect, receipt.refused(binding, reason));
        let Some(port) = &ports.integration else {
            return refuse(tx, None, "unavailable").await;
        };
        let Some(candidate_op) = candidate_op else {
            return refuse(tx, None, "candidate_unknown").await;
        };
        let workspace = self.namespace.workspace_id;
        let project = self.namespace.project_id;
        let binding = match port.binding(workspace, project).await? {
            Some(b) if !b.evidence.names_nothing() => b,
            _ => return refuse(tx, None, "policy_unavailable").await,
        };
        let candidate = PublicationCandidate {
            id: candidate_id,
            operation: candidate_op,
            binding: &binding,
            input,
        };
        let verified = match self
            .verified_publication(&mut tx, port.as_ref(), &candidate)
            .await?
        {
            Ok(verified) => verified,
            Err(reason) => return refuse(tx, Some(&binding), reason).await,
        };
        let (call, evidence) = candidate.publish_call(operation, verified);
        let plan = serde_json::to_value(&call)?;
        self.record_integration_plan(
            tx,
            effect,
            receipt.pending(
                operation,
                &binding,
                json!({"call": plan, "evidence": evidence}),
            ),
        )
        .await?;

        self.publish_send(
            effect.intent,
            effect.identity,
            Publication {
                port: port.as_ref(),
                call: &call,
                output,
            },
        )
        .await
    }

    async fn resume_publication(
        &self,
        mut tx: Transaction<'_, Postgres>,
        pending: PublicationResume<'_>,
        ports: &HostPorts,
    ) -> Result<HostResult> {
        let state: String = sqlx::query_scalar(
            "SELECT state FROM playbook_integration_receipts WHERE operation_id=$1",
        )
        .bind(pending.operation)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        let call: PublishCall = serde_json::from_value(pending.plan).map_err(|_| Error::Unknown)?;
        let port = ports.integration.as_ref().ok_or(Error::Unknown)?;
        let publication = Publication {
            port: port.as_ref(),
            call: &call,
            output: pending.output,
        };
        if state == "ambiguous" {
            self.publish_reconcile(pending.intent, pending.identity, publication)
                .await
        } else {
            // An unacknowledged operation is sent again under the original dispatch fence.
            self.publish_send(pending.intent, pending.identity, publication)
                .await
        }
    }

    async fn verified_publication(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        port: &dyn Integration,
        candidate: &PublicationCandidate<'_>,
    ) -> Result<std::result::Result<VerifiedPublication, &'static str>> {
        let (oid, expected) = match self.publication_target(tx, candidate).await? {
            Ok(target) => target,
            Err(reason) => return Ok(Err(reason)),
        };
        let verification = EvidenceSet::new(
            &candidate.binding.evidence,
            &candidate.input["evidence"],
            candidate.id,
            catalog_oid(&oid),
        );
        let evidence = self.verify_evidence(tx, port, verification).await?;
        Ok(evidence.map(|evidence| VerifiedPublication {
            candidate: oid,
            expected_old: expected,
            evidence,
        }))
    }

    async fn publication_target(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        candidate: &PublicationCandidate<'_>,
    ) -> Result<std::result::Result<(ObjectId, ObjectId), &'static str>> {
        let PublicationCandidate {
            operation: candidate_op,
            binding,
            input,
            ..
        } = *candidate;
        let Some(candidate) = sqlx::query("SELECT state,repository_id,binding_generation,detail FROM playbook_integration_receipts WHERE operation_id=$1 AND namespace_id=$2 AND generation=$3 AND command_name='candidate.prepare'")
            .bind(candidate_op)
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .fetch_optional(&mut **tx)
            .await?
        else {
            return Ok(Err("candidate_unknown"));
        };
        if input["candidate"]["revision"].as_i64() != Some(1) {
            return Ok(Err("candidate_stale"));
        }
        if candidate.get::<String, _>("state") != "prepared" {
            return Ok(Err("candidate_not_ready"));
        }
        if candidate.get::<Option<Uuid>, _>("repository_id") != Some(binding.repository_id)
            || candidate.get::<Option<i64>, _>("binding_generation")
                != Some(binding.binding_generation as i64)
        {
            return Ok(Err("binding_changed"));
        }
        let detail: Option<Value> = candidate.get("detail");
        let candidate_oid: ObjectId =
            serde_json::from_value(detail.ok_or(Error::Unknown)?["candidate"].clone())
                .map_err(|_| Error::Unknown)?;
        let Some((target, expected_old)) =
            text(&input["destination"], "id").ok().and_then(destination)
        else {
            return Ok(Err("invalid_request"));
        };
        if target != binding.publication.target {
            return Ok(Err("destination_mismatch"));
        }
        Ok(Ok((candidate_oid, expected_old)))
    }

    async fn publish_send(
        &self,
        intent: &Intent,
        identity: &str,
        publication: Publication<'_>,
    ) -> Result<HostResult> {
        let Publication { port, call, output } = publication;
        // A transport failure after dispatch is indistinguishable from a lost
        // acknowledgement.
        let reply = port.publish(call).await.unwrap_or(PublishReply::Ambiguous);
        let (outcome, reason, detail) = match reply {
            PublishReply::Published { old, new } => {
                ("published", None, json!({"old": old, "new": new}))
            }
            PublishReply::ObservedPresent { observed } => {
                ("published", None, json!({"observed": observed}))
            }
            PublishReply::Refused { reason }
                if matches!(reason.as_str(), "target_moved" | "conflicted") =>
            {
                ("conflict", Some(reason), Value::Null)
            }
            PublishReply::Refused { reason } => (
                "refused",
                Some(token(&reason, "publish_failed")),
                Value::Null,
            ),
            PublishReply::Ambiguous => {
                sqlx::query("UPDATE playbook_integration_receipts SET state='ambiguous' WHERE operation_id=$1 AND state='pending'")
                    .bind(call.operation)
                    .execute(&self.pool)
                    .await?;
                return Err(Error::Unknown);
            }
        };
        let result = HostResult {
            outcome: outcome.into(),
            output,
        };
        self.settle_integration(
            intent,
            identity,
            SettledIntegration {
                result,
                reason,
                detail,
            },
        )
        .await
    }

    /// Settles an ambiguous publication by asking D1 what happened, under a
    /// fresh operation per attempt. Never publishes again.
    async fn publish_reconcile(
        &self,
        intent: &Intent,
        identity: &str,
        publication: Publication<'_>,
    ) -> Result<HostResult> {
        let Publication { port, call, output } = publication;
        let attempt: Option<i32> = sqlx::query_scalar("UPDATE playbook_integration_receipts SET reconcile_attempts=reconcile_attempts+1 WHERE operation_id=$1 AND state='ambiguous' AND reconcile_attempts < $2 RETURNING reconcile_attempts")
            .bind(call.operation)
            .bind(MAX_RECONCILE)
            .fetch_optional(&self.pool)
            .await?;
        let attempt = attempt.ok_or(Error::Unknown)?;
        let reconcile = ReconcileCall {
            operation: stable_id(
                "aq.playbook.candidate.publish.reconcile",
                self.namespace.namespace_id,
                self.namespace.generation,
                &format!("{}#{attempt}", intent.id),
            )?,
            repository_id: call.repository_id,
            binding_generation: call.binding_generation,
            target_operation: call.operation,
        };
        let (outcome, reason, detail) = match port
            .reconcile(&reconcile)
            .await
            .map_err(|_| Error::Unknown)?
        {
            ReconcileReply::Present { observed } => (
                "published",
                None,
                json!({"observed": observed, "reconciled_by": reconcile.operation}),
            ),
            ReconcileReply::Resolved { disposition } => (
                "refused",
                Some(token(&format!("resolved_{disposition}"), "publish_failed")),
                json!({"reconciled_by": reconcile.operation}),
            ),
            ReconcileReply::Unresolved => return Err(Error::Unknown),
        };
        let result = HostResult {
            outcome: outcome.into(),
            output,
        };
        self.settle_integration(
            intent,
            identity,
            SettledIntegration {
                result,
                reason,
                detail,
            },
        )
        .await
    }

    /// Resolves every evidence item in its owning store. Returns the verified
    /// references in input order, or the typed refusal reason. Coverage of
    /// the policy is checked last so a bad item reports its own reason.
    async fn verify_evidence(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        port: &dyn Integration,
        mut verification: EvidenceSet<'_>,
    ) -> Result<std::result::Result<Vec<String>, &'static str>> {
        let ids = match verification.references() {
            Ok(ids) => ids,
            Err(reason) => return Ok(Err(reason)),
        };
        for id in &ids {
            let verdict = match EvidenceRef::parse(id) {
                EvidenceRef::Check(op) => {
                    let row =
                        EvidenceSet::receipt(tx, &self.namespace, op, "check.request").await?;
                    verification.check(row)?
                }
                EvidenceRef::Judgment(op) => {
                    let row =
                        EvidenceSet::receipt(tx, &self.namespace, op, "judgment.request").await?;
                    verification.judgment(row)
                }
                EvidenceRef::Work => {
                    let row = verification.work_row(tx, &self.namespace, id).await?;
                    verification.work(row)
                }
                EvidenceRef::Review(decision) => {
                    let row = port
                        .review(
                            self.namespace.workspace_id,
                            self.namespace.project_id,
                            decision,
                        )
                        .await?;
                    verification.review(row)
                }
                EvidenceRef::Unknown => Err("evidence_unknown"),
            };
            if let Err(reason) = verdict {
                return Ok(Err(reason));
            }
        }
        Ok(verification.finish(ids))
    }

    fn integration_receipts(&self) -> IntegrationReceipts<'_> {
        IntegrationReceipts {
            namespace: &self.namespace,
            pool: &self.pool,
        }
    }
    /// operation.get for candidate and publication references.
    pub(super) async fn integration_get(&self, reference: &str) -> Result<Option<HostResult>> {
        self.integration_receipts().operation(reference).await
    }
    /// The retained receipt in this namespace generation.
    pub async fn integration_receipt(&self, reference: &str) -> Result<Option<Value>> {
        self.integration_receipts().inspect(reference).await
    }
}

impl PrepareReply {
    fn settlement(self, output: Value, has_source: bool) -> Result<SettledIntegration> {
        let (outcome, reason, detail) = match self {
            PrepareReply::Prepared { candidate } => {
                ("prepared", None, json!({"candidate": candidate}))
            }
            PrepareReply::Conflict {
                step,
                paths,
                truncated,
                count,
            } => {
                let kept: Vec<_> = paths
                    .iter()
                    .filter(|p| p.len() <= 1024)
                    .take(MAX_CONFLICT_PATHS)
                    .collect();
                let truncated = truncated || kept.len() != paths.len();
                (
                    "conflict",
                    Some("conflicted".to_string()),
                    json!({"step": step, "paths": kept, "truncated": truncated, "count": count}),
                )
            }
            PrepareReply::Refused { reason } => (
                "refused",
                Some(token(&reason, "prepare_failed")),
                Value::Null,
            ),
        };
        // Catalog 2.1.0 (Host8): `prepared` carries the prepared candidate's
        // OID, which policies bind check.request `source` to. 2.0.0 has none.
        let output = match &detail["candidate"] {
            candidate if outcome == "prepared" && has_source => {
                let candidate: ObjectId =
                    serde_json::from_value(candidate.clone()).map_err(|_| Error::Unknown)?;
                merged(&output, json!({"source": catalog_oid(&candidate)}))
            }
            _ => output,
        };
        let result = HostResult {
            outcome: outcome.into(),
            output,
        };
        Ok(SettledIntegration {
            result,
            reason,
            detail,
        })
    }
}
