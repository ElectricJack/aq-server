//! The D1 automatic publish for D3's default completion policy, compiled out
//! with the cargo feature `delivery_playbook` (Epic 0, spec 0016 section 7).
use super::{SourceHead, current_head};
use crate::{
    AppState,
    delivery::{
        CandidateRequest, CandidateState, CommitMetadata, DeliverRequest, DeliveryError,
        DeliveryOutcome, DeliveryService, OperationContext, PublicationMode, PublicationPolicy,
    },
    profiled_execution::{
        results,
        settlement::{
            automatic::{AutoDisposition, AutoSettlement, settle_after_record},
            owner::*,
        },
    },
};
use aq_worker_protocol::{ObjectAlgorithm, ObjectId};
use serde_json::{Value, json};
use std::{collections::HashSet, sync::Mutex};
use uuid::Uuid;

const OWNER: &str = "aq.d1.default-policy-publication@1";
const HELD: &str = "delivery_evidence_missing";
const COMMIT_NAME: &str = "AQ";
const COMMIT_EMAIL: &str = "aq@aq.invalid";

// aq-git 0.2.0 cannot create a pull-request head branch and no GitHub
// credentials are configured (D0/D2): pull-request policies hold explicitly
// before any call, so no GitHub client is bound.
use crate::delivery::UnconfiguredGitHub as NoGitHub;

/// One in-process owner per report: a duplicate result POST never races a
/// running publication into a second evidence write.
static IN_FLIGHT: Mutex<Option<HashSet<Uuid>>> = Mutex::new(None);
struct Flight(Uuid);
impl Flight {
    fn take(report: Uuid) -> Option<Self> {
        let mut set = IN_FLIGHT.lock().ok()?;
        set.get_or_insert_with(HashSet::new)
            .insert(report)
            .then_some(Self(report))
    }
}
impl Drop for Flight {
    fn drop(&mut self) {
        if let Ok(mut set) = IN_FLIGHT.lock()
            && let Some(set) = set.as_mut()
        {
            set.remove(&self.0);
        }
    }
}

/// True when D3 holds this settlement only on missing delivery evidence.
pub fn awaits_delivery(s: &AutoSettlement) -> bool {
    s.disposition == AutoDisposition::Held && s.reason.as_deref() == Some(HELD)
}

/// Publish a held result and let D3 re-evaluate it. Idempotent per report.
/// The caller runs this detached from any client request, so a dropped
/// request can never cancel a publication between dispatch and receipt.
pub async fn deliver_then_settle(
    state: &AppState,
    delivery: &DeliveryService,
    report_id: Uuid,
) -> Result<Option<AutoSettlement>> {
    let Some(_flight) = Flight::take(report_id) else {
        return Ok(None);
    };
    let Some(settled) = settle_after_record(state, report_id).await? else {
        return Ok(None);
    };
    if !awaits_delivery(&settled) {
        return Ok(Some(settled));
    }
    match publish(state, delivery, report_id).await? {
        Step::Held => Ok(Some(settled)),
        // Evidence was recorded, or acceptance no longer holds (for example a
        // cancellation landed): D3 re-evaluates either way.
        Step::Recorded | Step::NotAccepted => settle_after_record(state, report_id).await,
    }
}

fn sha1(hex: &str) -> ObjectId {
    ObjectId {
        algorithm: ObjectAlgorithm::Sha1,
        hex: hex.to_owned(),
    }
}

enum Recorded {
    Published { outcome: Value, head: String },
    Unpublished(Value),
    Indeterminate(Value),
}

enum Step {
    Recorded,
    /// Nothing recorded; the result stays held for a later replay.
    Held,
    /// D3's acceptance no longer holds; nothing was sent to aq-git's publish.
    NotAccepted,
}

/// D3's acceptance, re-checked under the report lock, the D3 settlement lock
/// and the common node/attempt/hot locks that cancellation and abandonment
/// take, so neither can interleave with the fence written under them.
async fn accepted_locked(
    state: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    report_id: Uuid,
) -> Result<Option<results::RecordedResult>> {
    let r = results::load_locked(state, tx, report_id).await?;
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,56))")
        .bind(format!("aq-auto-settlement:{report_id}"))
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    let settlement: Option<(String, Option<String>)> = sqlx::query_as(
        "SELECT disposition,reason FROM profiled_auto_settlements WHERE report_id=$1",
    )
    .bind(report_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?;
    let v = crate::execution::authority::load_locked(
        state,
        tx,
        &r.report.binding.session,
        crate::execution::authority::Policy::IdentityOnly,
    )
    .await?;
    let accepted = settlement == Some(("held".into(), Some(HELD.into())))
        && r.is_current_report_head
        && matches!(
            r.report.outcome,
            aq_admission::transport::ProcessOutcome::Success
        )
        && !v.revoked
        && v.cancellation_id.is_none()
        && v.holds_task
        && v.stopped;
    Ok(accepted.then_some(r))
}

enum Fence {
    Fenced,
    NotAccepted,
    /// Final evidence already exists for this source.
    Recorded,
}

/// Durable publication fence: an `indeterminate` evidence row in phase
/// `publishing`, written only while D3's acceptance holds under the common
/// locks. Cancellation and abandonment refuse to act on an attempt with such a
/// row, and a crash after it leaves the result held (never completed, never
/// reworked) until the publication is reconciled.
async fn fence(
    state: &AppState,
    report_id: Uuid,
    source_op: Uuid,
    evidence: &Value,
) -> Result<Fence> {
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    let Some(r) = accepted_locked(state, &mut tx, report_id).await? else {
        tx.rollback().await.map_err(db)?;
        return Ok(Fence::NotAccepted);
    };
    let attempt = r.report.binding.attempt_id;
    let existing: Option<(String, Option<String>)> = sqlx::query_as("SELECT state,evidence->>'phase' FROM profiled_delivery_evidence WHERE attempt_id=$1 AND source_operation_id=$2 FOR UPDATE")
        .bind(attempt).bind(source_op).fetch_optional(&mut *tx).await.map_err(db)?;
    let fenced = match existing {
        Some((s, Some(phase))) if s == "indeterminate" && phase == "publishing" => Fence::Fenced,
        Some(_) => Fence::Recorded,
        None => {
            sqlx::query("INSERT INTO profiled_delivery_evidence(attempt_id,source_operation_id,state,evidence,recorded_at_ms) VALUES($1,$2,'indeterminate',$3,$4)")
                .bind(attempt).bind(source_op).bind(evidence).bind(now(state)?)
                .execute(&mut *tx).await.map_err(db)?;
            Fence::Fenced
        }
    };
    tx.commit().await.map_err(db)?;
    Ok(fenced)
}

/// Removes this owner's fence when the publish operation provably sent
/// nothing (refused before dispatch), so the result returns to held/missing.
async fn release_fence(state: &AppState, attempt: Uuid, source_op: Uuid) -> Result<()> {
    sqlx::query("DELETE FROM profiled_delivery_evidence WHERE attempt_id=$1 AND source_operation_id=$2 AND state='indeterminate' AND evidence->>'phase'='publishing'")
        .bind(attempt).bind(source_op).execute(&state.store.pool).await.map_err(db)?;
    Ok(())
}

/// Publishes the report's retained source; see [`Step`] for the outcomes.
async fn publish(state: &AppState, delivery: &DeliveryService, report_id: Uuid) -> Result<Step> {
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    let r = results::load_locked(state, &mut tx, report_id).await?;
    let mut source = None;
    for op in &r.report.source_receipt_ids {
        match crate::source_retention::load_confirmed_locked(&mut tx, *op, &r).await {
            Ok(confirmed) => {
                source = Some((*op, confirmed));
                break;
            }
            Err(crate::error::AppError::Conflict(_)) => {}
            Err(e) => return Err(e),
        }
    }
    tx.rollback().await.map_err(db)?;
    let Some((source_op, confirmed)) = source else {
        return Ok(Step::Held);
    };
    let attempt = r.report.binding.attempt_id;
    let admitted = &r.spec.source;
    let (project, repository, generation) = (
        admitted.project_id,
        admitted.repository_id,
        admitted.repository_binding_generation,
    );
    let binding = crate::project_bindings::current(state, project)
        .await?
        .binding;
    let Some(policy) = binding.publication.clone() else {
        tracing::info!(%report_id, %project, "delivery required but the project names no publication policy; held");
        return Ok(Step::Held);
    };
    if !binding.enabled
        || binding.repository_id != repository
        || binding.repository_binding_generation != generation
        || confirmed.manifest.source.repository_id != repository
        || confirmed.manifest.source.binding_generation != generation
    {
        tracing::warn!(%report_id, %project, "publication binding differs from the admitted source; held");
        return Ok(Step::Held);
    }
    if policy.mode == PublicationMode::PullRequest {
        tracing::warn!(%report_id, %project, "pull_request publication is unsupported with aq-git 0.2.0 (D0/D2); held, never degraded");
        return Ok(Step::Held);
    }
    let candidate_op = Uuid::new_v5(&report_id, b"aq.d1.default-policy.candidate");
    let publish_op = Uuid::new_v5(&report_id, b"aq.d1.default-policy.publish");
    let context = |operation_id| OperationContext {
        operation_id,
        repository_id: repository,
        binding_generation: generation,
    };
    // Candidate base and expected_old are one OID: the recorded source head of
    // the target, or before any publication the commit the attempt was
    // admitted on. A retry replays the base frozen in the first intent.
    let base = match delivery
        .frozen_inputs(candidate_op)
        .await
        .map_err(delivery_error)?
    {
        Some(inputs) => {
            serde_json::from_value::<ObjectId>(inputs["base"].clone()).map_err(|_| conflict())?
        }
        None => match current_head(
            &state.store.pool,
            project,
            repository,
            generation,
            &policy.target,
        )
        .await?
        {
            Some(head) => sha1(&head.oid),
            // A handoff successor's admitted base is its predecessor's saved commit; the
            // target still holds the lineage's original base (review C-1).
            None => {
                let mut conn = state.store.pool.acquire().await.map_err(db)?;
                let task = r.spec.task.task_id;
                let base = aq_machine_control::lineage_base(
                    &mut conn,
                    task,
                    &admitted.resolved_object.hex,
                );
                let Some(base) = base.await? else {
                    tracing::warn!(%report_id, "lineage history past its bound; held");
                    return Ok(Step::Held);
                };
                sha1(&base)
            }
        },
    };
    // Candidate creation has no remote effect; it may run before the fence.
    let created = delivery
        .create_candidate(CandidateRequest {
            context: context(candidate_op),
            base: base.clone(),
            source_receipts: vec![source_op],
            commit: CommitMetadata {
                name: COMMIT_NAME.into(),
                email: COMMIT_EMAIL.into(),
                message: format!(
                    "AQ: task {} result {}\n\nRetained checkpoint {} published by {}.",
                    r.spec.task.task_id, report_id, confirmed.manifest.checkpoint.hex, OWNER
                ),
            },
        })
        .await;
    let candidate = match created {
        Ok(receipt) => match receipt.outcome {
            Ok(c) if c.state == CandidateState::Ready => match c.candidate {
                Some(candidate) => Ok(candidate),
                None => return Err(conflict()),
            },
            Ok(c) => Err(
                json!({"reason":"conflicted","conflict_step":c.conflict_step,"conflict_path_count":c.conflict_path_count}),
            ),
            Err(e) => {
                tracing::warn!(%report_id, code = %e.code, "candidate creation failed; held");
                return Ok(Step::Held);
            }
        },
        Err(e) => {
            tracing::warn!(%report_id, error = %e, "candidate creation unavailable; held");
            return Ok(Step::Held);
        }
    };
    let evidence = |phase: Option<&str>, result: Option<&Value>, head: Option<&SourceHead>| {
        json!({
            "owner": OWNER,
            "phase": phase,
            "report_id": report_id,
            "policy": policy,
            "candidate_operation_id": candidate_op,
            "candidate": candidate.as_ref().ok(),
            "publication_operation_id": publish_op,
            "expected_old": base,
            "result": result,
            "source_head": head,
        })
    };
    // Acceptance is re-checked under the common locks immediately before any
    // publish request, and the fence then keeps cancellation and abandonment
    // from interleaving with the publication.
    match fence(
        state,
        report_id,
        source_op,
        &evidence(Some("publishing"), None, None),
    )
    .await?
    {
        Fence::Fenced => {}
        Fence::Recorded => return Ok(Step::Recorded),
        Fence::NotAccepted => {
            tracing::info!(%report_id, "acceptance no longer holds (cancelled, revoked, superseded or released); not published");
            return Ok(Step::NotAccepted);
        }
    }
    let candidate_oid = match &candidate {
        Ok(oid) => oid.clone(),
        Err(conflicted) => {
            return record(
                state,
                &r,
                source_op,
                &policy,
                &base,
                candidate_op,
                publish_op,
                &evidence,
                Recorded::Unpublished(conflicted.clone()),
            )
            .await;
        }
    };
    let outcome = match delivery
        .deliver(
            &policy,
            DeliverRequest {
                context: context(publish_op),
                candidate_id: candidate_op,
                expected_old: base.clone(),
            },
            &NoGitHub,
        )
        .await
    {
        Ok(o) => o,
        // A reply that fails validation after dispatch, or a lost receipt
        // write, proves nothing about the remote: the fence stays.
        Err(
            e @ (DeliveryError::Protocol
            | DeliveryError::Database(_)
            | DeliveryError::Json(_)
            | DeliveryError::Transport
            | DeliveryError::Indeterminate),
        ) => {
            let reason = json!({"reason":"publish_reply_unknown","error":e.to_string(),"operation_id":publish_op,"next":"operation.reconcile under a new operation ID; never a re-push"});
            return record(
                state,
                &r,
                source_op,
                &policy,
                &base,
                candidate_op,
                publish_op,
                &evidence,
                Recorded::Indeterminate(reason),
            )
            .await;
        }
        Err(e) => {
            // Refused before any intent or dispatch: release the fence.
            tracing::warn!(%report_id, error = %e, "publication refused before dispatch; held");
            release_fence(state, attempt, source_op).await?;
            return Ok(Step::Held);
        }
    };
    let recorded = match outcome {
        DeliveryOutcome::Published {
            old,
            new,
            acknowledged_at,
            ..
        } => Recorded::Published {
            outcome: json!({"outcome":"applied","old":old,"new":new,"acknowledged_at":acknowledged_at}),
            head: new.hex,
        },
        // Observed presence of exactly the candidate. A target that merely
        // contains it is a different head AQ cannot follow: held.
        DeliveryOutcome::ObservedPresent {
            observed_target,
            observed_at,
            ..
        } if observed_target == candidate_oid => Recorded::Published {
            outcome: json!({"outcome":"already_present","observed_target":observed_target,"observed_at":observed_at,"caused_by_this_operation":false}),
            head: candidate_oid.hex.clone(),
        },
        DeliveryOutcome::ObservedPresent {
            observed_target,
            observed_at,
            ..
        } => Recorded::Indeterminate(
            json!({"reason":"observed_target_differs","outcome":"already_present","observed_target":observed_target,"observed_at":observed_at,"caused_by_this_operation":false}),
        ),
        DeliveryOutcome::Refused(refusal) => {
            Recorded::Unpublished(serde_json::to_value(refusal).map_err(|_| conflict())?)
        }
        DeliveryOutcome::Ambiguous { operation_id } => Recorded::Indeterminate(
            json!({"reason":"operation_indeterminate","operation_id":operation_id,"next":"operation.reconcile under a new operation ID; never a re-push"}),
        ),
        DeliveryOutcome::DestinationBlocked => {
            tracing::info!(%report_id, "publication destination is held by another operation; held");
            release_fence(state, attempt, source_op).await?;
            return Ok(Step::Held);
        }
        DeliveryOutcome::RecoveryBlocked(blocked) => {
            tracing::warn!(%report_id, message = %blocked.message, "aq-git recovery_blocked; held");
            release_fence(state, attempt, source_op).await?;
            return Ok(Step::Held);
        }
        DeliveryOutcome::Failed { error, .. } => {
            tracing::warn!(%report_id, code = %error.code, "publication failed before dispatch; held");
            release_fence(state, attempt, source_op).await?;
            return Ok(Step::Held);
        }
    };
    record(
        state,
        &r,
        source_op,
        &policy,
        &base,
        candidate_op,
        publish_op,
        &evidence,
        recorded,
    )
    .await
}

fn delivery_error(_: DeliveryError) -> crate::error::AppError {
    unavailable()
}

type Evidence<'a> =
    dyn Fn(Option<&str>, Option<&Value>, Option<&SourceHead>) -> Value + Send + Sync + 'a;

/// Replaces this owner's fence with the final evidence. A published head
/// advances by compare-and-swap from the exact expected_old; if the recorded
/// head moved meanwhile, the publication is recorded indeterminate (held).
#[allow(clippy::too_many_arguments)]
async fn record(
    state: &AppState,
    r: &results::RecordedResult,
    source_op: Uuid,
    policy: &PublicationPolicy,
    base: &ObjectId,
    candidate_op: Uuid,
    publish_op: Uuid,
    evidence: &Evidence<'_>,
    recorded: Recorded,
) -> Result<Step> {
    let admitted = &r.spec.source;
    let time = now(state)?;
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    // Serializes head advancement per repository target.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1,57))")
        .bind(format!(
            "aq-d1-source-head:{}:{}",
            admitted.repository_id, policy.target
        ))
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    let (state_name, outcome, head) = match recorded {
        Recorded::Published { outcome, head } => {
            let current = current_head(
                &mut *tx,
                admitted.project_id,
                admitted.repository_id,
                admitted.repository_binding_generation,
                &policy.target,
            )
            .await?;
            if current.as_ref().is_none_or(|h| h.oid == base.hex) {
                let head = SourceHead {
                    schema_version: 1,
                    project_id: admitted.project_id,
                    repository_id: admitted.repository_id,
                    binding_generation: admitted.repository_binding_generation,
                    target: policy.target.clone(),
                    sequence: current.map_or(1, |h| h.sequence + 1),
                    oid: head,
                    retention_handle: candidate_op,
                    candidate_operation_id: candidate_op,
                    publication_operation_id: publish_op,
                    previous: base.hex.clone(),
                };
                if !head.validate() {
                    return Err(conflict());
                }
                ("published", outcome, Some(head))
            } else {
                tracing::warn!(report_id = %r.report.report_id, "source head moved since this publication's expected_old; recorded indeterminate");
                (
                    "indeterminate",
                    json!({"reason":"source_head_moved","publication":outcome}),
                    None,
                )
            }
        }
        Recorded::Unpublished(v) => ("unpublished", v, None),
        Recorded::Indeterminate(v) => ("indeterminate", v, None),
    };
    let evidence = evidence(None, Some(&outcome), head.as_ref());
    let written = sqlx::query("INSERT INTO profiled_delivery_evidence(attempt_id,source_operation_id,state,evidence,recorded_at_ms) VALUES($1,$2,$3,$4,$5) ON CONFLICT(attempt_id,source_operation_id) DO UPDATE SET state=EXCLUDED.state,evidence=EXCLUDED.evidence,recorded_at_ms=EXCLUDED.recorded_at_ms WHERE profiled_delivery_evidence.state='indeterminate' AND profiled_delivery_evidence.evidence->>'phase'='publishing'")
        .bind(r.report.binding.attempt_id).bind(source_op).bind(state_name).bind(&evidence).bind(time)
        .execute(&mut *tx).await.map_err(db)?.rows_affected();
    tx.commit().await.map_err(db)?;
    if written == 1 {
        tracing::info!(report_id = %r.report.report_id, attempt = %r.report.binding.attempt_id, state = state_name, "delivery evidence recorded");
    }
    Ok(Step::Recorded)
}
