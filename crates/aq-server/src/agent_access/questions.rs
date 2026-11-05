//! F5 agent questions: an agent asks its owner a typed `question` decision and
//! waits on it through an H4 `HumanDecision` wait. The owner's answer targets
//! the exact decision revision. A live waiting attempt is woken by the wait
//! only; an answer that arrives after the asking attempt ended is queued as
//! task-addressed mail for the continuation. Old session-addressed mail is
//! never retargeted, and notifications (F4) carry only the decision ID.
use super::{
    authenticate, id,
    messaging::{OwnerSendError, Recipient, Send, question_send_in_tx},
    now, ok,
    waits::{self, WaitError},
};
use crate::AppState;
use aq_wait_engine::wire;
use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::HeaderMap,
    response::{IntoResponse, Response},
    routing::post,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

pub mod action;
pub mod inbox;

/// Route → capability. Published in `protocol/agent-questions/v1/routes.json`.
pub const ROUTES: [(&str, &str); 2] = [
    ("/v1/agent/questions", "question.ask"),
    ("/v1/agent/questions/show", "question.show"),
];
pub const MAX_QUESTION_BYTES: usize = 4096;
/// Leaves room for the fixed mail preamble inside the 16 KiB mailbox body.
pub const MAX_ANSWER_BYTES: usize = 16_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuestionError(pub &'static str);
pub type Result<T> = std::result::Result<T, QuestionError>;
impl From<sqlx::Error> for QuestionError {
    fn from(_: sqlx::Error) -> Self {
        Self("internal")
    }
}
impl From<serde_json::Error> for QuestionError {
    fn from(_: serde_json::Error) -> Self {
        Self("internal")
    }
}
impl From<wire::ContractError> for QuestionError {
    fn from(_: wire::ContractError) -> Self {
        Self("internal")
    }
}
impl From<WaitError> for QuestionError {
    fn from(error: WaitError) -> Self {
        Self(error.0)
    }
}

fn text(value: String) -> Result<wire::Text> {
    wire::Text::new(value).map_err(|_| QuestionError("internal"))
}
fn digest(value: &Value) -> Result<String> {
    Ok(wire::Digest::of(&serde_json::to_vec(value)?)
        .as_str()
        .to_owned())
}
fn view(row: &sqlx::postgres::PgRow, with_answer: bool) -> Value {
    let mut value = json!({
        "api_version": 1,
        "decision_id": row.get::<Uuid, _>("decision_id"),
        "decision_revision": row.get::<i64, _>("decision_revision").to_string(),
        "kind": "question",
        "state": row.get::<String, _>("state"),
        "subject_id": row.get::<Uuid, _>("task_id"),
        "subject_revision": row.get::<String, _>("subject_revision"),
        "question_digest": row.get::<String, _>("question_digest"),
        "wait_id": row.get::<Uuid, _>("wait_id"),
        "expires_at_ms": row.get::<i64, _>("expires_at_ms"),
    });
    if with_answer && let Some(answer) = row.get::<Option<String>, _>("answer") {
        value["answer"] = json!(answer);
        value["receipt_digest"] = json!(row.get::<Option<String>, _>("receipt_digest"));
    }
    value
}

/// Ask (or replay) one question for the authenticated attempt. The decision
/// and its wait commit together, or neither does.
pub async fn ask(
    state: &AppState,
    node: Uuid,
    attempt: Uuid,
    operation_id: Uuid,
    question: &str,
) -> Result<Value> {
    if operation_id.is_nil() || question.trim().is_empty() || question.len() > MAX_QUESTION_BYTES {
        return Err(QuestionError("invalid_request"));
    }
    let question_digest = wire::Digest::of(question.as_bytes());
    let mut tx = state.store.pool.begin().await?;
    let caller = waits::load_caller(state, &mut tx, node, attempt).await?;
    if let Some(row) =
        sqlx::query("SELECT * FROM question_decisions WHERE attempt_id=$1 AND operation_id=$2")
            .bind(attempt)
            .bind(operation_id)
            .fetch_optional(&mut *tx)
            .await?
    {
        if row.get::<String, _>("question_digest") != question_digest.as_str() {
            return Err(QuestionError("id_conflict"));
        }
        return Ok(view(&row, false));
    }
    let binding = caller.fence.binding.clone();
    let scope = caller.fence.scope.clone();
    let at = now(state);
    let decision_id = Uuid::new_v4();
    let wait_id = Uuid::new_v5(&decision_id, b"question-wait");
    let subject_revision = format!(
        "attempt:{}:{}",
        binding.attempt_id, binding.attempt_generation
    );
    let expires_at = at + aq_wait_engine::MAX_AGE_MS;
    let row = sqlx::query("INSERT INTO question_decisions(decision_id,attempt_id,operation_id,node_id,project_id,workspace_id,task_id,session,attempt_generation,question,question_digest,subject_revision,decision_revision,state,expires_at_ms,wait_id,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,1,'pending',$13,$14,$15) RETURNING *")
        .bind(decision_id)
        .bind(attempt)
        .bind(operation_id)
        .bind(node)
        .bind(scope.project_id)
        .bind(scope.workspace_id)
        .bind(binding.task_id)
        .bind(serde_json::to_value(&binding.session)?)
        .bind(i64::try_from(binding.attempt_generation).map_err(|_| QuestionError("internal"))?)
        .bind(question)
        .bind(question_digest.as_str())
        .bind(&subject_revision)
        .bind(i64::try_from(expires_at).map_err(|_| QuestionError("internal"))?)
        .bind(wait_id)
        .bind(i64::try_from(at).map_err(|_| QuestionError("internal"))?)
        .fetch_one(&mut *tx)
        .await?;
    let predicate = wire::Predicate::Single {
        member: wire::Member {
            member_id: Uuid::new_v5(&decision_id, b"question-wait-member"),
            condition: wire::Condition::HumanDecision {
                decision_id,
                subject_id: binding.task_id,
                subject_revision: text(subject_revision)?,
                question_digest,
            },
        },
    };
    let request = wire::Request::seal(
        Uuid::new_v5(&decision_id, b"question-wait-operation"),
        wire::Command::RegisterWait(wire::WaitRegistration {
            scope: scope.clone(),
            binding,
            wait_id,
            wait_generation: 1,
            continuation_generation: 1,
            policy: waits::default_policy().reference,
            predicate_digest: wire::canonical_digest(&predicate)?,
            predicate,
            preference: wire::WaitPreference::Economical,
        }),
    )?;
    let response = waits::register_locked(state, &mut tx, &caller, &request).await?;
    if !matches!(response.body, wire::ResponseBody::WaitRegistered(_)) {
        // Typically a second active wait for this attempt: nothing is kept.
        return Err(QuestionError("wait_conflict"));
    }
    tx.commit().await?;
    notify(state, operation_id, &scope, decision_id, true).await;
    Ok(view(&row, false))
}

/// The caller task's own question, with the answer once one is recorded.
pub async fn show(state: &AppState, node: Uuid, attempt: Uuid, decision_id: Uuid) -> Result<Value> {
    let mut tx = state.store.pool.begin().await?;
    let caller = waits::load_caller(state, &mut tx, node, attempt).await?;
    let row = sqlx::query(
        "SELECT * FROM question_decisions WHERE decision_id=$1 AND project_id=$2 AND workspace_id=$3 AND task_id=$4",
    )
    .bind(decision_id)
    .bind(caller.fence.scope.project_id)
    .bind(caller.fence.scope.workspace_id)
    .bind(caller.fence.binding.task_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(QuestionError("not_found"))?;
    tx.commit().await?;
    Ok(view(&row, true))
}

/// Record the owner's answer at `expected_revision`. Replay of the same owner
/// operation with the same text returns the original receipt.
pub async fn answer(
    state: &AppState,
    owner: Uuid,
    decision_id: Uuid,
    expected_revision: u64,
    operation_id: Uuid,
    answer: &str,
) -> Result<Value> {
    if operation_id.is_nil() || answer.trim().is_empty() || answer.len() > MAX_ANSWER_BYTES {
        return Err(QuestionError("invalid_request"));
    }
    let mut tx = state.store.pool.begin().await?;
    let row = sqlx::query("SELECT * FROM question_decisions WHERE decision_id=$1 FOR UPDATE")
        .bind(decision_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(QuestionError("not_found"))?;
    if row.get::<String, _>("state") == "answered" {
        let same = row.get::<Option<Uuid>, _>("answered_by") == Some(owner)
            && row.get::<Option<Uuid>, _>("answer_operation_id") == Some(operation_id)
            && row.get::<Option<String>, _>("answer").as_deref() == Some(answer);
        return if same {
            Ok(receipt(&row))
        } else {
            Err(QuestionError("id_conflict"))
        };
    }
    let revision = row.get::<i64, _>("decision_revision");
    if u64::try_from(revision).ok() != Some(expected_revision) {
        return Err(QuestionError("stale_revision"));
    }
    let role: Option<String> = sqlx::query_scalar("SELECT role FROM users WHERE id=$1 FOR SHARE")
        .bind(owner)
        .fetch_optional(&mut *tx)
        .await?;
    if role.as_deref() != Some("owner") {
        return Err(QuestionError("scope_denied"));
    }
    let next = revision + 1;
    let at = now(state);
    let receipt_digest = digest(&json!({
        "decision_id": decision_id,
        "decision_revision": next,
        "answer_digest": wire::Digest::of(answer.as_bytes()).as_str(),
        "answered_by": owner,
        "operation_id": operation_id,
    }))?;
    let attempt: Uuid = row.get("attempt_id");
    let (project, workspace, task): (Uuid, Uuid, Uuid) = (
        row.get("project_id"),
        row.get("workspace_id"),
        row.get("task_id"),
    );
    let delivery = if ended(&mut tx, attempt).await? {
        let message_id = Uuid::new_v5(&operation_id, b"question-answer-message");
        question_send_in_tx(
            &mut tx,
            Send {
                operation_id,
                message_id,
                thread_id: decision_id,
                recipient: Recipient::Task {
                    workspace_id: workspace,
                    project_id: project,
                    task_id: task,
                },
                body: format!(
                    "AQ answer to your question (decision {decision_id}, revision {next}):\n\n{answer}"
                ),
            },
            at,
        )
        .await
        .map_err(|error| match error {
            OwnerSendError::Invalid => QuestionError("invalid_request"),
            OwnerSendError::NotFound => QuestionError("not_found"),
            OwnerSendError::Conflict => QuestionError("id_conflict"),
            OwnerSendError::Unknown => QuestionError("internal"),
        })?;
        json!({"kind":"task_mail","message_id":message_id})
    } else {
        json!({"kind":"wait"})
    };
    let row = sqlx::query("UPDATE question_decisions SET state='answered',decision_revision=$2,answer=$3,answered_by=$4,answer_operation_id=$5,receipt_digest=$6,answered_at_ms=$7,delivery=$8 WHERE decision_id=$1 RETURNING *")
        .bind(decision_id)
        .bind(next)
        .bind(answer)
        .bind(owner)
        .bind(operation_id)
        .bind(&receipt_digest)
        .bind(i64::try_from(at).map_err(|_| QuestionError("internal"))?)
        .bind(&delivery)
        .fetch_one(&mut *tx)
        .await
        .map_err(|error| match error {
            // The owner reused one operation ID for a different decision.
            sqlx::Error::Database(e) if e.is_unique_violation() => QuestionError("id_conflict"),
            _ => QuestionError("internal"),
        })?;
    tx.commit().await?;
    let scope = wire::Scope {
        project_id: project,
        workspace_id: workspace,
    };
    notify(state, operation_id, &scope, decision_id, false).await;
    Ok(receipt(&row))
}
fn receipt(row: &sqlx::postgres::PgRow) -> Value {
    json!({
        "decision_id": row.get::<Uuid, _>("decision_id"),
        "decision_revision": row.get::<i64, _>("decision_revision").to_string(),
        "receipt_digest": row.get::<Option<String>, _>("receipt_digest"),
        "delivery": row.get::<Option<Value>, _>("delivery"),
    })
}
/// The asking attempt can no longer receive a wait wake: it released the task,
/// was cancelled, or its launch binding was revoked. A merely stopped attempt
/// with a live wait is a hibernated continuation and still wakes by the wait.
async fn ended(tx: &mut Transaction<'_, Postgres>, attempt: Uuid) -> Result<bool> {
    Ok(sqlx::query_scalar("SELECT NOT e.holds_task OR e.state->>'cancellation_id' IS NOT NULL OR COALESCE(b.revoked,FALSE) FROM execution_attempts e LEFT JOIN agent_launch_bindings b USING(attempt_id) WHERE e.attempt_id=$1")
        .bind(attempt)
        .fetch_optional(&mut **tx)
        .await?
        .unwrap_or(true))
}
/// F4 `question.asked`: best-effort, after commit; carries only the decision ID.
async fn notify(state: &AppState, source: Uuid, scope: &wire::Scope, decision: Uuid, open: bool) {
    let _ = crate::notifications::policy::Policy::new(crate::notifications::api::service(state))
        .observe_question(
            source,
            aq_lifecycle_api::Scope {
                project_id: scope.project_id,
                workspace_id: scope.workspace_id,
            },
            decision,
            open,
        )
        .await;
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AskBody {
    api_version: u32,
    operation_id: Uuid,
    question: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ShowBody {
    api_version: u32,
    decision_id: Uuid,
}

pub fn machine_routes() -> Router<AppState> {
    Router::new()
        .route(ROUTES[0].0, post(ask_route))
        .route(ROUTES[1].0, post(show_route))
}
async fn caller(
    state: &AppState,
    headers: &HeaderMap,
) -> std::result::Result<(Uuid, Uuid), Response> {
    let principal = authenticate(state, headers)
        .await
        .map_err(IntoResponse::into_response)?;
    match (
        id(&principal.binding, "node_id"),
        id(&principal.binding, "attempt_id"),
    ) {
        (Ok(node), Ok(attempt)) => Ok((node, attempt)),
        _ => Err(super::Error("unauthorized").into_response()),
    }
}
fn reply(result: Result<Value>) -> Response {
    match result {
        Ok(value) => ok(value),
        Err(error) => super::Error(error.0).into_response(),
    }
}
async fn ask_route(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let (node, attempt) = match caller(&state, &headers).await {
        Ok(ids) => ids,
        Err(response) => return response,
    };
    match serde_json::from_slice::<AskBody>(&body) {
        Ok(b) if b.api_version == 1 => {
            reply(ask(&state, node, attempt, b.operation_id, &b.question).await)
        }
        Ok(_) => super::Error("unsupported_api_version").into_response(),
        Err(_) => super::Error("invalid_request").into_response(),
    }
}
async fn show_route(State(state): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    let (node, attempt) = match caller(&state, &headers).await {
        Ok(ids) => ids,
        Err(response) => return response,
    };
    match serde_json::from_slice::<ShowBody>(&body) {
        Ok(b) if b.api_version == 1 => reply(show(&state, node, attempt, b.decision_id).await),
        Ok(_) => super::Error("unsupported_api_version").into_response(),
        Err(_) => super::Error("invalid_request").into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::ROUTES;
    #[test]
    fn question_routes_match_contract() {
        let routes: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../protocol/agent-questions/v1/routes.json"
        ))
        .unwrap();
        let listed = routes["routes"].as_array().unwrap();
        assert_eq!(listed.len(), ROUTES.len());
        for (path, capability) in ROUTES {
            let entry = listed.iter().find(|r| r["path"] == path).unwrap();
            assert_eq!(entry["capability"], capability);
            assert_eq!(entry["method"], "POST");
        }
    }
}
