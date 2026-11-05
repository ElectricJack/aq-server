//! Dashboard cancel and retry over the B3 cancellation and B4 replacement owners.
//!
//! Neither kind writes task lifecycle: cancel records a B3 native cancellation
//! for the attempt holding the task, and retry records the B4 replacement
//! release that lets the scheduler start the replacement attempt.
use super::graph_actions::{admit, current, lookup};
use super::*;
use crate::dashboard_api::{
    actions::{self, ActionKind, ActionRequest, Disposition, Outcome, Registry, existing},
    contract::Scope,
    errors::ApiError,
};
use crate::profiled_execution::{cancellation, replacement};
use async_trait::async_trait;
use schemars::JsonSchema;

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct Execution {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
}

pub(crate) struct TaskCancel(Arc<ReadRuntime>);
pub(crate) struct TaskRetry(Arc<ReadRuntime>);

pub(crate) fn register(state: &AppState, r: &mut Registry) {
    let runtime = Arc::new(ReadRuntime::new(state));
    r.register(TaskCancel(runtime.clone()))
        .expect("task cancel adapter");
    r.register(TaskRetry(runtime)).expect("task retry adapter");
}
pub(crate) fn schemas(r: &mut Registry) {
    r.register_schema::<TaskCancel>()
        .expect("task cancel schema");
    r.register_schema::<TaskRetry>().expect("task retry schema");
}

async fn authorize(
    state: &AppState,
    runtime: &ReadRuntime,
    user: &User,
    request: &ActionRequest,
    p: &Execution,
) -> std::result::Result<Scope, ApiError> {
    admit(
        state,
        runtime,
        user,
        request,
        p.operation_id,
        p.project_id,
        "task",
        &p.expected_version,
        &[],
    )
    .await?;
    Ok(Scope::Project {
        project_id: p.project_id,
    })
}
/// Fresh task read and version check shared by both kinds.
async fn fresh(
    state: &AppState,
    runtime: &ReadRuntime,
    user: &User,
    request: &ActionRequest,
    p: &Execution,
) -> Result<std::result::Result<(Uuid, Value), Outcome>> {
    let task: Uuid = request.subject.id.parse().map_err(|_| invalid())?;
    let expected = graph_actions::version(&p.expected_version).ok_or_else(invalid)?;
    let a = access(state, runtime, user.clone(), p.project_id).await?;
    let raw = current(
        super::runtime(state)?,
        a.record.binding.graph_workspace_id,
        task,
        "task",
    )
    .await?;
    let live = raw["version"].as_u64().ok_or_else(unavailable)?;
    if live != expected {
        return Ok(Err(Outcome::conflict(
            [("task".to_string(), live.to_string())].into(),
        )));
    }
    Ok(Ok((task, raw)))
}
fn rejected() -> Outcome {
    existing::disposition(Disposition::Rejected)
}

#[async_trait]
impl ActionKind for TaskCancel {
    type Payload = Execution;
    const KIND: &'static str = "task.cancel";
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        p: &Execution,
    ) -> std::result::Result<Scope, ApiError> {
        // B3 records the human reason on the cancellation receipt.
        if request
            .reason
            .as_deref()
            .is_none_or(|r| r.trim().is_empty())
        {
            return Err(ApiError::invalid());
        }
        authorize(state, &self.0, user, request, p).await
    }
    async fn authorize_lookup(
        &self,
        state: &AppState,
        user: &User,
        scope: &Scope,
        operation: Option<&actions::Operation>,
    ) -> std::result::Result<(), ApiError> {
        lookup(state, &self.0, user, scope, operation, "task").await
    }
    async fn execute(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        p: Execution,
    ) -> Outcome {
        let run = async {
            let (task, _) = match fresh(state, &self.0, user, request, &p).await? {
                Ok(fresh) => fresh,
                Err(outcome) => return Ok(outcome),
            };
            let attempt: Option<Uuid> = sqlx::query_scalar(
                "SELECT attempt_id FROM execution_attempts WHERE task_id=$1 AND kind='profiled_v1' AND holds_task",
            )
            .bind(task)
            .fetch_optional(&state.store.pool)
            .await
            .map_err(|_| AppError::Internal)?;
            let Some(attempt) = attempt else {
                // Nothing is running; Graph task lifecycle stays with its owner.
                return Ok(rejected());
            };
            let reason = request.reason.as_deref().unwrap_or_default();
            let receipt = cancellation::cancel(state, attempt, p.operation_id, reason).await?;
            Ok::<_, AppError>(Outcome::confirmed(
                vec![existing::entity(
                    "native_cancellation",
                    receipt.operation_id,
                    Some(receipt.authority_revision.to_string()),
                )],
                vec![existing::entity("execution_attempt", attempt, None)],
            ))
        };
        run.await.unwrap_or_else(existing::failure)
    }
}

#[async_trait]
impl ActionKind for TaskRetry {
    type Payload = Execution;
    const KIND: &'static str = "task.retry";
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        p: &Execution,
    ) -> std::result::Result<Scope, ApiError> {
        authorize(state, &self.0, user, request, p).await
    }
    async fn authorize_lookup(
        &self,
        state: &AppState,
        user: &User,
        scope: &Scope,
        operation: Option<&actions::Operation>,
    ) -> std::result::Result<(), ApiError> {
        lookup(state, &self.0, user, scope, operation, "task").await
    }
    async fn execute(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        p: Execution,
    ) -> Outcome {
        let run = async {
            let (task, raw) = match fresh(state, &self.0, user, request, &p).await? {
                Ok(fresh) => fresh,
                Err(outcome) => return Ok(outcome),
            };
            if raw["task_state"] != "active" {
                return Ok(rejected());
            }
            let now = u64::try_from((state.clock)().timestamp_millis())
                .ok()
                .filter(|n| *n > 0)
                .ok_or(AppError::Internal)?;
            // B4 alone decides admissibility; an unsettled or exhausted
            // predecessor is a rejection, never a forced replacement.
            let assessment = replacement::assess_task(state, task, now).await?;
            let Some(r) = assessment.replacement() else {
                return Ok(rejected());
            };
            replacement::release(state, task, r).await?;
            Ok::<_, AppError>(Outcome::confirmed(
                vec![existing::entity("replacement_release", r.predecessor, None)],
                vec![existing::entity("execution_attempt", r.predecessor, None)],
            ))
        };
        run.await.unwrap_or_else(existing::failure)
    }
}
