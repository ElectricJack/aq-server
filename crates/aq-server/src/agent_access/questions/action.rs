//! G1b `question.answer`: the owner answers one F5 question at its exact
//! decision revision. Discord replies and notification acknowledgments are
//! never answers; this action (dashboard inbox or authenticated link) is.
use super::{MAX_ANSWER_BYTES, answer};
use crate::AppState;
use crate::dashboard_api::{
    actions::{self, ActionKind, ActionRequest, Disposition, Outcome, Registry, existing},
    contract::Scope,
    errors::ApiError,
};
use crate::store::User;
use async_trait::async_trait;
use axum::http::StatusCode;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub(crate) const KIND: &str = "question.answer";

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct QuestionAnswer {
    pub operation_id: Uuid,
    /// The decision revision being answered, as returned when it was asked.
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_revision: String,
    #[schemars(length(min = 1, max = 16000))]
    pub answer: String,
}

pub(crate) struct AnswerQuestion;

pub(crate) fn register(r: &mut Registry) {
    r.register(AnswerQuestion).expect("question answer adapter");
}
pub(crate) fn schemas(r: &mut Registry) {
    r.register_schema::<AnswerQuestion>()
        .expect("question answer schema");
}

fn revision(value: &str) -> Option<u64> {
    value
        .parse::<u64>()
        .ok()
        .filter(|r| *r > 0 && *r <= i64::MAX as u64 && r.to_string() == value)
}
fn decision(request: &ActionRequest, p: &QuestionAnswer) -> Result<(Uuid, u64), ApiError> {
    let id = existing::subject(request, "decision", "decision", &p.expected_revision)?;
    let revision = revision(&p.expected_revision).ok_or_else(ApiError::invalid)?;
    if request.operation_id != p.operation_id
        || p.answer.trim().is_empty()
        || p.answer.len() > MAX_ANSWER_BYTES
    {
        return Err(ApiError::invalid());
    }
    Ok((id, revision))
}
fn confirmed(response: &Value) -> Outcome {
    let (Some(id), Some(revision)) = (
        response["decision_id"].as_str(),
        response["decision_revision"].as_str(),
    ) else {
        return Outcome::unknown();
    };
    Outcome::confirmed(
        vec![existing::entity(
            "decision_receipt",
            id,
            Some(revision.into()),
        )],
        vec![existing::entity("decision", id, Some(revision.into()))],
    )
}

#[async_trait]
impl ActionKind for AnswerQuestion {
    type Payload = QuestionAnswer;
    const KIND: &'static str = KIND;
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        p: &QuestionAnswer,
    ) -> Result<Scope, ApiError> {
        let (id, _) = decision(request, p)?;
        existing::owner(state, user).await?;
        let project: Uuid =
            sqlx::query_scalar("SELECT project_id FROM question_decisions WHERE decision_id=$1")
                .bind(id)
                .fetch_optional(&state.store.pool)
                .await
                .map_err(|_| ApiError::unavailable())?
                .ok_or_else(|| ApiError::for_status(StatusCode::NOT_FOUND))?;
        Ok(Scope::Project {
            project_id: project,
        })
    }
    async fn authorize_lookup(
        &self,
        state: &AppState,
        user: &User,
        scope: &Scope,
        operation: Option<&actions::Operation>,
    ) -> Result<(), ApiError> {
        existing::owner(state, user).await?;
        if !matches!(scope, Scope::Project { .. }) {
            return Err(ApiError::invalid());
        }
        if let Some(op) = operation
            && (op.subject_refs.len() != 1 || op.subject_refs[0].entity_type != "decision")
        {
            return Err(ApiError::invalid());
        }
        Ok(())
    }
    async fn reconcile(
        &self,
        state: &AppState,
        user: &User,
        operation: &actions::Operation,
    ) -> Result<Option<Outcome>, ApiError> {
        // Read-only: this owner's recorded answer for the operation, if any.
        let row: Option<(Uuid, i64)> = sqlx::query_as(
            "SELECT decision_id,decision_revision FROM question_decisions WHERE answered_by=$1 AND answer_operation_id=$2",
        )
        .bind(user.id)
        .bind(operation.operation_id)
        .fetch_optional(&state.store.pool)
        .await
        .map_err(|_| ApiError::unavailable())?;
        Ok(row.map(|(id, revision)| {
            confirmed(&serde_json::json!({
                "decision_id": id.to_string(),
                "decision_revision": revision.to_string(),
            }))
        }))
    }
    async fn execute(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        p: QuestionAnswer,
    ) -> Outcome {
        let Ok((id, expected)) = decision(request, &p) else {
            return Outcome::unknown();
        };
        match answer(state, user.id, id, expected, p.operation_id, &p.answer).await {
            Ok(response) => confirmed(&response),
            Err(error) => match error.0 {
                "id_conflict" | "stale_revision" => existing::disposition(Disposition::Conflict),
                "invalid_request" | "not_found" | "scope_denied" => {
                    existing::disposition(Disposition::Rejected)
                }
                _ => Outcome::unknown(),
            },
        }
    }
}
