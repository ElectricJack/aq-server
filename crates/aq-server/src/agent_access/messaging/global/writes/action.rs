//! Confirmation is durable enqueue, never supervisor-host or agent delivery.
use super::*;
use crate::dashboard_api::{
    actions::{self, ActionKind, ActionRequest, Disposition, Outcome, existing},
    contract::Scope,
    errors::ApiError,
};
use async_trait::async_trait;
pub(crate) struct SendMessage(pub(crate) Arc<Service>);
pub(super) fn request(conversation: Uuid, payload: &Create) -> ActionRequest {
    existing::request(
        "supervisor.message.send",
        payload.operation_id,
        "conversation",
        conversation,
        payload.expected_conversation_revision.clone(),
        json!(payload),
        "conversation",
    )
}
fn result(receipt: Receipt) -> Outcome {
    if receipt.state == "queued" {
        Outcome::confirmed(
            vec![existing::entity(
                "mailbox_operation",
                receipt.operation_id,
                None,
            )],
            vec![existing::entity(
                "message",
                receipt.message_id,
                receipt.cursor,
            )],
        )
    } else {
        existing::disposition(Disposition::Conflict)
    }
}
#[async_trait]
impl ActionKind for SendMessage {
    type Payload = Create;
    const KIND: &'static str = "supervisor.message.send";
    const GRANT: &'static str = aq_agent_grants::op::MESSAGE_SEND;
    const TARGET: aq_command_registry::TargetRule = aq_command_registry::TargetRule::Own;
    async fn authorize(
        &self,
        _state: &AppState,
        user: &User,
        request: &ActionRequest,
        payload: &Create,
    ) -> std::result::Result<Scope, ApiError> {
        Create::parse(&serde_json::to_vec(payload).map_err(|_| ApiError::invalid())?)
            .map_err(ApiError::from)?;
        if request.operation_id != payload.operation_id {
            return Err(ApiError::invalid());
        }
        let id = existing::subject(
            request,
            "conversation",
            "conversation",
            &payload.expected_conversation_revision,
        )?;
        let mut tx = self.0.tx().await.map_err(ApiError::from)?;
        owner(&mut tx, user).await.map_err(ApiError::from)?;
        definition(&mut tx, Some(id), false)
            .await
            .map_err(ApiError::from)?;
        let sender = author(user.id);
        let sender_key = sender_key(&sender).map_err(|_| ApiError::unavailable())?;
        let native_digest=digest(&json!({"kind":"global_supervisor_send","conversation_id":id,"request":payload,"sender":sender})).map_err(|_|ApiError::unavailable())?;
        replay(&mut tx, &sender_key, request.operation_id, &native_digest)
            .await
            .map_err(|error| {
                if error.0 == "id_conflict" {
                    ApiError::for_status(axum::http::StatusCode::CONFLICT)
                } else {
                    ApiError::unavailable()
                }
            })?;
        tx.commit().await?;
        Ok(Scope::AllAuthorized)
    }
    async fn authorize_lookup(
        &self,
        _state: &AppState,
        user: &User,
        scope: &Scope,
        operation: Option<&actions::Operation>,
    ) -> std::result::Result<(), ApiError> {
        if *scope != Scope::AllAuthorized {
            return Err(ApiError::invalid());
        }
        let mut tx = self.0.tx().await.map_err(ApiError::from)?;
        owner(&mut tx, user).await.map_err(ApiError::from)?;
        let conversation = definition(
            &mut tx,
            operation
                .and_then(|op| op.subject_refs.first())
                .and_then(|r| r.id.parse().ok()),
            false,
        )
        .await
        .map_err(ApiError::from)?;
        tx.commit().await?;
        if let Some(op) = operation {
            if op.subject_refs.len() != 1
                || op.subject_refs[0].entity_type != "conversation"
                || op.subject_refs[0].id != conversation.id.to_string()
            {
                return Err(ApiError::invalid());
            }
            if op.disposition == Disposition::Confirmed {
                let Json(receipt) = lookup(
                    Extension(self.0.clone()),
                    RequireOwner(user.clone()),
                    Path((conversation.id, op.operation_id)),
                )
                .await
                .map_err(ApiError::from)?;
                let expected = result(receipt);
                if op.receipt_refs != expected.receipt_refs
                    || op.affected_entities != expected.affected_entities
                {
                    return Err(ApiError::invalid());
                }
            }
        }
        Ok(())
    }
    async fn execute(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        payload: Create,
    ) -> Outcome {
        let Ok(id) = existing::subject(
            request,
            "conversation",
            "conversation",
            &payload.expected_conversation_revision,
        ) else {
            return Outcome::unknown();
        };
        let bytes = Bytes::from(serde_json::to_vec(&payload).expect("typed enqueue"));
        match enqueue_domain(state, &self.0, user, id, bytes).await {
            Ok(r) => result(r),
            Err(e) => existing::failure(e),
        }
    }
    async fn reconcile(
        &self,
        _state: &AppState,
        user: &User,
        operation: &actions::Operation,
    ) -> std::result::Result<Option<Outcome>, ApiError> {
        let Some(subject) = operation.subject_refs.first() else {
            return Ok(None);
        };
        let id = subject
            .id
            .parse::<Uuid>()
            .map_err(|_| ApiError::invalid())?;
        match lookup(
            Extension(self.0.clone()),
            RequireOwner(user.clone()),
            Path((id, operation.operation_id)),
        )
        .await
        {
            Ok(Json(r)) => Ok(Some(result(r))),
            Err(AppError::NotFound(_)) => Ok(None),
            Err(e) => Err(ApiError::from(e)),
        }
    }
}
