async fn lock_replay_authority(
    tx: &mut Transaction<'_, Postgres>,
    preparation: &PhasePreparation<'_>,
) -> Result<crate::execution::authority::AuthorityView, AppError> {
    use crate::execution::authority::{self, Policy};
    let PhasePreparation {
        state,
        auth,
        request: q,
        ..
    } = *preparation;
    crate::worker_api::lock_current_node(tx, auth).await?;
    let view = on_heap(|| {
        authority::load_locked(state, tx, &q.original.binding.session, Policy::IdentityOnly)
    })
    .await?;
    let live_identity = (view.identity.node_id, view.identity.boot_id, view.offer_id);
    let replay_identity = (
        auth.id(),
        q.original.identity.boot_id,
        q.original.binding.offer_id,
    );
    require_replay(live_identity == replay_identity)?;
    require_replay(
        view.launch_manifest_digest.as_ref() == Some(&q.original.binding.launch_manifest_digest),
    )?;
    require_replay(
        view.execution_spec_digest == q.original.binding.execution_spec_digest.as_str(),
    )?;
    on_heap(|| super::closure::ensure_launch_open_locked(tx, &q.original.binding)).await?;
    Ok(view)
}

async fn find_replay_grant(
    tx: &mut Transaction<'_, Postgres>,
    preparation: &PhasePreparation<'_>,
) -> Result<Option<history::PhaseExecutionHistory>, AppError> {
    use sqlx::Row;
    use uuid::Uuid;
    let PhasePreparation {
        auth, request: q, ..
    } = *preparation;
    let rows=sqlx::query("SELECT attempt_id,operation_id,grant_id FROM profiled_phase_execution_grants WHERE attempt_id=$1 OR(node_id=$2 AND operation_id=$3) ORDER BY attempt_id LIMIT 2 FOR SHARE").bind(q.original.binding.attempt_id).bind(auth.id()).bind(q.original.operation_id).fetch_all(&mut **tx).await.map_err(db)?;
    if rows.is_empty() {
        let collision:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM profiled_execution_receipts WHERE node_id=$1 AND operation_id=$2) OR EXISTS(SELECT 1 FROM profiled_v2_accepts WHERE node_id=$1 AND operation_id=$2)").bind(auth.id()).bind(q.original.operation_id).fetch_one(&mut **tx).await.map_err(db)?;
        require_replay(!collision)?;
        return Ok(None);
    }
    let [row] = rows.as_slice() else {
        return Err(refused());
    };
    require_replay(row.get::<Uuid, _>("attempt_id") == q.original.binding.attempt_id)?;
    require_replay(row.get::<Uuid, _>("operation_id") == q.original.operation_id)?;
    let original = on_heap(|| {
        history::load_optional_locked(tx, auth.id(), &q.original.binding, row.get("grant_id"))
    })
    .await?
    .ok_or_else(|| refused())?;
    require_same(&original.request, q)?;
    require_same(&original.installation, &q.original.materials[0])?;
    Ok(Some(original))
}

async fn verify_registered_replay(
    tx: &mut Transaction<'_, Postgres>,
    preparation: &PhasePreparation<'_>,
    original: &history::PhaseExecutionHistory,
) -> Result<(), AppError> {
    let PhasePreparation {
        state,
        auth,
        request: q,
        ..
    } = *preparation;
    let runtime = on_heap(|| {
        super::registration::current_runtime_locked(
            state,
            tx,
            auth.id(),
            q.original.identity.boot_id,
        )
    })
    .await?;
    require_replay(
        policy::current(state, &original.accepted, &runtime)? == original.provenance.qualification,
    )?;
    let current_registration = on_heap(|| {
        super::registration::admitted_runtime_locked(tx, auth.id(), q.original.identity.boot_id)
    })
    .await?;
    let registered =
        on_heap(|| super::closure::history::load_locked(tx, original.provenance.registration_id))
            .await?;
    require_replay(current_registration.semantic_digest == registered.metadata.semantic_digest)?;
    require_replay(current_registration.request.identity == q.original.identity)?;
    Ok(())
}

async fn fence_replay_project(
    tx: &mut Transaction<'_, Postgres>,
    preparation: &PhasePreparation<'_>,
    original: &history::PhaseExecutionHistory,
) -> Result<(), AppError> {
    use sqlx::Row;
    let PhasePreparation { state, auth, .. } = *preparation;
    let project = sqlx::query(
        "SELECT revision,binding FROM project_execution_bindings WHERE project_id=$1 FOR SHARE",
    )
    .bind(original.accepted.proposal.spec.lifecycle.project_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    let binding: crate::project_bindings::domain::ProjectBinding =
        serde_json::from_value(project.get("binding")).map_err(|_| refused())?;
    let current_revision = project.get::<i64, _>("revision") as u64;
    require_replay(current_revision == original.accepted.proposal.spec.project_revision)?;
    require_replay(binding.enabled && binding.eligible_node(&state.config.projects, auth.id()))?;
    require_same(&binding, &original.provenance.project.project.binding)?;
    require_replay(enabled_replay_project(state, binding.project_id))
}

fn enabled_replay_project(state: &AppState, project: uuid::Uuid) -> bool {
    state
        .config
        .profiled_execution
        .as_ref()
        .and_then(|config| {
            config
                .projects
                .iter()
                .find(|entry| entry.project_id == project)
        })
        .is_some_and(|entry| entry.enabled)
}

async fn fence_replay_hot_state(
    tx: &mut Transaction<'_, Postgres>,
    q: &phase::PrepareRequest,
    view: &crate::execution::authority::AuthorityView,
    original: &history::PhaseExecutionHistory,
) -> Result<(), AppError> {
    use crate::execution::{authority::hot::HotState, profiled::Status};
    let raw: serde_json::Value = sqlx::query_scalar(
        "SELECT state FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE",
    )
    .bind(q.original.binding.attempt_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    let hot: HotState = serde_json::from_value(raw).map_err(|_| refused())?;
    hot.validate_against(&q.original.binding)
        .map_err(|_| refused())?;
    require_replay(replay_attempt_active(view) && hot.status == Status::Granted)?;
    require_same(
        hot.grant.as_ref().ok_or_else(|| refused())?,
        &original.receipt.grant,
    )?;
    require_replay(hot.execution_deadline_ms == Some(original.receipt.execution_deadline_ms))?;
    let owner = hot.owner.as_ref().ok_or_else(|| refused())?;
    let live_owner = (owner.guardian_id, owner.launch_id, owner.reservation_id);
    let replay_owner = (
        q.original.guardian_id,
        q.original.launch_id,
        q.original.reservation.reservation_id,
    );
    require_replay(live_owner == replay_owner)
}

fn replay_attempt_active(view: &crate::execution::authority::AuthorityView) -> bool {
    view.holds_task && !view.revoked && !view.stopped && view.cancellation_id.is_none()
}

#[track_caller]
fn require_replay(condition: bool) -> Result<(), AppError> {
    if condition { Ok(()) } else { Err(refused()) }
}
