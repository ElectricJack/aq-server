//! Dashboard Message slot: an owner's durable enqueue into one session mailbox.
//!
//! Confirmation means the message is queued in the session's scoped mailbox,
//! nothing more: runner delivery, harness input and nudges belong to their own
//! owners (F3 and H3). The recipient is derived from the session's retained
//! launch binding, never from client-supplied scope.
use super::*;
use crate::dashboard_api::{
    actions::{self, ActionKind, ActionRequest, Disposition, Outcome, Registry, existing},
    contract::Scope,
    errors::ApiError,
};
use crate::store::User;
use async_trait::async_trait;
use schemars::JsonSchema;

pub(crate) const KIND: &str = "session.message.send";

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionMessage {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    pub message_id: Uuid,
    /// Mailbox thread; a new thread named by `message_id` when omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<Uuid>,
    /// The session's assignment generation, as projected in `session_identity`.
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_generation: String,
    #[schemars(length(min = 1, max = 16384))]
    pub body: String,
}

pub(crate) struct SendSessionMessage;

pub(crate) const NUDGE_KIND: &str = "session.nudge";

/// H3 owner nudge: the same safe-boundary path as the stall ladder. Input that
/// is unknown, or a pane a human controls, keeps it inbox-only.
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct SessionNudge {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    /// The session's assignment generation, as projected in `session_identity`.
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_generation: String,
    #[schemars(length(min = 1, max = 16384))]
    pub body: String,
}

pub(crate) struct NudgeSession;

pub(crate) fn register(r: &mut Registry) {
    r.register(SendSessionMessage)
        .expect("session message adapter");
    r.register(NudgeSession).expect("session nudge adapter");
}
pub(crate) fn schemas(r: &mut Registry) {
    r.register_schema::<SendSessionMessage>()
        .expect("session message schema");
    r.register_schema::<NudgeSession>()
        .expect("session nudge schema");
}

fn owner_sender(user: &User) -> Value {
    json!({"kind":"owner","user_id":user.id})
}
fn generation(value: &str) -> Option<u64> {
    value
        .parse::<u64>()
        .ok()
        .filter(|g| *g > 0 && *g <= i64::MAX as u64 && g.to_string() == value)
}
fn session_id(request: &ActionRequest, p: &SessionMessage) -> std::result::Result<Uuid, ApiError> {
    let id = existing::subject(request, "session", "session", &p.expected_generation)?;
    if request.operation_id != p.operation_id
        || p.project_id.is_nil()
        || p.message_id.is_nil()
        || p.message_id == p.operation_id
        || p.thread_id.is_some_and(|t| t.is_nil())
        || p.body.trim().is_empty()
        || p.body.len() > 16384
        || generation(&p.expected_generation).is_none()
    {
        return Err(ApiError::invalid());
    }
    Ok(id)
}
enum Live {
    Current(Recipient),
    Stale(u64),
}
/// The live mailbox recipient for a session with an active launch binding.
async fn recipient(
    state: &AppState,
    session: Uuid,
    project: Uuid,
    expected: u64,
) -> std::result::Result<Live, ApiError> {
    let row = sqlx::query("SELECT b.binding,b.revoked FROM execution_attempts e JOIN agent_launch_bindings b USING(attempt_id) WHERE e.session_id=$1")
        .bind(session)
        .fetch_optional(&state.store.pool)
        .await
        .map_err(|_| ApiError::unavailable())?
        .ok_or_else(|| ApiError::for_status(StatusCode::NOT_FOUND))?;
    let b: Value = row.get("binding");
    if row.get::<bool, _>("revoked") || b["project_id"] != json!(project) {
        return Err(ApiError::for_status(StatusCode::NOT_FOUND));
    }
    let live = b["generation"].as_u64().ok_or_else(ApiError::unavailable)?;
    if b["session_id"] != json!(session) {
        return Err(ApiError::unavailable());
    }
    if live != expected {
        return Ok(Live::Stale(live));
    }
    let field = |k: &str| id(&b, k).map_err(|_| ApiError::unavailable());
    Ok(Live::Current(Recipient::Session {
        workspace_id: field("workspace_id")?,
        project_id: project,
        task_id: field("task_id")?,
        session_id: session,
        attempt_id: field("attempt_id")?,
        generation: live,
    }))
}
fn result(response: &Value) -> Outcome {
    let (Some(op), Some(message), Some(cursor)) = (
        response["operation_id"].as_str(),
        response["message_id"].as_str(),
        response["cursor"].as_str(),
    ) else {
        return Outcome::unknown();
    };
    if response["disposition"] != "queued" {
        return existing::disposition(Disposition::Conflict);
    }
    Outcome::confirmed(
        vec![existing::entity("mailbox_operation", op, None)],
        vec![existing::entity("message", message, Some(cursor.into()))],
    )
}

#[async_trait]
impl ActionKind for SendSessionMessage {
    type Payload = SessionMessage;
    const KIND: &'static str = KIND;
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        p: &SessionMessage,
    ) -> std::result::Result<Scope, ApiError> {
        let session = session_id(request, p)?;
        existing::owner(state, user).await?;
        // A stale generation is admitted so the operation records a conflict.
        recipient(state, session, p.project_id, 1).await?;
        Ok(Scope::Project {
            project_id: p.project_id,
        })
    }
    async fn authorize_lookup(
        &self,
        state: &AppState,
        user: &User,
        scope: &Scope,
        operation: Option<&actions::Operation>,
    ) -> std::result::Result<(), ApiError> {
        existing::owner(state, user).await?;
        if !matches!(scope, Scope::Project { .. }) {
            return Err(ApiError::invalid());
        }
        if let Some(op) = operation
            && (op.subject_refs.len() != 1 || op.subject_refs[0].entity_type != "session")
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
    ) -> std::result::Result<Option<Outcome>, ApiError> {
        // Read-only: the mailbox owner's own retained receipt for this operation.
        let key = sender_key(&owner_sender(user)).map_err(|_| ApiError::unavailable())?;
        let response: Option<Value> = sqlx::query_scalar(
            "SELECT response FROM mailbox_operations WHERE sender_key=$1 AND operation_id=$2",
        )
        .bind(key)
        .bind(operation.operation_id)
        .fetch_optional(&state.store.pool)
        .await
        .map_err(|_| ApiError::unavailable())?;
        Ok(response.as_ref().map(result))
    }
    async fn execute(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        p: SessionMessage,
    ) -> Outcome {
        let (Ok(session), Some(expected)) =
            (session_id(request, &p), generation(&p.expected_generation))
        else {
            return Outcome::unknown();
        };
        let recipient = match recipient(state, session, p.project_id, expected).await {
            Ok(Live::Current(r)) => r,
            Ok(Live::Stale(live)) => {
                return Outcome::conflict([("session".to_string(), live.to_string())].into());
            }
            Err(e) if e.status() == StatusCode::NOT_FOUND => {
                return existing::disposition(Disposition::Rejected);
            }
            Err(_) => return Outcome::unknown(),
        };
        let Ok(mut tx) = state.store.pool.begin().await else {
            return Outcome::unknown();
        };
        let sent = owner_send_in_tx(
            &mut tx,
            Send {
                operation_id: p.operation_id,
                message_id: p.message_id,
                thread_id: p.thread_id.unwrap_or(p.message_id),
                recipient,
                body: p.body,
            },
            user.id,
            now(state),
        )
        .await;
        match sent {
            Ok(response) if tx.commit().await.is_ok() => result(&response),
            Ok(_) => Outcome::unknown(),
            Err(OwnerSendError::Invalid | OwnerSendError::NotFound) => {
                existing::disposition(Disposition::Rejected)
            }
            Err(OwnerSendError::Conflict) => existing::disposition(Disposition::Conflict),
            Err(OwnerSendError::Unknown) => Outcome::unknown(),
        }
    }
}

fn nudge_session_id(
    request: &ActionRequest,
    p: &SessionNudge,
) -> std::result::Result<Uuid, ApiError> {
    let id = existing::subject(request, "session", "session", &p.expected_generation)?;
    if request.operation_id != p.operation_id
        || p.project_id.is_nil()
        || p.body.trim().is_empty()
        || p.body.len() > 16384
        || generation(&p.expected_generation).is_none()
    {
        return Err(ApiError::invalid());
    }
    Ok(id)
}

#[async_trait]
impl ActionKind for NudgeSession {
    type Payload = SessionNudge;
    const KIND: &'static str = NUDGE_KIND;
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        p: &SessionNudge,
    ) -> std::result::Result<Scope, ApiError> {
        let session = nudge_session_id(request, p)?;
        existing::owner(state, user).await?;
        recipient(state, session, p.project_id, 1).await?;
        Ok(Scope::Project {
            project_id: p.project_id,
        })
    }
    async fn authorize_lookup(
        &self,
        state: &AppState,
        user: &User,
        scope: &Scope,
        operation: Option<&actions::Operation>,
    ) -> std::result::Result<(), ApiError> {
        SendSessionMessage
            .authorize_lookup(state, user, scope, operation)
            .await
    }
    async fn reconcile(
        &self,
        state: &AppState,
        user: &User,
        operation: &actions::Operation,
    ) -> std::result::Result<Option<Outcome>, ApiError> {
        SendSessionMessage.reconcile(state, user, operation).await
    }
    async fn execute(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        p: SessionNudge,
    ) -> Outcome {
        let (Ok(session), Some(expected)) = (
            nudge_session_id(request, &p),
            generation(&p.expected_generation),
        ) else {
            return Outcome::unknown();
        };
        let target = match recipient(state, session, p.project_id, expected).await {
            Ok(Live::Current(Recipient::Session {
                workspace_id,
                project_id,
                task_id,
                session_id,
                attempt_id,
                generation,
            })) => crate::stall::engine::Target {
                attempt_id,
                session_id,
                task_id,
                project_id,
                workspace_id,
                generation,
            },
            Ok(Live::Stale(live)) => {
                return Outcome::conflict([("session".to_string(), live.to_string())].into());
            }
            Ok(Live::Current(_)) => return Outcome::unknown(),
            Err(e) if e.status() == StatusCode::NOT_FOUND => {
                return existing::disposition(Disposition::Rejected);
            }
            Err(_) => return Outcome::unknown(),
        };
        let Ok(mut tx) = state.store.pool.begin().await else {
            return Outcome::unknown();
        };
        let sent = crate::stall::ports::nudge_as(
            &mut tx,
            &target,
            p.operation_id,
            &p.body,
            false,
            Some(user.id),
            now(state),
        )
        .await;
        match sent {
            Ok((_, response)) if tx.commit().await.is_ok() => result(&response),
            Ok(_) => Outcome::unknown(),
            Err(OwnerSendError::Invalid | OwnerSendError::NotFound) => {
                existing::disposition(Disposition::Rejected)
            }
            Err(OwnerSendError::Conflict) => existing::disposition(Disposition::Conflict),
            Err(OwnerSendError::Unknown) => Outcome::unknown(),
        }
    }
}
