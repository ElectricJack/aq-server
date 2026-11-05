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
use aq_worker_protocol::{ObjectAlgorithm, ObjectId};
use serde::{Deserialize, Serialize};

const CANDIDATE_REF: &str = "aq.candidate/";
const PUBLICATION_REF: &str = "aq.publication/";
const SOURCE_REF: &str = "aq.source/";
const CHECK_EVIDENCE: &str = "check-receipt:";
const JUDGMENT_EVIDENCE: &str = "judgment:";
const REVIEW_EVIDENCE: &str = "aq.review.decision/";
const MAX_SOURCES: usize = 64;
const MAX_EVIDENCE: usize = 64;
const MAX_CONFLICT_PATHS: usize = 32;
const MAX_RECONCILE: i32 = 64;

/// Evidence a project's integration policy requires before publication.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidencePolicy {
    /// Check ids that each need a passing D6 check receipt.
    pub checks: Vec<String>,
    /// An approving review decision from an authorized issuer.
    pub review: bool,
    /// A D6 judgment assessed with one of these choices; empty = none needed.
    pub judgment_choices: Vec<String>,
}
impl EvidencePolicy {
    fn names_nothing(&self) -> bool {
        self.checks.is_empty() && !self.review && self.judgment_choices.is_empty()
    }
}

/// The project's revisioned repository binding and policies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntegrationBinding {
    pub repository_id: Uuid,
    pub binding_generation: u64,
    pub publication: PublicationPolicy,
    pub evidence: EvidencePolicy,
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

/// A review decision as its owning service reports it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReviewEvidence {
    pub subject: Value,
    pub approved: bool,
    /// The decision's issuer holds review authority for this project.
    pub issuer_authorized: bool,
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
fn handle(id: &str, prefix: &str) -> Option<Uuid> {
    Uuid::parse_str(id.strip_prefix(prefix)?).ok()
}
/// A bounded typed reason from the port; anything else maps to `fallback`.
fn token(reason: &str, fallback: &str) -> String {
    if (1..=64).contains(&reason.len())
        && reason
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
    {
        reason.into()
    } else {
        fallback.into()
    }
}
fn same_subject(a: &Value, id: &str, revision: i64) -> bool {
    a["id"].as_str() == Some(id) && a["revision"].as_i64() == Some(revision)
}
fn merged(base: &Value, extra: Value) -> Value {
    let mut out = base.as_object().cloned().unwrap_or_default();
    if let Value::Object(extra) = extra {
        out.extend(extra);
    }
    Value::Object(out)
}

/// What a receipt row records, besides the intent identity.
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
        self.seal_event(
            tx,
            "aq.integration.changed",
            event_id,
            self.operation(intent)?,
            &intent.id,
            &output["subject"],
            json!({"outcome": result.outcome, "reference": output["reference"], "subject": output["subject"]}),
        )
        .await
    }

    /// Single-phase refusal: final effect row, refused receipt and event.
    #[allow(clippy::too_many_arguments)]
    async fn refuse_fresh(
        &self,
        mut tx: Transaction<'_, Postgres>,
        id: Uuid,
        revision: i64,
        intent: &Intent,
        identity: &str,
        candidate_operation: Uuid,
        binding: Option<&IntegrationBinding>,
        reason: &str,
        output: &Value,
    ) -> Result<HostResult> {
        let result = HostResult {
            outcome: "refused".into(),
            output: output.clone(),
        };
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
        result: HostResult,
        reason: Option<&str>,
        detail: Value,
    ) -> Result<HostResult> {
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

    // --------------------------------------------------------- candidate.prepare

    pub(super) async fn candidate_prepare(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<HostResult> {
        let ports = self.ports()?;
        let (mut tx, stage, identity) = self.begin(Some(owner), id, revision, intent).await?;
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
                let refuse = |tx, binding, reason| {
                    self.refuse_fresh(
                        tx, id, revision, intent, &identity, operation, binding, reason, &output,
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
                let mut sources = Some(vec![]);
                for item in input["sources"].as_array().into_iter().flatten() {
                    let receipt = self.source_receipt(&mut tx, ports, item).await?;
                    match (receipt, sources.as_mut()) {
                        (Some(r), Some(list)) => list.push(r),
                        _ => sources = None,
                    }
                }
                let (Some(base), Some(sources)) = (base, sources) else {
                    return refuse(tx, Some(&binding), "invalid_request").await;
                };
                let unique: std::collections::BTreeSet<_> = sources.iter().collect();
                if sources.is_empty()
                    || sources.len() > MAX_SOURCES
                    || unique.len() != sources.len()
                {
                    return refuse(tx, Some(&binding), "invalid_request").await;
                }
                let call = PrepareCall {
                    operation,
                    repository_id: binding.repository_id,
                    binding_generation: binding.binding_generation,
                    base,
                    sources,
                };
                let plan = serde_json::to_value(&call)?;
                self.record(&mut tx, id, revision, intent, &identity, PENDING, &plan)
                    .await?;
                self.insert_receipt(
                    &mut tx,
                    intent,
                    Receipt {
                        operation,
                        candidate_operation: operation,
                        binding: Some(&binding),
                        state: PENDING,
                        reason: None,
                        request: merged(&output, json!({"call": plan})),
                        detail: Value::Null,
                    },
                )
                .await?;
                self.commit_fresh(tx, id, intent).await?;
                call
            }
        };
        let port = ports.integration.as_ref().ok_or(Error::Unknown)?;
        // A failed call leaves the intent pending; re-dispatch re-sends it.
        let reply = port.prepare(&call).await.map_err(|_| Error::Unknown)?;
        let (outcome, reason, detail) = match reply {
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
            candidate if outcome == "prepared" && self.profile == CatalogProfile::Host8 => {
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
        self.settle_integration(intent, &identity, result, reason.as_deref(), detail)
            .await
    }

    // --------------------------------------------------------- candidate.publish

    pub(super) async fn candidate_publish(
        &self,
        owner: &RuntimeOwner,
        id: Uuid,
        revision: i64,
        intent: &Intent,
    ) -> Result<HostResult> {
        let ports = self.ports()?;
        let (mut tx, stage, identity) = self.begin(Some(owner), id, revision, intent).await?;
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
                let state: String = sqlx::query_scalar(
                    "SELECT state FROM playbook_integration_receipts WHERE operation_id=$1",
                )
                .bind(operation)
                .fetch_one(&mut *tx)
                .await?;
                tx.commit().await?;
                let call: PublishCall = serde_json::from_value(plan).map_err(|_| Error::Unknown)?;
                let port = ports.integration.as_ref().ok_or(Error::Unknown)?;
                return if state == "ambiguous" {
                    self.publish_reconcile(intent, &identity, port.as_ref(), &call, output)
                        .await
                } else {
                    // Never acknowledged either way: the same operation id is
                    // re-sent and D1's dispatch fence never pushes it twice.
                    self.publish_send(intent, &identity, port.as_ref(), &call, output)
                        .await
                };
            }
            Stage::Fresh => {}
        }
        let candidate_id = text(&input["candidate"], "id")?;
        let candidate_op = handle(candidate_id, CANDIDATE_REF);
        let refuse = |tx, binding, reason| {
            self.refuse_fresh(
                tx,
                id,
                revision,
                intent,
                &identity,
                candidate_op.unwrap_or(Uuid::nil()),
                binding,
                reason,
                &output,
            )
        };
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
        let Some(candidate) = sqlx::query("SELECT state,repository_id,binding_generation,detail FROM playbook_integration_receipts WHERE operation_id=$1 AND namespace_id=$2 AND generation=$3 AND command_name='candidate.prepare'")
            .bind(candidate_op)
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .fetch_optional(&mut *tx)
            .await?
        else {
            return refuse(tx, Some(&binding), "candidate_unknown").await;
        };
        if input["candidate"]["revision"].as_i64() != Some(1) {
            return refuse(tx, Some(&binding), "candidate_stale").await;
        }
        if candidate.get::<String, _>("state") != "prepared" {
            return refuse(tx, Some(&binding), "candidate_not_ready").await;
        }
        if candidate.get::<Option<Uuid>, _>("repository_id") != Some(binding.repository_id)
            || candidate.get::<Option<i64>, _>("binding_generation")
                != Some(binding.binding_generation as i64)
        {
            return refuse(tx, Some(&binding), "binding_changed").await;
        }
        let detail: Option<Value> = candidate.get("detail");
        let candidate_oid: ObjectId =
            serde_json::from_value(detail.ok_or(Error::Unknown)?["candidate"].clone())
                .map_err(|_| Error::Unknown)?;
        let Some((target, expected_old)) =
            text(&input["destination"], "id").ok().and_then(destination)
        else {
            return refuse(tx, Some(&binding), "invalid_request").await;
        };
        if target != binding.publication.target {
            return refuse(tx, Some(&binding), "destination_mismatch").await;
        }
        let evidence = match self
            .verify_evidence(
                &mut tx,
                port.as_ref(),
                &binding.evidence,
                &input["evidence"],
                candidate_id,
                &candidate_oid,
            )
            .await?
        {
            Ok(evidence) => evidence,
            Err(reason) => return refuse(tx, Some(&binding), reason).await,
        };
        let call = PublishCall {
            operation,
            repository_id: binding.repository_id,
            binding_generation: binding.binding_generation,
            candidate_operation: candidate_op,
            candidate: candidate_oid,
            publication: binding.publication.clone(),
            expected_old,
        };
        let plan = serde_json::to_value(&call)?;
        self.record(&mut tx, id, revision, intent, &identity, PENDING, &plan)
            .await?;
        self.insert_receipt(
            &mut tx,
            intent,
            Receipt {
                operation,
                candidate_operation: candidate_op,
                binding: Some(&binding),
                state: PENDING,
                reason: None,
                request: merged(&output, json!({"call": plan, "evidence": evidence})),
                detail: Value::Null,
            },
        )
        .await?;
        self.commit_fresh(tx, id, intent).await?;
        self.publish_send(intent, &identity, port.as_ref(), &call, output)
            .await
    }

    async fn publish_send(
        &self,
        intent: &Intent,
        identity: &str,
        port: &dyn Integration,
        call: &PublishCall,
        output: Value,
    ) -> Result<HostResult> {
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
        self.settle_integration(intent, identity, result, reason.as_deref(), detail)
            .await
    }

    /// Settles an ambiguous publication by asking D1 what happened, under a
    /// fresh operation per attempt. Never publishes again.
    async fn publish_reconcile(
        &self,
        intent: &Intent,
        identity: &str,
        port: &dyn Integration,
        call: &PublishCall,
        output: Value,
    ) -> Result<HostResult> {
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
        self.settle_integration(intent, identity, result, reason.as_deref(), detail)
            .await
    }

    /// Resolves every evidence item in its owning store. Returns the verified
    /// references in input order, or the typed refusal reason. Coverage of
    /// the policy is checked last so a bad item reports its own reason.
    async fn verify_evidence(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        port: &dyn Integration,
        policy: &EvidencePolicy,
        evidence: &Value,
        candidate: &str,
        candidate_oid: &ObjectId,
    ) -> Result<std::result::Result<Vec<String>, &'static str>> {
        let ids: Option<Vec<&str>> = evidence
            .as_array()
            .and_then(|items| items.iter().map(|e| e["id"].as_str()).collect());
        let Some(ids) = ids else {
            return Ok(Err("invalid_request"));
        };
        let unique: std::collections::BTreeSet<_> = ids.iter().collect();
        if ids.len() > MAX_EVIDENCE || unique.len() != ids.len() {
            return Ok(Err("invalid_request"));
        }
        let source = catalog_oid(candidate_oid);
        let (workspace, project) = (self.namespace.workspace_id, self.namespace.project_id);
        let mut passed_checks = std::collections::BTreeSet::new();
        let (mut reviewed, mut judged) = (false, false);
        for id in &ids {
            if let Some(op) = handle(id, CHECK_EVIDENCE) {
                let Some(row) = self.evidence_row(tx, op, "check.request").await? else {
                    return Ok(Err("evidence_unknown"));
                };
                let (status, request, check_id): (String, Value, Option<String>) =
                    (row.get("status"), row.get("request"), row.get("check_id"));
                if status == "running" {
                    return Ok(Err("evidence_pending"));
                }
                if request["execution_profile"].as_str() != Some(CHECK_EXECUTION_PROFILE) {
                    return Ok(Err("evidence_unauthorized"));
                }
                if !same_subject(&request["subject"], candidate, 1)
                    || request["source"]["format"] != source["format"]
                    || request["source"]["hex"] != source["hex"]
                {
                    return Ok(Err("evidence_stale"));
                }
                if status != "passed" {
                    return Ok(Err("evidence_failed"));
                }
                passed_checks.insert(check_id.ok_or(Error::Unknown)?);
            } else if let Some(op) = handle(id, JUDGMENT_EVIDENCE) {
                let Some(row) = self.evidence_row(tx, op, "judgment.request").await? else {
                    return Ok(Err("evidence_unknown"));
                };
                if policy.judgment_choices.is_empty() {
                    return Ok(Err("evidence_unauthorized"));
                }
                let (status, request, output): (String, Value, Option<Value>) =
                    (row.get("status"), row.get("request"), row.get("output"));
                if status == "running" {
                    return Ok(Err("evidence_pending"));
                }
                if !same_subject(&request["subject"], candidate, 1) {
                    return Ok(Err("evidence_stale"));
                }
                let output = output.unwrap_or(Value::Null);
                let profile = request["credential_profile"].as_str().unwrap_or("");
                if status == "assessed"
                    && (profile.is_empty()
                        || output["provenance"]["credential_profile"].as_str() != Some(profile))
                {
                    return Ok(Err("evidence_unauthorized"));
                }
                let choice = output["choice"].as_str().unwrap_or("");
                if status != "assessed" || !policy.judgment_choices.iter().any(|c| c == choice) {
                    return Ok(Err("evidence_failed"));
                }
                judged = true;
            } else if id.starts_with(WORK_REF) {
                // D11: Host7 review work. Its disposition came from the review
                // recorded on the task (`aq:review_disposition`), never inferred.
                let row = sqlx::query("SELECT status,request,output FROM playbook_host_operations WHERE namespace_id=$1 AND generation=$2 AND command_name='work.ensure' AND request->>'reference'=$3")
                    .bind(self.namespace.namespace_id)
                    .bind(self.namespace.generation)
                    .bind(id)
                    .fetch_optional(&mut **tx)
                    .await?;
                let Some(row) = row else {
                    return Ok(Err("evidence_unknown"));
                };
                let (status, request, output): (String, Value, Option<Value>) =
                    (row.get("status"), row.get("request"), row.get("output"));
                if !matches!(request["kind"].as_str(), Some("review" | "fidelity_review")) {
                    return Ok(Err("evidence_unauthorized"));
                }
                if status == "running" {
                    return Ok(Err("evidence_pending"));
                }
                if !same_subject(&request["motivation"], candidate, 1) {
                    return Ok(Err("evidence_stale"));
                }
                if status != "completed"
                    || output.as_ref().map(|o| &o["disposition"]) != Some(&json!("accepted"))
                {
                    return Ok(Err("evidence_failed"));
                }
                reviewed = true;
            } else if let Some(decision) = handle(id, REVIEW_EVIDENCE) {
                let Some(review) = port.review(workspace, project, decision).await? else {
                    return Ok(Err("evidence_unknown"));
                };
                if !review.issuer_authorized {
                    return Ok(Err("evidence_unauthorized"));
                }
                if !same_subject(&review.subject, candidate, 1) {
                    return Ok(Err("evidence_stale"));
                }
                if !review.approved {
                    return Ok(Err("evidence_failed"));
                }
                reviewed = true;
            } else {
                // Caller-asserted evidence ("approved", "check:passed", ...)
                // has no owning store.
                return Ok(Err("evidence_unknown"));
            }
        }
        let covered = policy.checks.iter().all(|c| passed_checks.contains(c))
            && (!policy.review || reviewed)
            && (policy.judgment_choices.is_empty() || judged);
        if !covered {
            return Ok(Err("evidence_missing"));
        }
        Ok(Ok(ids.iter().map(|s| s.to_string()).collect()))
    }

    async fn evidence_row(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        operation: Uuid,
        command: &str,
    ) -> Result<Option<sqlx::postgres::PgRow>> {
        Ok(sqlx::query("SELECT status,request,output,check_id FROM playbook_host_operations WHERE operation_id=$1 AND namespace_id=$2 AND generation=$3 AND command_name=$4")
            .bind(operation)
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(command)
            .fetch_optional(&mut **tx)
            .await?)
    }

    async fn receipt_row(&self, reference: &str) -> Result<Option<sqlx::postgres::PgRow>> {
        let (command, operation) = if let Some(op) = handle(reference, CANDIDATE_REF) {
            ("candidate.prepare", op)
        } else if let Some(op) = handle(reference, PUBLICATION_REF) {
            ("candidate.publish", op)
        } else {
            return Ok(None);
        };
        Ok(sqlx::query("SELECT operation_id,command_name,state,reason,candidate_operation_id,repository_id,binding_generation,request,detail,reconcile_attempts FROM playbook_integration_receipts WHERE operation_id=$1 AND namespace_id=$2 AND generation=$3 AND command_name=$4")
            .bind(operation)
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(command)
            .fetch_optional(&self.pool)
            .await?)
    }

    /// operation.get for candidate and publication references; `None` for
    /// any other handle.
    pub(super) async fn integration_get(&self, reference: &str) -> Result<Option<HostResult>> {
        let Some(row) = self.receipt_row(reference).await? else {
            return Ok(None);
        };
        let request: Value = row.get("request");
        let pointer = json!({"reference": request["reference"], "subject": request["subject"]});
        let state: String = row.get("state");
        let completed = |outcome: &str, disposition: &str| HostResult {
            outcome: "completed".into(),
            output: json!({"outcome": outcome, "output": merged(&pointer, json!({"disposition": disposition}))}),
        };
        Ok(Some(match state.as_str() {
            PENDING => HostResult {
                outcome: "running".into(),
                output: pointer,
            },
            "ambiguous" => HostResult {
                outcome: "indeterminate".into(),
                output: pointer,
            },
            "prepared" | "published" => completed("completed", "accepted"),
            "conflict" => completed("completed", "failed"),
            _ => completed("refused", "failed"),
        }))
    }

    /// Inspection: the typed receipt behind a candidate or publication
    /// reference in this namespace generation.
    pub async fn integration_receipt(&self, reference: &str) -> Result<Option<Value>> {
        let Some(row) = self.receipt_row(reference).await? else {
            return Ok(None);
        };
        Ok(Some(json!({
            "operation": row.get::<Uuid, _>("operation_id"),
            "command": row.get::<String, _>("command_name"),
            "state": row.get::<String, _>("state"),
            "reason": row.get::<Option<String>, _>("reason"),
            "candidate_operation": row.get::<Uuid, _>("candidate_operation_id"),
            "repository_id": row.get::<Option<Uuid>, _>("repository_id"),
            "binding_generation": row.get::<Option<i64>, _>("binding_generation"),
            "request": row.get::<Value, _>("request"),
            "detail": row.get::<Option<Value>, _>("detail"),
            "reconcile_attempts": row.get::<i32, _>("reconcile_attempts"),
        })))
    }
}
