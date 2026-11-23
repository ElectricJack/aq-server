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
    if view.identity.node_id != auth.id()
        || view.identity.boot_id != q.original.identity.boot_id
        || view.offer_id != q.original.binding.offer_id
        || view.launch_manifest_digest.as_ref() != Some(&q.original.binding.launch_manifest_digest)
        || view.execution_spec_digest != q.original.binding.execution_spec_digest.as_str()
    {
        return Err(refused());
    }
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
        if collision {
            return Err(refused());
        }
        return Ok(None);
    }
    if rows.len() != 1
        || rows[0].get::<Uuid, _>("attempt_id") != q.original.binding.attempt_id
        || rows[0].get::<Uuid, _>("operation_id") != q.original.operation_id
    {
        return Err(refused());
    }
    let original = on_heap(|| {
        history::load_optional_locked(tx, auth.id(), &q.original.binding, rows[0].get("grant_id"))
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
    if policy::current(state, &original.accepted, &runtime)? != original.provenance.qualification {
        return Err(refused());
    }
    let current_registration = on_heap(|| {
        super::registration::admitted_runtime_locked(tx, auth.id(), q.original.identity.boot_id)
    })
    .await?;
    let registered =
        on_heap(|| super::closure::history::load_locked(tx, original.provenance.registration_id))
            .await?;
    if current_registration.semantic_digest != registered.metadata.semantic_digest
        || current_registration.request.identity != q.original.identity
    {
        return Err(refused());
    }
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
    if project.get::<i64, _>("revision") as u64 != original.accepted.proposal.spec.project_revision
        || !binding.enabled
        || !binding.eligible_node(&state.config.projects, auth.id())
    {
        return Err(refused());
    }
    require_same(&binding, &original.provenance.project.project.binding)?;
    if !enabled_replay_project(state, binding.project_id) {
        return Err(refused());
    }
    Ok(())
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
    if !view.holds_task
        || view.revoked
        || view.stopped
        || view.cancellation_id.is_some()
        || hot.status != Status::Granted
    {
        return Err(refused());
    }
    require_same(
        hot.grant.as_ref().ok_or_else(|| refused())?,
        &original.receipt.grant,
    )?;
    if hot.execution_deadline_ms != Some(original.receipt.execution_deadline_ms) {
        return Err(refused());
    }
    let Some(owner) = hot.owner.as_ref() else {
        return Err(refused());
    };
    if owner.guardian_id != q.original.guardian_id
        || owner.launch_id != q.original.launch_id
        || owner.reservation_id != q.original.reservation.reservation_id
    {
        return Err(refused());
    }
    Ok(())
}
