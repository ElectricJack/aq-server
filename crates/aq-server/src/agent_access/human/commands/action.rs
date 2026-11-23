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
    existing::VersionedSubject {
        kind: "project",
        id: project,
        revision: payload.expected_project_revision.clone(),
        version_key: "project",
    }
    .request("task.create", payload.operation_id, json!(payload))
}
impl TaskCreate {
    fn owner<'a>(&'a self, state: &'a AppState, actor: &'a User) -> Owner<'a> {
        Owner {
            state,
            runtime: &self.runtime,
            actor,
        }
    }
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
        let payload = parse(&native.request).map_err(ApiError::from)?;
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
impl Operation {
    fn matches_payload(&self, project: Uuid, payload: &Create) -> bool {
        self.project == project
            && serde_json::from_slice::<Value>(&self.request).ok() == Some(json!(payload))
    }
}
impl Owner<'_> {
    async fn outcome(&self, native: &Operation) -> Outcome {
        journal::status(self.state, self.runtime, self.actor, native)
            .await
            .map(result)
            .unwrap_or_else(|_| Outcome::unknown())
    }
    async fn creation_failure(&self, id: Uuid, error: AppError) -> Outcome {
        match journal::load(self.runtime, id).await {
            Ok(Some(native)) => self.outcome(&native).await,
            Ok(None) => existing::failure(error),
            Err(_) => Outcome::unknown(),
        }
    }
    async fn verify_lookup(
        &self,
        project: Uuid,
        op: &actions::Operation,
    ) -> std::result::Result<(), ApiError> {
        verify_references(project, op)?;
        let native = journal::load(self.runtime, op.operation_id)
            .await
            .map_err(ApiError::from)?;
        let confirmed = op.disposition == Disposition::Confirmed;
        let Some(native) = native else {
            return if confirmed {
                Err(ApiError::invalid())
            } else {
                Ok(())
            };
        };
        journal::owns(&native, self.actor).map_err(ApiError::from)?;
        if native.project != project {
            return Err(ApiError::invalid());
        }
        let expected = journal::status(self.state, self.runtime, self.actor, &native)
            .await
            .map(result)
            .map_err(ApiError::from)?;
        let matches = op.receipt_refs == expected.receipt_refs
            && op.affected_entities == expected.affected_entities;
        if confirmed && !matches {
            return Err(ApiError::invalid());
        }
        Ok(())
    }
}
fn verify_references(project: Uuid, op: &actions::Operation) -> std::result::Result<(), ApiError> {
    let [subject] = op.subject_refs.as_slice() else {
        return Err(ApiError::invalid());
    };
    let project_matches = subject.entity_type == "project" && subject.id == project.to_string();
    let valid_tasks = op
        .affected_entities
        .iter()
        .all(|task| task.entity_type == "task" && task.id == op.operation_id.to_string());
    if !project_matches || !valid_tasks {
        return Err(ApiError::invalid());
    }
    Ok(())
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
    const GRANT: &'static str = aq_agent_grants::op::TASK_CREATE_SUBTASK;
    const TARGET: aq_command_registry::TargetRule = aq_command_registry::TargetRule::ParentTask {
        field: "parent_id",
        unparented: aq_agent_grants::op::TASK_CREATE_PROJECT,
    };
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        payload: &Create,
    ) -> std::result::Result<Scope, ApiError> {
        parse(&serde_json::to_vec(payload).map_err(|_| ApiError::invalid())?)
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
            self.owner(state, user)
                .authorize_parent(project, parent)
                .await
                .map_err(ApiError::from)?;
        }
        if let Some(role) = &payload.role {
            crate::role_settings::named_api(state, &[role]).await?;
        }
        if let Some(old) = journal::load(&self.runtime, request.operation_id)
            .await
            .map_err(ApiError::from)?
        {
            journal::owns(&old, user).map_err(ApiError::from)?;
            if !old.matches_payload(project, payload) {
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
            self.owner(state, user)
                .verify_lookup(*project_id, op)
                .await?;
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
                if journal::owns(&old, user).is_err() || !old.matches_payload(project, &payload) {
                    return existing::disposition(Disposition::Conflict);
                }
                return self.owner(state, user).outcome(&old).await;
            }
            Err(_) => return Outcome::unknown(),
            _ => {}
        }
        let bytes = self.original.clone().unwrap_or_else(|| {
            Bytes::from(serde_json::to_vec(&json!(payload)).expect("typed create"))
        });
        match self.owner(state, user).create(project, bytes, None).await {
            Ok(s) => result(s),
            Err(error) => {
                self.owner(state, user)
                    .creation_failure(request.operation_id, error)
                    .await
            }
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
