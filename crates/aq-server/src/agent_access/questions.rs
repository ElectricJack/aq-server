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
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

pub mod action;
pub mod inbox;

mod answer;
mod ask;
pub use answer::answer;
pub use ask::ask;

use aq_wait_engine::question::{Command, Route};
pub use aq_wait_engine::question::{
    DecisionRevision, MAX_ANSWER_BYTES, MAX_QUESTION_BYTES, ROUTES, Submission,
};
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

/// F4 `question.asked`: best-effort, after commit; carries only the decision ID.
async fn notify(
    state: &AppState,
    origin: aq_notifications::policy::Origin,
    decision: Uuid,
    open: bool,
) {
    let _ = crate::notifications::policy::Policy::new(crate::notifications::api::service(state))
        .observe_question(origin, decision, open)
        .await;
}

pub fn machine_routes() -> Router<AppState> {
    Router::new()
        .route(
            ROUTES[0].0,
            post(|State(state), headers, body| dispatch(state, headers, body, Route::Ask)),
        )
        .route(
            ROUTES[1].0,
            post(|State(state), headers, body| dispatch(state, headers, body, Route::Show)),
        )
}
async fn caller(
    state: &AppState,
    headers: &HeaderMap,
    path: &str,
) -> std::result::Result<(Uuid, Uuid), Response> {
    let principal = authenticate(state, headers)
        .await
        .map_err(IntoResponse::into_response)?;
    super::grant_check::require(state, headers, &principal, path, None).await?;
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

async fn dispatch(state: AppState, headers: HeaderMap, body: Bytes, route: Route) -> Response {
    let (node, attempt) = match caller(&state, &headers, route.path()).await {
        Ok(ids) => ids,
        Err(response) => return response,
    };
    let result = match route.decode(&body) {
        Ok(Command::Ask {
            operation_id,
            question,
        }) => {
            ask(
                &state,
                node,
                attempt,
                Submission::new(operation_id, &question),
            )
            .await
        }
        Ok(Command::Show { decision_id }) => show(&state, node, attempt, decision_id).await,
        Err(reason) => Err(QuestionError(reason)),
    };
    reply(result)
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
