//! Source adapters: read the owner's committed row inside the wait transaction
//! and project it into a `CommittedObservation`. A missing or mismatched owner
//! row is a retryable unavailability (freshness stays visible), never a result.
use super::{Result, artifact, text};
use crate::{AppState, agent_access::messaging::Recipient};
use aq_wait_engine::{CommittedObservation, SourceProtocol, SourceReply, SourceRequest, wire};
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

mod decision;
mod message;

struct Observed {
    state: wire::MemberState,
    result: Option<wire::ArtifactRef>,
    revision: String,
    snapshot: wire::ArtifactRef,
}
fn unavailable(request: &SourceRequest, reason: &str) -> SourceReply {
    SourceReply::RetryableUnavailable {
        member_id: request.member.member_id,
        reason: text(reason),
    }
}

pub(super) async fn observe(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    request: &SourceRequest,
    now: u64,
) -> Result<SourceReply> {
    let observed = match &request.member.condition {
        wire::Condition::HumanDecision {
            decision_id,
            subject_id,
            subject_revision,
            question_digest,
        } => {
            decision::Decision {
                scope: &request.scope,
                id: *decision_id,
                subject: *subject_id,
                revision: subject_revision,
                question: question_digest,
            }
            .read(tx, now)
            .await?
        }
        wire::Condition::Task {
            task_id,
            requirement,
            ..
        }
        | wire::Condition::Dependency {
            depends_on_task_id: task_id,
            requirement,
            ..
        } => task(state, request, *task_id, requirement).await,
        wire::Condition::Message {
            thread_id,
            exclusive_cursor,
            sender,
        } => {
            message::Thread {
                id: *thread_id,
                after: *exclusive_cursor,
                sender,
            }
            .read(tx, request)
            .await?
        }
        _ => None,
    };
    let Some(observed) = observed else {
        return Ok(unavailable(request, "source owner row unavailable"));
    };
    committed(request, observed)
}

fn committed(request: &SourceRequest, observed: Observed) -> Result<SourceReply> {
    let owner = request.member.condition.owner();
    let condition_digest = wire::canonical_digest(&request.member.condition)?;
    let observation = wire::MemberOutcome {
        member_id: request.member.member_id,
        state: observed.state,
        evidence: observed.result.map(|result| wire::SourceEvidence {
            owner: owner.clone(),
            condition_digest: condition_digest.clone(),
            revision: text(observed.revision.clone()),
            result,
        }),
    };
    let subscription = request
        .subscription
        .digest()
        .map_err(|_| super::WaitError("internal"))?;
    Ok(SourceReply::Committed(Box::new(CommittedObservation {
        protocol: SourceProtocol::CommittedReplay,
        subscription: request.subscription.clone(),
        scope: request.scope.clone(),
        binding: request.binding.clone(),
        principal: request.principal.clone(),
        source_access: request.source_access.clone(),
        incarnation: text("1"),
        boundary: wire::SourceBoundary {
            member_id: request.member.member_id,
            owner,
            condition_digest,
            committed_revision: text(observed.revision.clone()),
            committed_replay_cursor: text(observed.revision),
            subscription_receipt: artifact("wait-subscription", subscription),
            snapshot_digest: observed.snapshot.digest.clone(),
        },
        owner_snapshot: observed.snapshot,
        projection_digest: wire::canonical_digest(&observation)?,
        replay_from: request
            .previous
            .as_ref()
            .map(|b| b.committed_replay_cursor.clone()),
        observation,
    })))
}
fn snapshot(id: String, value: &Value) -> Result<wire::ArtifactRef> {
    Ok(artifact(id, value_digest(value)?))
}
fn value_digest(value: &Value) -> Result<wire::Digest> {
    Ok(wire::Digest::of(&serde_json::to_vec(value)?))
}

async fn task(
    state: &AppState,
    request: &SourceRequest,
    task_id: Uuid,
    requirement: &wire::TaskRequirement,
) -> Option<Observed> {
    let rt = super::super::runtime(state).ok()?;
    let node = super::super::settlement_node(rt, request.scope.workspace_id, task_id)
        .await
        .ok()??;
    let task_state = node["task_state"].as_str()?.to_owned();
    let version = node["version"].to_string();
    let state = match (task_state.as_str(), requirement) {
        ("completed", _) | ("canceled", wire::TaskRequirement::Settled) => {
            wire::MemberState::Satisfied
        }
        ("canceled", wire::TaskRequirement::Successful) => wire::MemberState::SourceFailed,
        _ => wire::MemberState::Pending,
    };
    let snapshot = snapshot(
        format!("graph-node-{task_id}"),
        &json!({"id":task_id,"version":version,"task_state":task_state}),
    )
    .ok()?;
    Some(Observed {
        result: (state != wire::MemberState::Pending).then(|| snapshot.clone()),
        revision: version,
        snapshot,
        state,
    })
}
