//! Exact code-review decision adapter; finality stays in the review owner.
use super::*;
use crate::dashboard_api::{
    actions::{ActionKind, ActionRequest, Disposition, Operation, Outcome, existing},
    contract::Scope,
    errors::ApiError,
};
use crate::profiled_execution::settlement::actor::ActorPin;
use crate::store::User;
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Resolve {
    schema_version: u32,
    operation_id: Uuid,
    decision_id: Uuid,
    expected_decision_revision: u64,
    choice: Choice,
    comment: Option<String>,
    expected_subject: ArtifactRef,
}
impl Resolve {
    fn answer(&self) -> ResolveRequest {
        ResolveRequest {
            schema_version: self.schema_version,
            operation_id: self.operation_id,
            decision_id: self.decision_id,
            expected_decision_revision: self.expected_decision_revision,
            choice: self.choice,
            comment: self.comment.clone(),
        }
    }
    pub(super) fn from_answer(answer: ResolveRequest, subject: ArtifactRef) -> Self {
        Self {
            schema_version: answer.schema_version,
            operation_id: answer.operation_id,
            decision_id: answer.decision_id,
            expected_decision_revision: answer.expected_decision_revision,
            choice: answer.choice,
            comment: answer.comment,
            expected_subject: subject,
        }
    }
    pub(super) fn request(&self) -> ActionRequest {
        existing::request(
            "decision.resolve",
            self.operation_id,
            "decision",
            self.decision_id,
            self.expected_decision_revision.to_string(),
            json!(self),
            "decision",
        )
    }
}
pub(crate) struct ResolveDecision(pub(crate) Option<Arc<Service>>);
impl ResolveDecision {
    fn service(&self) -> std::result::Result<&Service, ApiError> {
        self.0.as_deref().ok_or_else(ApiError::unavailable)
    }
}
fn receipt(view: &serde_json::Value) -> std::result::Result<Option<DecisionReceipt>, ApiError> {
    serde_json::from_value(view["receipt"].clone()).map_err(|_| ApiError::unavailable())
}
fn outcome(receipt: DecisionReceipt, actor: Uuid, operation: Uuid) -> Outcome {
    if matches!(receipt.finality, DecisionFinality::HumanAnswer{owner_user_id,operation_id,..} if owner_user_id==actor && operation_id==operation)
    {
        Outcome::confirmed(
            vec![existing::entity(
                "decision_receipt",
                receipt.decision_id,
                Some(receipt.decision_revision.to_string()),
            )],
            vec![existing::entity(
                "decision",
                receipt.decision_id,
                Some(receipt.decision_revision.to_string()),
            )],
        )
    } else {
        existing::disposition(Disposition::Conflict)
    }
}
fn project(view: &serde_json::Value) -> std::result::Result<Uuid, ApiError> {
    serde_json::from_value(view["subject"]["project_id"].clone())
        .map_err(|_| ApiError::unavailable())
}
#[async_trait]
impl ActionKind for ResolveDecision {
    type Payload = Resolve;
    const KIND: &'static str = "decision.resolve";
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        payload: &Resolve,
    ) -> std::result::Result<Scope, ApiError> {
        existing::owner(state, user).await?;
        payload
            .answer()
            .validate()
            .map_err(|_| ApiError::invalid())?;
        let decision = existing::subject(
            request,
            "decision",
            "decision",
            &payload.expected_decision_revision.to_string(),
        )?;
        if decision != payload.decision_id || request.operation_id != payload.operation_id {
            return Err(ApiError::invalid());
        }
        let pin = ActorPin::observe(state, user)
            .await
            .map_err(ApiError::from)?;
        let view = self
            .service()?
            .inspect_subject(user.id, decision, SubjectProfile::Code, Some(&pin))
            .await
            .map_err(ApiError::from)?;
        let expected: ArtifactRef = serde_json::from_value(view["request"]["subject"].clone())
            .map_err(|_| ApiError::unavailable())?;
        let native: DecisionRequest =
            serde_json::from_value(view["request"].clone()).map_err(|_| ApiError::unavailable())?;
        if !native.eligible_owners.contains(&user.id) {
            return Err(ApiError::for_status(axum::http::StatusCode::FORBIDDEN));
        }
        if expected != payload.expected_subject {
            return Err(ApiError::for_status(axum::http::StatusCode::CONFLICT));
        }
        let old:Option<(Uuid,String)>=sqlx::query_as("SELECT decision_id,request_digest FROM human_decision_finality WHERE actor_id=$1 AND operation_id=$2")
            .bind(user.id).bind(request.operation_id).fetch_optional(&state.store.pool).await?;
        if let Some((decision, digest)) = old {
            let expected = hash(&json!({"actor":user.id,"answer":payload.answer()}))
                .map_err(ApiError::from)?;
            if decision != payload.decision_id || digest != expected {
                return Err(ApiError::for_status(axum::http::StatusCode::CONFLICT));
            }
        }
        Ok(Scope::Project {
            project_id: project(&view)?,
        })
    }
    async fn authorize_lookup(
        &self,
        state: &AppState,
        user: &User,
        scope: &Scope,
        operation: Option<&Operation>,
    ) -> std::result::Result<(), ApiError> {
        existing::owner(state, user).await?;
        if let Some(op) = operation {
            let [subject] = op.subject_refs.as_slice() else {
                return Err(ApiError::invalid());
            };
            if subject.entity_type != "decision" {
                return Err(ApiError::invalid());
            }
            let decision = subject.id.parse().map_err(|_| ApiError::invalid())?;
            let pin = ActorPin::observe(state, user)
                .await
                .map_err(ApiError::from)?;
            let view = self
                .service()?
                .inspect_subject(user.id, decision, SubjectProfile::Code, Some(&pin))
                .await
                .map_err(ApiError::from)?;
            if *scope
                != (Scope::Project {
                    project_id: project(&view)?,
                })
            {
                return Err(ApiError::invalid());
            }
            if op.disposition == Disposition::Confirmed {
                let expected = outcome(
                    receipt(&view)?.ok_or_else(ApiError::unavailable)?,
                    user.id,
                    op.operation_id,
                );
                if expected.disposition != Disposition::Confirmed
                    || op.receipt_refs != expected.receipt_refs
                    || op.affected_entities != expected.affected_entities
                {
                    return Err(ApiError::invalid());
                }
            }
        } else {
            // Tombstones contain only scope; require current project access.
            let Scope::Project { project_id } = scope else {
                return Err(ApiError::invalid());
            };
            crate::agent_access::human::authorize_project(state, user, *project_id)
                .await
                .map_err(ApiError::from)?;
        }
        Ok(())
    }
    async fn execute(
        &self,
        state: &AppState,
        user: &User,
        _request: &ActionRequest,
        payload: Resolve,
    ) -> Outcome {
        let Ok(service) = self.service() else {
            return Outcome::unknown();
        };
        let pin = match ActorPin::observe(state, user).await {
            Ok(pin) => pin,
            Err(error) => return existing::failure(error),
        };
        match service
            .resolve_subject(
                user.id,
                &payload.answer(),
                SubjectProfile::Code,
                Some(&payload.expected_subject),
                Some(&pin),
            )
            .await
        {
            Ok(r) => outcome(r, user.id, payload.operation_id),
            Err(e) => existing::failure(e),
        }
    }
    async fn reconcile(
        &self,
        state: &AppState,
        user: &User,
        operation: &Operation,
    ) -> std::result::Result<Option<Outcome>, ApiError> {
        let Some(subject) = operation.subject_refs.first() else {
            return Ok(None);
        };
        let decision = subject.id.parse().map_err(|_| ApiError::invalid())?;
        let pin = ActorPin::observe(state, user)
            .await
            .map_err(ApiError::from)?;
        let view = self
            .service()?
            .inspect_subject(user.id, decision, SubjectProfile::Code, Some(&pin))
            .await
            .map_err(ApiError::from)?;
        Ok(receipt(&view)?.map(|r| outcome(r, user.id, operation.operation_id)))
    }
}
