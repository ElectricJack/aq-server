//! Source adapters: read the owner's committed row inside the wait transaction
//! and project it into a `CommittedObservation`. A missing or mismatched owner
//! row is a retryable unavailability (freshness stays visible), never a result.
use super::{Result, artifact, text};
use crate::{AppState, agent_access::messaging::Recipient};
use aq_wait_engine::{CommittedObservation, SourceProtocol, SourceReply, SourceRequest, wire};
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

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
            decision(
                tx,
                request,
                now,
                *decision_id,
                *subject_id,
                subject_revision,
                question_digest,
            )
            .await?
        }
        wire::Condition::Task {
            task_id,
            requirement,
            ..
        } => task(state, request, *task_id, requirement).await,
        wire::Condition::Dependency {
            depends_on_task_id,
            requirement,
            ..
        } => task(state, request, *depends_on_task_id, requirement).await,
        wire::Condition::Message {
            thread_id,
            exclusive_cursor,
            sender,
        } => message(tx, request, *thread_id, *exclusive_cursor, sender).await?,
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

async fn decision(
    tx: &mut Transaction<'_, Postgres>,
    request: &SourceRequest,
    now: u64,
    decision_id: Uuid,
    subject_id: Uuid,
    subject_revision: &wire::Text,
    question: &wire::Digest,
) -> Result<Option<Observed>> {
    let Some(row) = sqlx::query("SELECT d.subject_digest,d.request_digest,d.decision_revision,d.state,d.expires_at_ms,s.report_id,s.project_id,s.workspace_id,f.receipt_digest FROM human_decision_requests d JOIN review_subjects s USING(subject_digest) LEFT JOIN human_decision_finality f USING(decision_id) WHERE d.decision_id=$1")
        .bind(decision_id)
        .fetch_optional(&mut **tx)
        .await?
    else {
        return question_decision(tx, request, now, decision_id, subject_id, subject_revision, question)
            .await;
    };
    let subject: String = row.get("subject_digest");
    let asked: String = row.get("request_digest");
    if row.get::<Uuid, _>("report_id") != subject_id
        || subject != subject_revision.as_str()
        || asked != question.as_str()
        || row.get::<Uuid, _>("project_id") != request.scope.project_id
        || row.get::<Uuid, _>("workspace_id") != request.scope.workspace_id
    {
        return Ok(None);
    }
    let revision: i64 = row.get("decision_revision");
    let decision_state: String = row.get("state");
    let receipt: Option<String> = row.get("receipt_digest");
    let expired = row.get::<i64, _>("expires_at_ms") as u64 <= now;
    let state = match (decision_state.as_str(), &receipt) {
        ("answered", Some(_)) => wire::MemberState::Satisfied,
        ("answered", None) => return Ok(None),
        ("pending", _) if !expired => wire::MemberState::Pending,
        _ => wire::MemberState::SourceFailed,
    };
    let result = match state {
        wire::MemberState::Pending => None,
        _ => Some(artifact(
            format!("human-decision-receipt-{decision_id}"),
            match &receipt {
                Some(digest) => serde_json::from_value(json!(digest))?,
                None => value_digest(&json!({"decision_id":decision_id,"state":decision_state}))?,
            },
        )),
    };
    Ok(Some(Observed {
        snapshot: snapshot(
            format!("human-decision-{decision_id}"),
            &json!({"decision_id":decision_id,"revision":revision,"state":decision_state,"receipt":receipt}),
        )?,
        revision: revision.to_string(),
        state,
        result,
    }))
}
/// F5 agent questions: a typed `question` decision owned by `question_decisions`.
/// The subject is the asking task; the subject revision names the asking
/// attempt generation. Same state mapping as code-review decisions.
async fn question_decision(
    tx: &mut Transaction<'_, Postgres>,
    request: &SourceRequest,
    now: u64,
    decision_id: Uuid,
    subject_id: Uuid,
    subject_revision: &wire::Text,
    question: &wire::Digest,
) -> Result<Option<Observed>> {
    let Some(row) = sqlx::query("SELECT task_id,project_id,workspace_id,subject_revision,question_digest,decision_revision,state,expires_at_ms,receipt_digest FROM question_decisions WHERE decision_id=$1")
        .bind(decision_id)
        .fetch_optional(&mut **tx)
        .await?
    else {
        return Ok(None);
    };
    if row.get::<Uuid, _>("task_id") != subject_id
        || row.get::<String, _>("subject_revision") != subject_revision.as_str()
        || row.get::<String, _>("question_digest") != question.as_str()
        || row.get::<Uuid, _>("project_id") != request.scope.project_id
        || row.get::<Uuid, _>("workspace_id") != request.scope.workspace_id
    {
        return Ok(None);
    }
    let revision: i64 = row.get("decision_revision");
    let decision_state: String = row.get("state");
    let receipt: Option<String> = row.get("receipt_digest");
    let expired = row.get::<i64, _>("expires_at_ms") as u64 <= now;
    let state = match (decision_state.as_str(), &receipt) {
        ("answered", Some(_)) => wire::MemberState::Satisfied,
        ("answered", None) => return Ok(None),
        ("pending", _) if !expired => wire::MemberState::Pending,
        _ => wire::MemberState::SourceFailed,
    };
    let result = match state {
        wire::MemberState::Pending => None,
        _ => Some(artifact(
            format!("question-decision-receipt-{decision_id}"),
            match &receipt {
                Some(digest) => serde_json::from_value(json!(digest))?,
                None => value_digest(&json!({"decision_id":decision_id,"state":decision_state}))?,
            },
        )),
    };
    Ok(Some(Observed {
        snapshot: snapshot(
            format!("question-decision-{decision_id}"),
            &json!({"decision_id":decision_id,"revision":revision,"state":decision_state,"receipt":receipt}),
        )?,
        revision: revision.to_string(),
        state,
        result,
    }))
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

async fn message(
    tx: &mut Transaction<'_, Postgres>,
    request: &SourceRequest,
    thread_id: Uuid,
    exclusive_cursor: u64,
    sender: &wire::SenderConstraint,
) -> Result<Option<Observed>> {
    // Task-addressed mailbox only: held for any successor attempt, never consumed.
    let key = Recipient::Task {
        workspace_id: request.scope.workspace_id,
        project_id: request.binding.project_id,
        task_id: request.binding.task_id,
    }
    .key()
    .map_err(|_| super::WaitError("internal"))?;
    let after = i64::try_from(exclusive_cursor).map_err(|_| super::WaitError("internal"))?;
    let rows = sqlx::query("SELECT message_id,cursor,sender,payload_digest FROM mailbox_messages WHERE mailbox_key=$1 AND thread_id=$2 AND cursor>$3 ORDER BY cursor LIMIT 256")
        .bind(&key)
        .bind(thread_id)
        .bind(after)
        .fetch_all(&mut **tx)
        .await?;
    let head: Option<i64> =
        sqlx::query_scalar("SELECT next_cursor FROM mailbox_heads WHERE mailbox_key=$1")
            .bind(&key)
            .fetch_optional(&mut **tx)
            .await?;
    let found = rows.iter().find(|row| {
        let from: Value = row.get("sender");
        match sender {
            wire::SenderConstraint::AnyAuthorized => true,
            wire::SenderConstraint::Task { task_id } => {
                from["binding"]["task_id"] == json!(task_id)
            }
            wire::SenderConstraint::Session { binding } => {
                from["binding"]["attempt_id"] == json!(binding.attempt_id)
            }
        }
    });
    let revision = head.unwrap_or(0).to_string();
    let snapshot = snapshot(
        format!("mailbox-thread-{thread_id}"),
        &json!({"mailbox":key,"thread_id":thread_id,"head":revision}),
    )?;
    Ok(Some(match found {
        Some(row) => {
            let message_id: Uuid = row.get("message_id");
            let payload: String = row.get("payload_digest");
            Observed {
                state: wire::MemberState::Satisfied,
                result: Some(artifact(
                    format!("mailbox-message-{message_id}"),
                    value_digest(
                        &json!({"message_id":message_id,"payload_digest":payload,"cursor":row.get::<i64, _>("cursor")}),
                    )?,
                )),
                revision,
                snapshot,
            }
        }
        None => Observed {
            state: wire::MemberState::Pending,
            result: None,
            revision,
            snapshot,
        },
    }))
}
