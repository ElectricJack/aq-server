//! First bounded native profile: one worker slot and one immutable-base task.
//! It uses installed owner facts and never calls a model provider.
use super::{
    artifacts::{RetainedSet, canonical},
    build, compose, now, publication, refused,
};
use crate::{
    AppState,
    error::AppError,
    intelligence::resolver::{self, ResolveRequest},
    profiled_execution::{
        artifacts::OwnerKind, context_budget, registration, source, source_exports,
    },
    source_retention::profiled_base,
    worker_api::AuthenticatedNode,
};
use aq_admission::{
    ArtifactKind, SourceSelection,
    control::NativePollPage,
    transport::{Poll, TransportDocument},
};
use aq_selection::{CapabilitySnapshot, ExecutionTarget, TaskLineage};
use sqlx::Row;
use std::sync::OnceLock;
use tokio::sync::Semaphore;
use uuid::Uuid;
fn db(_: sqlx::Error) -> AppError {
    AppError::Internal
}
async fn runtime(
    state: &AppState,
    auth: &AuthenticatedNode,
    request: &Poll,
) -> Result<(registration::RegisteredRuntime, CapabilitySnapshot), AppError> {
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    crate::worker_api::lock_current_node(&mut tx, auth).await?;
    if auth.id() != request.identity.node_id {
        return Err(refused());
    }
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?;
    if version.parse::<u32>().map_err(|_| refused())? < 27 {
        return Err(refused());
    }
    let runtime =
        registration::current_runtime_locked(state, &mut tx, auth.id(), request.identity.boot_id)
            .await?;
    let admitted =
        registration::admitted_runtime_locked(&mut tx, auth.id(), request.identity.boot_id).await?;
    let qualified = state
        .config
        .profiled_execution
        .as_ref()
        .ok_or_else(refused)?
        .qualified(&admitted.request)
        .map_err(|_| refused())?;
    for feature in [
        aq_admission::transport::Feature::ArtifactTransferV1,
        aq_admission::transport::Feature::ServerMaterialV1,
        aq_admission::transport::Feature::DurableHostJournalV1,
    ] {
        if !admitted.request.features.contains(&feature) || !qualified.features.contains(&feature) {
            return Err(refused());
        }
    }
    for cap in [
        "aq.profiled-launch",
        "aq.terminal",
        "aq.harness.startup-context",
        "aq.harness.confined-worktree",
        "aq.harness.stop",
    ] {
        if !admitted
            .request
            .capabilities
            .iter()
            .any(|c| c.id.as_str() == cap && c.version == 1)
            || !qualified
                .capabilities
                .iter()
                .any(|c| c.id.as_str() == cap && c.version == 1)
        {
            return Err(refused());
        }
    }
    let row=sqlx::query("SELECT boot_id,sequence,snapshot FROM intelligence_node_snapshots WHERE node_id=$1 FOR SHARE").bind(auth.id()).fetch_optional(&mut *tx).await.map_err(db)?.ok_or_else(refused)?;
    let snapshot: CapabilitySnapshot =
        serde_json::from_value(row.get("snapshot")).map_err(|_| refused())?;
    if row.get::<Uuid, _>("boot_id") != request.identity.boot_id
        || row.get::<i64, _>("sequence") as u64 != request.inventory_sequence
        || snapshot.target
            != (ExecutionTarget::Worker {
                node_id: auth.id(),
                boot_id: request.identity.boot_id,
            })
        || now(state)? >= snapshot.expires_at_ms
    {
        return Err(refused());
    }
    tx.commit().await.map_err(db)?;
    Ok((runtime, snapshot))
}
async fn reservation(
    state: &AppState,
    compiled: &compose::Compiled,
    operation: Uuid,
) -> Result<source_exports::Reservation, AppError> {
    let project = compiled.frozen.project.binding.project_id;
    if let Some(old) = source_exports::recover_existing(state, operation, project).await? {
        return Ok(old);
    }
    let config = state
        .config
        .profiled_execution
        .as_ref()
        .ok_or_else(refused)?;
    let policy = config
        .project_policy_record(project)
        .map_err(|_| refused())?;
    if !matches!(policy.source, SourceSelection::Immutable { .. }) {
        return Err(refused());
    }
    let observed = source::observe(state, &compiled.frozen, &policy).await?;
    // Original proposal remains Context. Final Source owner publication is a
    // separate receipt of the actual admission observation, not the Git bundle.
    let mut proposals = RetainedSet::new(
        project,
        now(state)?
            .checked_add(7 * 86_400_000)
            .ok_or_else(refused)?,
    );
    proposals
        .retain(
            state,
            OwnerKind::SourceRetention,
            ArtifactKind::Context,
            observed.artifact.clone(),
            &observed.bytes,
        )
        .await?;
    let binding = &compiled.frozen.project.binding;
    let identity = source::repository_identity(
        state,
        binding.repository_id,
        binding.repository_binding_generation,
    )
    .await?;
    source_exports::reserve(
        state,
        source_exports::Request {
            schema_version: 1,
            operation_id: operation,
            project_id: project,
            project_revision: compiled.frozen.project.revision,
            repository_id: binding.repository_id,
            logical_identity: identity,
            binding_generation: binding.repository_binding_generation,
            base: aq_worker_protocol::ObjectId {
                algorithm: aq_worker_protocol::ObjectAlgorithm::Sha1,
                hex: observed.admission.resolved_object.hex.clone(),
            },
            source_retention_handle: observed.retention_handle,
            source_observation: observed.artifact,
        },
    )
    .await
}
async fn selection(
    state: &AppState,
    task: &aq_admission::TaskRevision,
    compiled: &compose::Compiled,
) -> Result<resolver::Decision, AppError> {
    resolver::resolve_for_project_policy(
        state,
        ResolveRequest {
            operation_id: Uuid::new_v4(),
            lineage: TaskLineage {
                project_id: compiled.frozen.project.binding.project_id,
                workspace_id: task.workspace_id,
                task_id: task.task_id,
            },
            project: compiled.frozen.clone(),
            compositions: compiled.candidates.clone(),
        },
    )
    .await
}
/// Authenticated daemon-owned scheduling entry. The public request carries only
/// this worker's inventory revision/capacity, never task/composition/source facts.
pub async fn poll(
    state: &AppState,
    auth: &AuthenticatedNode,
    request: &Poll,
) -> Result<NativePollPage, AppError> {
    static ASSEMBLERS: OnceLock<Semaphore> = OnceLock::new();
    let _permit = ASSEMBLERS
        .get_or_init(|| Semaphore::new(4))
        .try_acquire()
        .map_err(|_| refused())?;
    TransportDocument::Poll(request.clone())
        .validate()
        .map_err(|_| AppError::BadRequest("invalid native poll".into()))?;
    let (runtime, snapshot) = runtime(state, auth, request).await?;
    let mut page = NativePollPage {
        schema_version: 1,
        operation_id: request.operation_id,
        identity: request.identity.clone(),
        server_time_ms: now(state)?,
        offers: vec![],
        cancellations: vec![],
        next_after: None,
    };
    if request.available.slots == 0 {
        return Ok(page);
    }
    // Redelivery precedes new scheduling. Preparing/granted rows are not offers;
    // those require their independently fenced control/reconciliation owners.
    if let Some(native) = super::redelivery::existing(state, auth, request).await? {
        page.server_time_ms = native.offered.clock.server_time_ms;
        page.offers.push(native);
        page.validate_for(request).map_err(|_| refused())?;
        return Ok(page);
    }
    let busy: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM execution_attempts WHERE node_id=$1 AND holds_task)",
    )
    .bind(auth.id())
    .fetch_one(&state.store.pool)
    .await
    .map_err(db)?;
    if busy {
        return Ok(page);
    }
    let config = state
        .config
        .profiled_execution
        .as_ref()
        .ok_or_else(refused)?;
    let mut projects = config
        .projects
        .iter()
        .filter(|p| p.enabled)
        .collect::<Vec<_>>();
    projects.sort_by_key(|p| (std::cmp::Reverse(p.priority), p.project_id));
    let Some(policy) = projects.first() else {
        return Ok(page);
    };
    // Conservative finite first profile: exactly one configured candidate project
    // per poll. Later fairness is a scheduler policy, not an invented task label.
    if !matches!(policy.source, SourceSelection::Immutable { .. }) {
        return Err(refused());
    }
    let record = crate::project_bindings::current(state, policy.project_id).await?;
    let graph = state.agent_access.as_ref().ok_or_else(refused)?;
    let ready = graph
        .scheduling_ready(record.binding.graph_workspace_id, 8, None)
        .await
        .map_err(|_| refused())?;
    let Some(task) = ready.items.first().map(|t| t.revision.clone()) else {
        return Ok(page);
    };
    let counts = super::super::no_grant::attempt_counts(&state.store.pool, task.task_id)
        .await?
        .check_retry_bound()?;
    if counts.started != 0 {
        return Err(refused());
    }
    let generation =
        super::super::no_grant::next_generation(&state.store.pool, task.task_id).await?;
    let context = graph
        .scheduling_context(&task, 1, 64, 1_048_576)
        .await
        .map_err(|_| refused())?;
    let compiled = compose::compile(state, &record, auth.id(), &snapshot, &context).await?;
    super::observation::ReadProvenance::capture(
        &task,
        &ready,
        &context,
        compiled.projection.artifact.digest.clone(),
        now(state)?,
    )?;
    let original_ready_node = canonical(
        &ready
            .items
            .iter()
            .find(|r| r.revision == task)
            .ok_or_else(refused)?
            .node,
    )?;
    let decision = selection(state, &task, &compiled).await?;
    let selected = decision.outcome.resolved.as_ref().ok_or_else(refused)?;
    let composition = compiled
        .candidates
        .iter()
        .find(|c| c.digest == selected.choice.composition_digest)
        .ok_or_else(refused)?;
    // Qualification and exact startup counting precede expensive source export.
    context_budget::evaluate(
        &config.context_qualifications,
        &runtime,
        selected,
        &composition.composition,
        now(state)?,
    )
    .map_err(|_| refused())?;
    let source_operation = Uuid::new_v5(
        &Uuid::from_bytes(*b"aq-native-export"),
        &canonical(&(
            policy.project_id,
            &task,
            record.revision,
            generation,
            config
                .project_policy_ref(policy.project_id)
                .map_err(|_| refused())?,
        ))?,
    );
    let reserved = reservation(state, &compiled, source_operation).await?;
    let (token, fact) = match reserved {
        source_exports::Reservation::Pending(token) => {
            let fact = profiled_base::verify_export(state, &token).await?;
            source_exports::finalize(state, &token, &fact).await?;
            (token, fact)
        }
        source_exports::Reservation::Verified {
            reservation,
            canonical_verified,
        } => {
            let fact =
                profiled_base::VerifiedExport::decode_stored(&reservation, &canonical_verified)?;
            (reservation, fact)
        }
    };
    // External export can consume the original graph/selection freshness. Read
    // actual owners again; a task CAS/content change refuses this original task.
    let ready = graph
        .scheduling_ready(record.binding.graph_workspace_id, 8, None)
        .await
        .map_err(|_| refused())?;
    if canonical(
        &ready
            .items
            .iter()
            .find(|r| r.revision == task)
            .ok_or_else(refused)?
            .node,
    )? != original_ready_node
    {
        return Err(refused());
    }
    let context = graph
        .scheduling_context(&task, 1, 64, 1_048_576)
        .await
        .map_err(|_| refused())?;
    let compiled = compose::compile(state, &record, auth.id(), &snapshot, &context).await?;
    let decision = selection(state, &task, &compiled).await?;
    let source = source::observe(
        state,
        &compiled.frozen,
        &config
            .project_policy_record(policy.project_id)
            .map_err(|_| refused())?,
    )
    .await?;
    if source.admission.resolved_object.hex != token.request().base.hex
        || source.admission.repository_binding_generation != token.request().binding_generation
    {
        return Err(refused());
    }
    let built = build::assemble(build::Assembly {
        state,
        runtime: &runtime,
        compiled,
        decision,
        task: &task,
        context: &context,
        ready: &ready,
        source,
        source_operation,
        logical_identity: &token.request().logical_identity,
        generation,
    })
    .await?;
    let limits = &built.native.offered.offer.manifest.resources;
    if limits.memory_bytes > request.available.memory_bytes
        || limits.cpu_millis > request.available.cpu_millis
        || fact.bundle_bytes() > request.available.disk_bytes
    {
        return Err(refused());
    }
    let inspection = profiled_base::inspect_retained(state, &token, &fact).await?;
    let offer = publication::publish(state, auth, built, &token, &fact, &inspection).await?;
    page.server_time_ms = offer.offered.clock.server_time_ms;
    page.offers.push(offer);
    page.validate_for(request).map_err(|_| refused())?;
    Ok(page)
}
