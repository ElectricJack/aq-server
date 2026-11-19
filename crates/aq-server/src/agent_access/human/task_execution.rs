//! Explicit retry over the B4 replacement owner.
use super::graph_actions::{admit, current, lookup};
use super::*;
use crate::dashboard_api::{
    actions::{self, ActionKind, ActionRequest, Outcome, Registry, existing},
    contract::Scope,
    errors::ApiError,
};
use crate::profiled_execution::replacement;
use aq_human_work::Execution;
use async_trait::async_trait;

pub(crate) struct TaskRetry(Arc<ReadRuntime>);
/// Recorded as the reason of an override whose retry stated none.
const NO_REASON: &str = "owner retry (no reason given)";

pub(crate) fn register(state: &AppState, r: &mut Registry) {
    let runtime = Arc::new(ReadRuntime::new(state));
    r.register(TaskRetry(runtime)).expect("task retry adapter");
}
pub(crate) fn schemas(r: &mut Registry) {
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
    // M2 (owner decision 2026-11-13): a new retry of a task AQ holds for what happened to it (a
    // quarantine, a changed project binding, ...) cannot take effect, so it is refused with the
    // hold and what the owner can do, before anything is admitted. A budget hold is not refused:
    // the retry is the owner's explicit decision and overrides it (owner decision 2026-11-18).
    // A replay of an admitted operation keeps its outcome.
    if !actions::journal::admitted(&state.store.pool, p.operation_id).await? {
        let task: Uuid = request
            .subject
            .id
            .parse()
            .map_err(|_| ApiError::invalid())?;
        let revision = crate::project_bindings::current(state, p.project_id).await?;
        let held = replacement::retry_hold(state, task, now(state)?, revision.revision).await?;
        if let Some(code) = held.filter(|c| !aq_machine_control::holds::is_budget_hold(c)) {
            let message = aq_machine_control::holds::retry_refusal(code);
            let status = axum::http::StatusCode::UNPROCESSABLE_ENTITY;
            return Err(ApiError::new(status, code, "").with_message(&message));
        }
    }
    Ok(Scope::Project {
        project_id: p.project_id,
    })
}
fn now(state: &AppState) -> std::result::Result<u64, AppError> {
    u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|n| *n > 0)
        .ok_or(AppError::Internal)
}
/// Fresh task read and version check, with the current project revision.
async fn fresh(
    state: &AppState,
    runtime: &ReadRuntime,
    user: &User,
    request: &ActionRequest,
    p: &Execution,
) -> Result<std::result::Result<(Uuid, Value, u64), Outcome>> {
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
    Ok(Ok((task, raw, a.record.revision)))
}
fn rejected() -> Outcome {
    existing::disposition(actions::Disposition::Rejected)
}

#[async_trait]
impl ActionKind for TaskRetry {
    type Payload = Execution;
    const KIND: &'static str = "task.retry";
    const GRANT: &'static str = "task.retry";
    const TARGET: aq_command_registry::TargetRule = aq_command_registry::TargetRule::Subject;
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
            let (task, raw, revision) = match fresh(state, &self.0, user, request, &p).await? {
                Ok(fresh) => fresh,
                Err(outcome) => return Ok(outcome),
            };
            if raw["task_state"] != "active" {
                return Ok(rejected());
            }
            // B4 decides admissibility, as the scheduler does: an unsettled predecessor is a
            // rejection, never a forced replacement. A budget hold alone is overridden: the
            // owner's reason is recorded, the budget starts again, and the assessment below is
            // the one the offer path makes next (never a confirmation it would refuse, M2).
            let now = now(state)?;
            let mut assessment =
                replacement::assess_task_for_revision(state, task, now, revision).await?;
            if let replacement::Assessment::Blocked(code) = assessment {
                let reason = request.reason.as_deref().map(str::trim);
                let o = aq_machine_control::holds::Override {
                    operation_id: p.operation_id,
                    attempt_id: Uuid::nil(),
                    hold_code: String::new(),
                    reason: reason.filter(|r| !r.is_empty()).unwrap_or(NO_REASON).into(),
                    actor: user.email.clone(),
                    recorded_at_ms: i64::try_from(now).map_err(|_| AppError::Internal)?,
                };
                let at = (p.project_id, task);
                assessment = replacement::override_budget(state, at, code, o, revision).await?;
            }
            // Review M-5: a refusal names its reason code on the receipt (spec 0004 item 10).
            let r = match &assessment {
                replacement::Assessment::Replace(r) => r,
                replacement::Assessment::Blocked(code) => return Ok(Outcome::refused(code)),
                replacement::Assessment::Fresh => {
                    return Ok(Outcome::refused("task_has_no_attempt"));
                }
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
