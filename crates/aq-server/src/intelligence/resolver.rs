//! Trusted snapshot adapter. No network handler accepts these facts as launch
//! authority. AQ-08 supplies authenticated task/context and owns actual sealing.
use super::Result;
use super::*;
pub(crate) use aq_project_policy::decision::relevant;
pub use aq_project_policy::decision::{Decision, ReplacementBudget, ResolveRequest};
use aq_selection::*;
mod budget;
mod evidence;
mod inputs;
mod publication;
mod reservation;
mod snapshot;
pub(crate) use reservation::Reservation;
use snapshot::fence_snapshot;

/// Internal authenticated-caller service, never a grant. Task/context arguments
/// must be obtained from their real owners. Replays retain their original result;
/// new observations need a new operation ID. No provider calls occur here.
pub async fn resolve_trusted(
    state: &AppState,
    actor: Uuid,
    request: ResolveRequest,
    snapshot: &aq_profiles::roles::Snapshot,
) -> Result<Decision> {
    let subject = principal::Principal::human(actor)?;
    resolve_with_principal(state, subject, request, snapshot).await
}
/// Scheduled selection derives its principal only from this daemon's configured
/// project policy. No user identity or worker-selected policy enters this path.
pub async fn resolve_for_project_policy(
    state: &AppState,
    request: ResolveRequest,
    snapshot: &aq_profiles::roles::Snapshot,
) -> Result<Decision> {
    let subject = principal::Principal::configured(state, request.lineage.project_id)?;
    resolve_with_principal(state, subject, request, snapshot).await
}
/// `snapshot` is the role snapshot the request's frozen binding names; the
/// candidates and the selection policy are resolved from it alone.
async fn resolve_with_principal(
    state: &AppState,
    subject: principal::Principal,
    request: ResolveRequest,
    snapshot: &aq_profiles::roles::Snapshot,
) -> Result<Decision> {
    let launch = request.launch(snapshot).map_err(bad)?;
    let publication = publication::Publication::new(state, subject, &request)?;
    if let Some(decision) = publication.replay().await? {
        return Ok(decision);
    }
    let (project, input) = inputs::collect(state, request, snapshot.digest()).await?;
    let mut outcome =
        select_for_offer(&launch, &input).map_err(|_| bad("selection snapshot refused"))?;
    check_capacity(state, &input, &mut outcome).await?;
    publication
        .publish(Decision {
            decision_id: Uuid::new_v4(),
            project,
            inputs: input,
            outcome,
        })
        .await
}
async fn check_capacity(
    state: &AppState,
    input: &SelectionInputs,
    outcome: &mut SelectionOutcome,
) -> Result<()> {
    // Route from task requirements first. Capacity can wait on this exact class,
    // and never influences the router's model or class selection.
    if let Some(resolved) = &outcome.resolved {
        let mut capacity = state.store.pool.begin().await.map_err(db)?;
        let cell = Some(capacity_cell(input, &resolved.choice));
        let reason = aq_swarm_settings::admission::observe(
            &mut capacity,
            input.project.lineage.task_id,
            cell.as_ref(),
            input.now_ms as i64,
        )
        .await
        .map_err(db)?;
        capacity.commit().await.map_err(db)?;
        if let Some(reason) = reason {
            outcome.resolved = None;
            outcome.reason_codes = vec![reason.into()];
        }
    }
    Ok(())
}

/// The swarm capacity cell of a choice: its provider at the level it runs on.
/// A level's id is the intelligence class the swarm limits count.
fn capacity_cell(
    inputs: &SelectionInputs,
    choice: &ConcreteChoice,
) -> aq_swarm_settings::admission::Cell {
    aq_swarm_settings::admission::Cell {
        provider: choice.provider.clone(),
        intelligence_class: inputs.project.intelligence_profile.clone(),
    }
}
/// AQ-08 must call inside its SERIALIZABLE offer transaction, after resolving
/// current task/context authority, and retain all locks through atomic offer +
/// frozen selection insertion. Retry serialization failures; never do external
/// I/O here. This only checks facts, never inserts an attempt or grants execution.
pub async fn fence_for_offer(
    state: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    decision: &Decision,
) -> Result<()> {
    fence_role(state, decision).await?;
    fence_retained_decision(state, tx, decision, None).await
}
/// Spec 0014 §5.1: a disabled role is handed no new work. An offer made
/// before its role was disabled (or removed) is not granted; the task is then
/// held by its role. A change of the active snapshot alone refuses nothing:
/// the offer stays on the snapshot it was resolved from.
async fn fence_role(state: &AppState, decision: &Decision) -> Result<()> {
    if !crate::role_settings::tables(state) {
        return Ok(());
    }
    let active = crate::role_settings::active(&state.store.pool).await?;
    let role = active.as_ref().and_then(|s| s.role(&decision.project.role));
    match role {
        Some(role) if role.enabled => Ok(()),
        Some(_) => Err(conflict("role_disabled")),
        None => Err(conflict("role_missing")),
    }
}
/// Only the current locked, never-granted native reservation may be discounted
/// when comparing its original pre-offer decision. No durable count is changed.
pub(crate) async fn fence_for_preparation(
    state: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    decision: &Decision,
    binding: &aq_session_protocol::LaunchBinding,
) -> Result<()> {
    fence_role(state, decision).await?;
    Reservation::ungranted(binding)
        .fence(state, tx, decision)
        .await
}
async fn fence_retained_decision(
    state: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    decision: &Decision,
    owned_attempt: Option<Uuid>,
) -> Result<()> {
    let stored: Value = sqlx::query_scalar(
        "SELECT decision FROM intelligence_decisions WHERE decision_id=$1 FOR SHARE",
    )
    .bind(decision.decision_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?
    .ok_or_else(|| conflict("decision was never retained"))?;
    if digest(&stored).map_err(|_| AppError::Internal)?
        != digest(decision).map_err(|_| AppError::Internal)?
    {
        return Err(conflict(
            "decision differs from immutable retained artifact",
        ));
    }
    let isolation: String = sqlx::query_scalar("SHOW transaction_isolation")
        .fetch_one(&mut **tx)
        .await
        .map_err(db)?;
    if isolation != "serializable" {
        return Err(conflict(
            "profiled admission requires serializable retry/budget predicate fencing",
        ));
    }
    if decision.outcome.resolved.is_none() {
        return Err(conflict("decision has no eligible selection"));
    }
    fence_snapshot(state, tx, decision, owned_attempt.is_some()).await?;
    // C4 placement is re-derived here, not only at selection: a node that
    // reached a ceiling or lost its template since selection is refused.
    crate::profiled_execution::placement::fence_locked(
        state,
        tx,
        decision,
        owned_attempt,
        now(state)?,
    )
    .await?;
    if owned_attempt.is_none() {
        let choice = &decision.outcome.resolved.as_ref().unwrap().choice;
        let cell = Some(capacity_cell(&decision.inputs, choice));
        if let Some(reason) = aq_swarm_settings::admission::check(tx, cell.as_ref())
            .await
            .map_err(db)?
        {
            return Err(conflict(reason));
        }
        aq_swarm_settings::admission::clear_waiting(tx, decision.inputs.project.lineage.task_id)
            .await
            .map_err(db)?;
    }
    principal::fence(state, tx, decision).await?;
    check_time(state, decision)
}
pub(crate) fn check_time(state: &AppState, decision: &Decision) -> Result<()> {
    let time = now(state)?;
    if !decision.valid_at(time) {
        return Err(conflict("selection expired during admission locks"));
    }
    Ok(())
}
/// Current principal fence for already accepted no-launch input retrieval only.
/// Does not renew snapshots, select a candidate or authorize execution.
pub(crate) async fn fence_preparation_input_principal(
    state: &AppState,
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    attempt: Uuid,
) -> Result<()> {
    let row=sqlx::query("SELECT d.decision_id,d.decision#>>'{inputs,project,lineage,project_id}' AS project,l.launch_manifest#>>'{bindings,project_id}' AS bound_project FROM profiled_launches l JOIN intelligence_decisions d ON d.decision_id=l.decision_id WHERE l.attempt_id=$1 FOR SHARE OF l,d").bind(attempt).fetch_optional(&mut **tx).await.map_err(db)?.ok_or_else(||conflict("preparation principal missing"))?;
    let project: Uuid = row
        .get::<String, _>("project")
        .parse()
        .map_err(|_| AppError::Internal)?;
    let bound: Uuid = row
        .get::<String, _>("bound_project")
        .parse()
        .map_err(|_| AppError::Internal)?;
    principal::fence_subject(state, tx, row.get("decision_id"), project, bound).await
}
