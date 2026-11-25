//! Explicit retry over the B4 replacement owner.
use super::graph_actions::{Claim, OwnerRead, current};
use super::*;
use crate::dashboard_api::{
    actions::{self, ActionKind, ActionRequest, Outcome, Registry, existing},
    contract::Scope,
    errors::ApiError,
};
use crate::profiled_execution::replacement;
use aq_human_work::TaskRetry as Retry;
use aq_machine_control::{holds, owner_retry};
use aq_profiled_state::replacement_sql as sql;
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
    OwnerRead {
        state,
        runtime,
        user,
    }
    .admit(
        request,
        Claim {
            operation: p.operation_id,
            project: p.project_id,
            entity: "task",
            expected: &p.expected_version,
            tasks: &[],
        },
    )
    .await?;
    // Admission refuses permanent holds. An admitted retry may wait for evidence.
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
    anchor: Option<&owner_retry::Anchor>,
) -> std::result::Result<Outcome, AppError> {
    use aq_machine_control::fresh;
    if !matches!(assessment, replacement::Assessment::Blocked(c) if c == fresh::HOLD) {
        return Ok(Outcome::refused("fresh_retry_not_applicable"));
    }
    let now = now(state)?;
    let replacement::Assessment::Replace(r) =
        replacement::fresh_predecessor(state, task, now).await?
    else {
        return Ok(rejected());
    };
    if anchor.is_some_and(|a| a.predecessor != Some(r.predecessor)) {
        return Ok(Outcome::conflict(Default::default()));
    }
    let owner_operation = anchor.map(|_| p.operation_id);
    let reason = request.reason.as_deref().map(str::trim);
    let f = fresh::FreshStart {
        operation_id: p.operation_id,
        reason: reason.filter(|r| !r.is_empty()).unwrap_or(NO_REASON).into(),
        actor: user.email.clone(),
        recorded_at_ms: i64::try_from(now).map_err(|_| AppError::Internal)?,
        project_revision: revision,
    };
    replacement::release_as(state, task, &r, Some(&f), owner_operation).await?;
    fresh::post(
        &state.store.pool,
        aq_task_comments::Thread {
            project_id: p.project_id,
            task_id: task,
        },
        &f,
        (state.clock)(),
    )
    .await;
    Ok(released(r.predecessor))
}

fn released(predecessor: Uuid) -> Outcome {
    Outcome::confirmed(
        vec![existing::entity("replacement_release", predecessor, None)],
        vec![existing::entity("execution_attempt", predecessor, None)],
    )
}
fn override_confirmed(task: Uuid) -> Outcome {
    Outcome::confirmed(vec![], vec![existing::entity("task", task, None)])
}
fn stale_or_overridden(task: Uuid, overridden: bool) -> Outcome {
    if overridden {
        override_confirmed(task)
    } else {
        Outcome::conflict(Default::default())
    }
}

pub async fn check_retry(state: &AppState, runtime: &ReadRuntime) -> Result<()> {
    let Some(due) = owner_retry::claim_due::<ActionRequest, actions::Operation>(
        &state.store.pool,
        (state.clock)(),
    )
    .await
    .map_err(crate::error::db)?
    else {
        return Ok(());
    };
    let Some(user) = state.store.find_user(due.principal_id).await? else {
        return Ok(());
    };
    let request = due.request;
    let original = due.response;
    let payload =
        serde_json::from_value(request.payload.clone()).map_err(|_| AppError::Internal)?;
    let outcome = execute(state, runtime, &user, &request, payload).await;
    actions::finish(state, &user, original, outcome)
        .await
        .map_err(existing::legacy_error)?;
    Ok(())
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
        OwnerRead {
            state,
            runtime: &self.0,
            user,
        }
        .lookup(scope, operation, "task")
        .await
    }
    async fn execute(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        p: Retry,
    ) -> Outcome {
        execute(state, &self.0, user, request, p).await
    }
}

/// `task.dependency.add` with `resume_held`: the subject's limit hold is checked before `edge`
/// is added and overridden after it, under the operation's id, so the task is never neither held
/// nor blocked. Its stopped attempt keeps the claim until the poll replaces it.
pub(super) async fn resume_held(
    (state, user, request): (&AppState, &User, &ActionRequest),
    (record, workspace, task, blocker): (&ProjectRecord, Uuid, Uuid, Uuid),
    edge: impl std::future::Future<Output = Result<Outcome>>,
) -> Result<Outcome> {
    let (project, pool) = (record.binding.project_id, &state.store.pool);
    let a = replacement::assess_task_for_revision(state, task, now(state)?, record.revision);
    let held = sql::resumable(pool, (project, task), &a.await?).await;
    let code = match held.map_err(|_| AppError::Internal)? {
        Ok(code) => code,
        Err(code) => return Ok(Outcome::refused(code)),
    };
    let outcome = edge.await?;
    if outcome.disposition != actions::Disposition::Confirmed {
        return Ok(outcome);
    }
    let why = request.reason.as_deref().map(str::trim);
    let why = why.filter(|r| !r.is_empty()).map(str::to_owned);
    let why = why.unwrap_or_else(|| format!("waits on task {blocker}"));
    let o = (request.operation_id, Uuid::nil(), why.as_str(), now(state)?);
    let at = (project, workspace, task, record.revision);
    let again = override_limit(state, user, o, at, code).await?;
    Ok(again.blocked().map_or(outcome, Outcome::refused))
}
/// Records the override of the limit hold `code` ([`replacement::override_budget`]) and assesses
/// the task again. It releases nothing: the stopped attempt keeps its claim until a replacement.
pub(super) async fn override_limit(
    state: &AppState,
    user: &User,
    (operation, attempt, reason, now): (Uuid, Uuid, &str, u64),
    (project, workspace, task, revision): (Uuid, Uuid, Uuid, u64),
    code: &'static str,
) -> Result<replacement::Assessment> {
    let lineage = aq_admission::transport::task_lineage_id(project, workspace, task);
    let spent = match lineage {
        Ok(l) => crate::observations::budget::lineage(state, l, None)
            .await
            .ok(),
        Err(_) => None,
    };
    let spent = spent.flatten().and_then(|b| b.snapshot().spent_microunits);
    let actor = (!user.email.is_empty()).then(|| user.email.clone());
    let o = holds::Override {
        operation_id: operation,
        attempt_id: attempt,
        hold_code: String::new(),
        reason: reason.into(),
        actor: actor.unwrap_or_else(|| format!("supervisor session {}", user.id)),
        recorded_at_ms: i64::try_from(now).map_err(|_| AppError::Internal)?,
        spent_microunits: spent.and_then(|s| i64::try_from(s).ok()),
    };
    replacement::override_budget(state, (project, task), code, o, revision).await
}

async fn execute(
    state: &AppState,
    runtime: &ReadRuntime,
    user: &User,
    request: &ActionRequest,
    p: Retry,
) -> Outcome {
    let run = async {
        let task = request.subject.id.parse().map_err(|_| invalid())?;
        let anchor = owner_retry::anchor(&state.store.pool, p.operation_id)
            .await
            .map_err(crate::error::db)?;
        if let Some(previous) = anchor
            .as_ref()
            .filter(|a| a.released)
            .and_then(|a| a.predecessor)
        {
            return Ok(released(previous));
        }
        let prior_override = anchor.as_ref().is_some_and(|a| a.overridden);
        let (task, raw, revision, workspace) = match fresh(state, runtime, user, request, &p).await
        {
            Ok(Ok(fresh)) => fresh,
            Ok(Err(_)) if prior_override => return Ok(override_confirmed(task)),
            Ok(Err(outcome)) => return Ok(outcome),
            Err(AppError::Forbidden(_)) if anchor.is_some() => return Ok(Outcome::unknown()),
            Err(error) => return Err(error),
        };
        if raw["task_state"] != "active" {
            return Ok(rejected());
        }
        if let Some(anchor) = &anchor
            && !anchor
                .matches(&state.store.pool, task, revision)
                .await
                .map_err(|_| AppError::Internal)?
        {
            return Ok(stale_or_overridden(task, prior_override));
        }
        let owner_operation = anchor.as_ref().map(|_| p.operation_id);
        // B4 checks stop, source and effects for the same predecessor as the scheduler.
        let now = now(state)?;
        let mut assessment =
            replacement::assess_task_for_revision(state, task, now, revision).await?;
        #[cfg(feature = "delivery_playbook")]
        if !p.fresh
            && matches!(
                assessment,
                replacement::Assessment::Blocked(
                    "replacement_effects_unreconciled" | "replacement_source_retention_unknown"
                )
            )
        {
            Box::pin(
                crate::profiled_execution::settlement::recover_held_for_retry_exact(
                    state,
                    task,
                    anchor.as_ref().and_then(|a| a.predecessor),
                ),
            )
            .await?;
            assessment = replacement::assess_task_for_revision(state, task, now, revision).await?;
        }
        if p.fresh {
            return fresh_start(
                state,
                user,
                request,
                &p,
                (task, revision),
                assessment,
                anchor.as_ref(),
            )
            .await;
        }
        let at = (p.project_id, task);
        let limit = sql::limit_hold(&state.store.pool, at, &assessment).await;
        let limit = limit.map_err(|_| AppError::Internal)?;
        if let Some(code) = limit {
            let reason = request.reason.as_deref().map(str::trim);
            let reason = reason.filter(|r| !r.is_empty()).unwrap_or(NO_REASON);
            let predecessor = anchor.as_ref().and_then(|a| a.predecessor);
            let o = (p.operation_id, predecessor.unwrap_or_default(), reason, now);
            let at = (p.project_id, workspace, task, revision);
            assessment = override_limit(state, user, o, at, code).await?;
        }
        // Review M-5: a refusal names its reason code on the receipt (spec 0004 item 10).
        let r = match &assessment {
            replacement::Assessment::Replace(r) => r,
            replacement::Assessment::Blocked(code) => {
                return Ok(
                    if owner_retry::wait_on(&state.store.pool, owner_operation, code)
                        .await
                        .map_err(crate::error::db)?
                    {
                        Outcome::pending()
                    } else {
                        Outcome::refused(code)
                    },
                );
            }
            replacement::Assessment::Fresh if limit.is_some() || prior_override => {
                return Ok(override_confirmed(task));
            }
            replacement::Assessment::Fresh => {
                return Ok(Outcome::refused("task_has_no_attempt"));
            }
        };
        if let Some(anchor) = &anchor
            && anchor.predecessor != Some(r.predecessor)
        {
            return Ok(stale_or_overridden(task, prior_override));
        }
        replacement::release_as(state, task, r, None, owner_operation).await?;
        Ok::<_, AppError>(released(r.predecessor))
    };
    run.await.unwrap_or_else(existing::failure)
}
