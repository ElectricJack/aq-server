//! Explicit retry over the B4 replacement owner.
use super::graph_actions::{admit, current, lookup};
use super::*;
use crate::dashboard_api::{
    actions::{self, ActionKind, ActionRequest, Outcome, Registry, existing},
    contract::Scope,
    errors::ApiError,
};
use crate::profiled_execution::replacement;
use aq_human_work::TaskRetry as Retry;
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
    p: &Retry,
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
    // A fresh retry (2026-11-19) is for a changed project binding alone. A replay of an
    // admitted operation keeps its outcome.
    if !actions::journal::admitted(&state.store.pool, p.operation_id).await? {
        let task: Uuid = request
            .subject
            .id
            .parse()
            .map_err(|_| ApiError::invalid())?;
        let revision = crate::project_bindings::current(state, p.project_id).await?;
        let held = replacement::retry_hold(state, task, now(state)?, revision.revision).await?;
        if let Some((code, message)) = aq_machine_control::fresh::refusal(p.fresh, held) {
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
    p: &Retry,
) -> Result<std::result::Result<(Uuid, Value, u64, Uuid), Outcome>> {
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
    let binding = &a.record.binding;
    Ok(Ok((
        task,
        raw,
        a.record.revision,
        binding.graph_workspace_id,
    )))
}
fn rejected() -> Outcome {
    existing::disposition(actions::Disposition::Rejected)
}
/// A fresh retry (owner decision 2026-11-19) of a task held because the project binding
/// changed: the held attempt is released as for a replacement, and the task's next attempt is a
/// first attempt from the project's current source. Its earlier results stay retained.
async fn fresh_start(
    state: &AppState,
    user: &User,
    request: &ActionRequest,
    p: &Retry,
    (task, revision): (Uuid, u64),
    assessment: replacement::Assessment,
) -> std::result::Result<Outcome, AppError> {
    use aq_machine_control::fresh;
    if !matches!(assessment, replacement::Assessment::Blocked(c) if c == fresh::HOLD) {
        return Ok(Outcome::refused("fresh_retry_not_applicable"));
    }
    let now = now(state)?;
    let replacement::Assessment::Replace(r) = replacement::assess_task(state, task, now).await?
    else {
        return Ok(rejected());
    };
    let reason = request.reason.as_deref().map(str::trim);
    let f = fresh::FreshStart {
        operation_id: p.operation_id,
        reason: reason.filter(|r| !r.is_empty()).unwrap_or(NO_REASON).into(),
        actor: user.email.clone(),
        recorded_at_ms: i64::try_from(now).map_err(|_| AppError::Internal)?,
        project_revision: revision,
    };
    replacement::release_as(state, task, &r, Some(&f)).await?;
    fresh::post(&state.store.pool, p.project_id, task, &f, (state.clock)()).await;
    Ok(Outcome::confirmed(
        vec![existing::entity("replacement_release", r.predecessor, None)],
        vec![existing::entity("execution_attempt", r.predecessor, None)],
    ))
}

#[async_trait]
impl ActionKind for TaskRetry {
    type Payload = Retry;
    const KIND: &'static str = "task.retry";
    const GRANT: &'static str = "task.retry";
    const TARGET: aq_command_registry::TargetRule = aq_command_registry::TargetRule::Subject;
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        p: &Retry,
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
        p: Retry,
    ) -> Outcome {
        let run = async {
            let (task, raw, revision, workspace) =
                match fresh(state, &self.0, user, request, &p).await? {
                    Ok(fresh) => fresh,
                    Err(outcome) => return Ok(outcome),
                };
            if raw["task_state"] != "active" {
                return Ok(rejected());
            }
            // B4 decides admissibility, as the scheduler does: an unsettled predecessor is a
            // rejection. A limit hold alone (the assessment's, or one selection recorded) is
            // overridden with the owner's reason; never a confirmation the offer path refuses.
            use aq_machine_control::holds;
            let now = now(state)?;
            let mut assessment =
                replacement::assess_task_for_revision(state, task, now, revision).await?;
            if p.fresh {
                return fresh_start(state, user, request, &p, (task, revision), assessment).await;
            }
            let recorded = holds::current(&state.store.pool, p.project_id, task).await;
            let recorded = recorded.map_err(|_| AppError::Internal)?;
            let limit = match assessment {
                replacement::Assessment::Blocked(code) => Some(code),
                _ => recorded.and_then(|h| holds::budget_code(&h.reason)),
            };
            if let Some(code) = limit {
                let reason = request.reason.as_deref().map(str::trim);
                let lineage =
                    aq_admission::transport::task_lineage_id(p.project_id, workspace, task);
                let spent = match lineage {
                    Ok(l) => crate::observations::budget::lineage(state, l, None)
                        .await
                        .ok(),
                    Err(_) => None,
                };
                let spent = spent.flatten().and_then(|b| b.snapshot().spent_microunits);
                let o = holds::Override {
                    operation_id: p.operation_id,
                    attempt_id: Uuid::nil(),
                    hold_code: String::new(),
                    reason: reason.filter(|r| !r.is_empty()).unwrap_or(NO_REASON).into(),
                    actor: user.email.clone(),
                    recorded_at_ms: i64::try_from(now).map_err(|_| AppError::Internal)?,
                    spent_microunits: spent.and_then(|s| i64::try_from(s).ok()),
                };
                let at = (p.project_id, task);
                assessment = replacement::override_budget(state, at, code, o, revision).await?;
            }
            // Review M-5: a refusal names its reason code on the receipt (spec 0004 item 10).
            let r = match &assessment {
                replacement::Assessment::Replace(r) => r,
                replacement::Assessment::Blocked(code) => return Ok(Outcome::refused(code)),
                replacement::Assessment::Fresh if limit.is_some() => {
                    let task = existing::entity("task", task, None);
                    return Ok(Outcome::confirmed(vec![], vec![task]));
                }
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
