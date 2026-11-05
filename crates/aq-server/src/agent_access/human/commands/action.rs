//! Action adapter over the existing fixed-key Graph operation journal.
use super::*;
use crate::dashboard_api::{
    actions::{self, ActionKind, ActionRequest, Disposition, Outcome, existing},
    contract::Scope,
    errors::ApiError,
};
use async_trait::async_trait;
pub(crate) struct TaskCreate {
    pub(crate) runtime: Arc<ReadRuntime>,
    pub(crate) original: Option<Bytes>,
}
pub(super) fn request(project: Uuid, payload: &Create) -> ActionRequest {
    existing::request(
        "task.create",
        payload.operation_id,
        "project",
        project,
        payload.expected_project_revision.clone(),
        json!(payload),
        "project",
    )
}
impl TaskCreate {
    pub(crate) async fn retained_outcome(
        &self,
        state: &AppState,
        user: &User,
        id: Uuid,
    ) -> std::result::Result<Option<Outcome>, ApiError> {
        let Some(op) = journal::load(&self.runtime, id)
            .await
            .map_err(ApiError::from)?
        else {
            return Ok(None);
        };
        journal::owns(&op, user).map_err(ApiError::from)?;
        existing::owner(state, user).await?;
        access(state, &self.runtime, user.clone(), op.project)
            .await
            .map_err(ApiError::from)?;
        journal::status(state, &self.runtime, user, &op)
            .await
            .map(result)
            .map(Some)
            .map_err(ApiError::from)
    }
    /// Explicit human recovery retains the original native key and dispatch guards.
    pub(crate) async fn recover(
        &self,
        state: &AppState,
        user: &User,
        id: Uuid,
    ) -> std::result::Result<Option<Outcome>, ApiError> {
        let Some(op) = journal::load(&self.runtime, id)
            .await
            .map_err(ApiError::from)?
        else {
            return Ok(None);
        };
        journal::owns(&op, user).map_err(ApiError::from)?;
        dispatch::run(state, &self.runtime, user, id)
            .await
            .map(result)
            .map(Some)
            .map_err(ApiError::from)
    }

    /// Recover pre-union admission metadata without re-entering create or dispatch.
    pub(crate) async fn retained_request(
        &self,
        state: &AppState,
        user: &User,
        id: Uuid,
    ) -> std::result::Result<Option<(ActionRequest, Scope, chrono::DateTime<chrono::Utc>)>, ApiError>
    {
        let Some(native) = journal::load(&self.runtime, id)
            .await
            .map_err(ApiError::from)?
        else {
            return Ok(None);
        };
        journal::owns(&native, user).map_err(ApiError::from)?;
        existing::owner(state, user).await?;
        access(state, &self.runtime, user.clone(), native.project)
            .await
            .map_err(ApiError::from)?;
        journal::status(state, &self.runtime, user, &native)
            .await
            .map_err(ApiError::from)?;
        let payload = Create::parse(&native.request).map_err(ApiError::from)?;
        let created = chrono::DateTime::from_timestamp_millis(native.created)
            .ok_or_else(ApiError::unavailable)?;
        Ok(Some((
            request(native.project, &payload),
            Scope::Project {
                project_id: native.project,
            },
            created,
        )))
    }
}
fn result(status: Status) -> Outcome {
    let disposition = match status.state {
        OperationState::Pending => Disposition::Pending,
        OperationState::Unknown => Disposition::Unknown,
        OperationState::Confirmed => Disposition::Confirmed,
        OperationState::Conflict => Disposition::Conflict,
        OperationState::Rejected => Disposition::Rejected,
    };
    if let Some(receipt) = status.receipt {
        return Outcome::confirmed(
            vec![existing::entity(
                "graph_change_set",
                receipt.change_set_id,
                Some(receipt.version),
            )],
            vec![existing::entity(
                "task",
                receipt.task_id,
                Some(receipt.content_revision),
            )],
        );
    }
    existing::disposition(disposition)
}
#[async_trait]
impl ActionKind for TaskCreate {
    type Payload = Create;
    const KIND: &'static str = "task.create";
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        payload: &Create,
    ) -> std::result::Result<Scope, ApiError> {
        Create::parse(&serde_json::to_vec(payload).map_err(|_| ApiError::invalid())?)
            .map_err(ApiError::from)?;
        if request.operation_id != payload.operation_id {
            return Err(ApiError::invalid());
        }
        let project = existing::subject(
            request,
            "project",
            "project",
            &payload.expected_project_revision,
        )?;
        existing::owner(state, user).await?;
        access(state, &self.runtime, user.clone(), project)
            .await
            .map_err(ApiError::from)?;
        if let Some(parent) = payload.parent_id {
            super::authorize_parent(state, &self.runtime, user, project, parent)
                .await
                .map_err(ApiError::from)?;
        }
        if let Some(old) = journal::load(&self.runtime, request.operation_id)
            .await
            .map_err(ApiError::from)?
        {
            journal::owns(&old, user).map_err(ApiError::from)?;
            if old.project != project
                || serde_json::from_slice::<Value>(&old.request).ok() != Some(json!(payload))
            {
                return Err(ApiError::for_status(StatusCode::CONFLICT));
            }
        }
        if !task_create_available(state) {
            return Err(ApiError::unavailable());
        }
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
    ) -> std::result::Result<(), ApiError> {
        existing::owner(state, user).await?;
        let Scope::Project { project_id } = scope else {
            return Err(ApiError::invalid());
        };
        access(state, &self.runtime, user.clone(), *project_id)
            .await
            .map_err(ApiError::from)?;
        if let Some(op) = operation {
            if op.subject_refs.len() != 1
                || op.subject_refs[0].entity_type != "project"
                || op.subject_refs[0].id != project_id.to_string()
                || op
                    .affected_entities
                    .iter()
                    .any(|r| r.entity_type != "task" || r.id != op.operation_id.to_string())
            {
                return Err(ApiError::invalid());
            }
            if let Some(native) = journal::load(&self.runtime, op.operation_id)
                .await
                .map_err(ApiError::from)?
            {
                journal::owns(&native, user).map_err(ApiError::from)?;
                if native.project != *project_id {
                    return Err(ApiError::invalid());
                }
                let expected = result(
                    journal::status(state, &self.runtime, user, &native)
                        .await
                        .map_err(ApiError::from)?,
                );
                if op.disposition == Disposition::Confirmed
                    && (op.receipt_refs != expected.receipt_refs
                        || op.affected_entities != expected.affected_entities)
                {
                    return Err(ApiError::invalid());
                }
            } else if op.disposition == Disposition::Confirmed {
                return Err(ApiError::invalid());
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
        let Ok(project) = existing::subject(
            request,
            "project",
            "project",
            &payload.expected_project_revision,
        ) else {
            return Outcome::unknown();
        };
        match journal::load(&self.runtime, request.operation_id).await {
            Ok(Some(old)) => {
                if journal::owns(&old, user).is_err()
                    || old.project != project
                    || serde_json::from_slice::<Value>(&old.request).ok() != Some(json!(payload))
                {
                    return existing::disposition(Disposition::Conflict);
                }
                return match journal::status(state, &self.runtime, user, &old).await {
                    Ok(s) => result(s),
                    Err(_) => Outcome::unknown(),
                };
            }
            Err(_) => return Outcome::unknown(),
            _ => {}
        }
        let bytes = self.original.clone().unwrap_or_else(|| {
            Bytes::from(serde_json::to_vec(&json!(payload)).expect("typed create"))
        });
        match create_domain(state, &self.runtime, user, project, bytes).await {
            Ok(s) => result(s),
            Err(error) => match journal::load(&self.runtime, request.operation_id).await {
                Ok(Some(native)) => {
                    match journal::status(state, &self.runtime, user, &native).await {
                        Ok(status) => result(status),
                        Err(_) => Outcome::unknown(),
                    }
                }
                Ok(None) => existing::failure(error),
                Err(_) => Outcome::unknown(),
            },
        }
    }
    async fn reconcile(
        &self,
        state: &AppState,
        user: &User,
        operation: &actions::Operation,
    ) -> std::result::Result<Option<Outcome>, ApiError> {
        let Some(old) = journal::load(&self.runtime, operation.operation_id)
            .await
            .map_err(ApiError::from)?
        else {
            return Ok(None);
        };
        journal::owns(&old, user).map_err(ApiError::from)?;
        Ok(Some(result(
            journal::status(state, &self.runtime, user, &old)
                .await
                .map_err(ApiError::from)?,
        )))
    }
}
