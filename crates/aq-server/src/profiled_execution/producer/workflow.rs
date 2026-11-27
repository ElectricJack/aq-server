//! Bounded native profile: one worker slot, one task per poll from fairly
//! ordered projects, failed-attempt replacement within budget, and an exact
//! commit frozen at offer time. It never calls a model provider.
use super::{
    artifacts::{ArtifactBody, RetainedSet, canonical},
    build, compose, fairness, now, on_heap, publication, refused,
};
use crate::{
    AppState,
    error::AppError,
    intelligence::resolver::{self, ReplacementBudget, ResolveRequest},
    profiled_execution::{
        artifacts::OwnerKind,
        registration,
        replacement::{self, Assessment},
        source, source_exports,
    },
    source_retention::profiled_base,
    worker_api::AuthenticatedNode,
};
use aq_admission::{
    ArtifactKind, SourceSelection,
    control::NativePollPage,
    transport::{Poll, TransportDocument},
};
use aq_machine_control::holds;
use aq_profiled_state::git_author::PollDelivery;
use aq_role_settings::refusals::SlotResources as Slot;
use aq_routing::Placement;
use aq_selection::{CapabilitySnapshot, ExecutionTarget, TaskLineage};
use sqlx::Row;
use uuid::Uuid;
/// Bounded Graph reads per poll while looking for a project with ready work.
const MAX_PROJECTS_PER_POLL: usize = 4;

/// Where each project's further ready page starts on the next poll (in this
/// process only: after a restart the rotation starts behind the head page).
mod rotation {
    use std::{collections::BTreeMap, sync::Mutex};
    use uuid::Uuid;
    static NEXT: Mutex<BTreeMap<Uuid, String>> = Mutex::new(BTreeMap::new());
    pub(super) fn get(project: Uuid) -> Option<String> {
        NEXT.lock().ok()?.get(&project).cloned()
    }
    pub(super) fn set(project: Uuid, cursor: Option<String>) {
        if let Ok(mut next) = NEXT.lock() {
            match cursor {
                Some(cursor) => next.insert(project, cursor),
                None => next.remove(&project),
            };
        }
    }
}
const MAX_SKIPPED: usize = 8;
use crate::error::db;
async fn runtime(
    state: &AppState,
    auth: &AuthenticatedNode,
    request: &Poll,
) -> Result<
    (
        registration::RegisteredRuntime,
        CapabilitySnapshot,
        Vec<&'static str>,
    ),
    AppError,
> {
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
        .ok_or_else(|| refused())?
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
    let has = |cap: &str| {
        admitted
            .request
            .capabilities
            .iter()
            .any(|c| c.id.as_str() == cap && c.version == 1)
            && qualified
                .capabilities
                .iter()
                .any(|c| c.id.as_str() == cap && c.version == 1)
    };
    for cap in [
        "aq.profiled-launch",
        "aq.terminal",
        "aq.harness.startup-context",
        "aq.harness.stop",
    ] {
        if !has(cap) {
            return Err(refused());
        }
    }
    // Assembly rechecks the confinement mode pinned by the manifest.
    let modes: Vec<&'static str> = [
        aq_selection::ConfinementMode::Native.capability(),
        aq_selection::ConfinementMode::Container.capability(),
    ]
    .into_iter()
    .filter(|cap| has(cap))
    .collect();
    if modes.is_empty() {
        return Err(refused());
    }
    let row=sqlx::query("SELECT boot_id,sequence,snapshot FROM intelligence_node_snapshots WHERE node_id=$1 FOR SHARE").bind(auth.id()).fetch_optional(&mut *tx).await.map_err(db)?.ok_or_else(|| refused())?;
    let snapshot: CapabilitySnapshot =
        serde_json::from_value(row.get("snapshot")).map_err(|_| refused())?;
    if row.get::<Uuid, _>("boot_id") != request.identity.boot_id
        || (row.get::<i64, _>("sequence") as u64) < request.inventory_sequence
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
    Ok((runtime, snapshot, modes))
}
async fn reservation(
    state: &AppState,
    compiled: &compose::Compiled,
    operation: Uuid,
    task: Uuid,
    observed: source::Observation,
) -> Result<source_exports::Reservation, AppError> {
    let project = compiled.frozen.project.binding.project_id;
    if let Some(old) = source_exports::recover_existing(state, operation, project).await? {
        let token = match &old {
            source_exports::Reservation::Pending(token)
            | source_exports::Reservation::Verified {
                reservation: token, ..
            } => token,
        };
        source::require_base(state, task, &observed, token.request()).await?;
        return Ok(old);
    }
    // Original proposal remains Context. Final Source owner publication is a
    // separate receipt of the actual admission observation, not the Git bundle.
    let mut proposals = RetainedSet::new(
        project,
        now(state)?
            .checked_add(7 * 86_400_000)
            .ok_or_else(|| refused())?,
    );
    let artifact_body = ArtifactBody::new(
        ArtifactKind::Context,
        observed.artifact.clone(),
        &observed.bytes,
    );
    proposals
        .retain(state, OwnerKind::SourceRetention, artifact_body)
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
    replacement: Option<&replacement::Replacement>,
) -> Result<resolver::Decision, AppError> {
    let lineage = TaskLineage {
        project_id: compiled.frozen.project.binding.project_id,
        workspace_id: task.workspace_id,
        task_id: task.task_id,
    };
    let replacement = replacement
        .map(|r| {
            Ok::<_, AppError>(ReplacementBudget {
                attempts_started: r.attempts_started,
                exempt_attempts: r.exempt_attempts,
                elapsed_ms: r.elapsed_ms,
                evidence: replacement::evidence(r, lineage.clone(), now(state)?)
                    .ok_or(AppError::Internal)?,
            })
        })
        .transpose()?;
    resolver::resolve_for_scheduler(
        state,
        ResolveRequest {
            operation_id: Uuid::new_v4(),
            lineage,
            project: compiled.frozen.clone(),
            launches: compiled.prepared.iter().map(|p| p.launch.clone()).collect(),
            replacement,
            record_refused: false,
            route: compiled.route.clone(),
        },
        &compiled.snapshot,
        compiled.workspace_alternative,
    )
    .await
}
struct PollDiagnostics<'a> {
    capture: Option<&'a crate::scheduling_diagnostics::Capture>,
    clock: &'a crate::Clock,
    operation: Uuid,
    known: std::sync::atomic::AtomicBool,
}

impl<'a> PollDiagnostics<'a> {
    fn new(state: &'a AppState, request: &Poll) -> Self {
        Self {
            capture: state.store.scheduling_diagnostics.as_ref(),
            clock: &state.clock,
            operation: request.operation_id,
            known: Default::default(),
        }
    }

    fn record(&self, task: Option<Uuid>, reason: &'static str) {
        if let Some(capture) = self.capture {
            self.known.store(true, std::sync::atomic::Ordering::Relaxed);
            capture.decision(
                self.operation,
                task,
                reason,
                (self.clock)().timestamp_millis(),
            );
        }
    }

    fn empty(&self) {
        if !self.known.load(std::sync::atomic::Ordering::Relaxed) {
            self.record(None, "unknown");
        }
    }
}

/// A task this poll leaves for a later one or another node; never a refused poll.
fn not_offered(reason: &str) -> bool {
    aq_role_settings::refusals::is_refusal(reason) || reason.starts_with(compose::INELIGIBLE)
}
/// Review I5: a poll skips at most MAX_SKIPPED refused successors.
fn skips_spent(skipped: &std::collections::BTreeSet<Uuid>) -> bool {
    skipped.len() >= MAX_SKIPPED
}
/// Spec 0007 Q1 backstop: selection refused a successor. That is no offer this
/// poll; first-attempt capacity waits also skip, while other refusals are retained.
/// A refused replacement joins `skipped`: the poll goes on without it, its hold or skip
/// recorded (R6, #116 I1).
async fn refused_successor(
    state: &AppState,
    decision: &resolver::Decision,
    diagnostics: &PollDiagnostics<'_>,
    successor: Option<holds::Successor>,
    skipped: &mut std::collections::BTreeSet<Uuid>,
) -> bool {
    // Spec 0011 §4: a route this node cannot serve skips a first attempt for the poll; so does
    // a routed task no candidate resolves for (a refused poll took the node out of service).
    let unresolved = decision.inputs.route.is_some() && decision.outcome.resolved.is_none();
    let unservable = successor.is_none() && (unservable_route(decision).is_some() || unresolved);
    if unservable {
        tracing::warn!(task_id = %decision.inputs.project.lineage.task_id, reasons = ?decision.outcome.reason_codes, "no candidate serves this routed task on the polling node; skipped");
    }
    let waiting = aq_swarm_settings::admission::is_wait(&decision.outcome.reason_codes);
    if waiting || unservable {
        let reason = match unservable_route(decision) {
            Some(aq_selection::RouteRefusal::ProviderPinned) => "route_provider_pinned",
            Some(aq_selection::RouteRefusal::AllExcluded) => "route_all_excluded",
            None if waiting => "admission_wait",
            None => "route_unserved",
        };
        diagnostics.record(Some(decision.inputs.project.lineage.task_id), reason);
        skipped.insert(decision.inputs.project.lineage.task_id);
        return true;
    }
    let (None, Some(s)) = (&decision.outcome.resolved, successor) else {
        return false;
    };
    diagnostics.record(Some(s.task), "successor_selection_refused");
    let (pool, reasons) = (&state.store.pool, &decision.outcome.reason_codes);
    holds::selection_refused(pool, s, reasons, (state.clock)()).await;
    skipped.insert(s.task)
}
/// The route reason selection refused a routed task for: no candidate can serve its route
/// on this node now (spec 0011 §4).
pub(crate) fn unservable_route(
    decision: &resolver::Decision,
) -> Option<aq_selection::RouteRefusal> {
    decision.inputs.route.as_ref()?;
    aq_selection::route_refusal(&decision.outcome)
}
/// The commit frozen into the export operation key. An immutable policy keeps
/// its historical key; a named ref adds the commit it resolved to at offer
/// time, so a moved ref is a different export and never a followed one.
fn frozen_commit<'a>(
    policy: &SourceSelection,
    observed: &'a source::Observation,
) -> Option<&'a str> {
    match policy {
        SourceSelection::Immutable { .. } => None,
        SourceSelection::NamedRef { .. } => Some(observed.admission.resolved_object.hex.as_str()),
    }
}
async fn first_eligible_task<F, Fut>(
    ready: &crate::agent_access::scheduling::ReadyPage,
    skipped: &std::collections::BTreeSet<Uuid>,
    diagnostics: Option<&PollDiagnostics<'_>>,
    mut assess: F,
) -> Result<Option<(aq_admission::TaskRevision, Assessment)>, AppError>
where
    F: FnMut(Uuid) -> Fut,
    Fut: std::future::Future<Output = Result<Assessment, AppError>>,
{
    // Priority is AQ policy carried in Graph metadata. Order only this bounded
    // page; stable sorting retains Graph order for ties and neutral priorities.
    // Missing or non-integer values use zero, so arbitrary Graph metadata cannot
    // prevent otherwise eligible tasks from being considered.
    let mut candidates = ready.items.iter().collect::<Vec<_>>();
    candidates.sort_by_key(|candidate| {
        std::cmp::Reverse(
            candidate.node["metadata"]["aq:priority"]
                .as_i64()
                .unwrap_or(0),
        )
    });
    for candidate in candidates
        .into_iter()
        .filter(|c| !skipped.contains(&c.revision.task_id))
    {
        // Graph readiness is independent of AQ's retained task claims. A held
        // task, an exhausted retry/time/sandbox budget or an uncertain prior
        // must not starve later tasks on this same bounded page.
        match assess(candidate.revision.task_id).await? {
            Assessment::Blocked(reason) => {
                if let Some(diagnostics) = diagnostics {
                    diagnostics.record(Some(candidate.revision.task_id), reason);
                }
                tracing::debug!(task_id = %candidate.revision.task_id, reason, "ready task not offered");
            }
            admitted => return Ok(Some((candidate.revision.clone(), admitted))),
        }
    }
    Ok(None)
}
/// Authenticated daemon-owned scheduling entry. The public request carries only
/// this worker's inventory revision/capacity, never task/composition/source facts.
pub async fn poll(
    state: &AppState,
    auth: &AuthenticatedNode,
    request: &Poll,
) -> Result<NativePollPage, AppError> {
    poll_slot(state, auth, request, 1, false, false)
        .await
        .map(PollDelivery::into_native)
}
pub(super) async fn poll_slot(
    state: &AppState,
    auth: &AuthenticatedNode,
    request: &Poll,
    slot: u32,
    supports_git_author: bool,
    supports_workspace: bool,
) -> Result<PollDelivery, AppError> {
    TransportDocument::Poll(request.clone())
        .validate()
        .map_err(|_| AppError::BadRequest("invalid native poll".into()))?;
    let cancellations = super::super::cancellation::pending(state, auth, &request.identity).await?;
    let page = PollDelivery {
        schema_version: 1,
        operation_id: request.operation_id,
        identity: request.identity.clone(),
        server_time_ms: now(state)?,
        offers: vec![],
        cancellations,
        next_after: None,
    };
    let diagnostics = PollDiagnostics::new(state, request);
    // Zero-capacity polls deliver cancellation without depending on inventory.
    if request.available.slots == 0 {
        diagnostics.record(None, "zero_available_slots");
        page.validate_for(request).map_err(|_| refused())?;
        return Ok(page);
    }
    // Retain one assembler per node/slot across polls.
    let Some(permit) = aq_machine_control::offer_assembly::enter(auth.id(), slot) else {
        diagnostics.record(None, "offer_assembly_contention");
        return Ok(page);
    };
    let mut assembly = tokio::spawn({
        let state = state.clone();
        let auth = auth.clone();
        let request = request.clone();
        let page = page.clone();
        async move {
            let _permit = permit;
            let result = Box::pin(offers(
                &state,
                &auth,
                &request,
                page,
                slot,
                supports_git_author,
                supports_workspace,
            ))
            .await;
            if let Err(error) = &result {
                tracing::warn!(node_id = %auth.id(), ?error, "background offer assembly refused");
            }
            result
        }
    });
    match tokio::time::timeout(std::time::Duration::from_millis(750), &mut assembly).await {
        // Contention with a sibling slot is no offer this poll, not a failed poll.
        Ok(Ok(Err(AppError::Busy))) => {
            diagnostics.record(None, "offer_assembly_contention");
            Ok(page)
        }
        Err(_) => {
            diagnostics.record(None, "offer_assembly_pending");
            Ok(page)
        }
        Ok(Ok(result)) if page.cancellations.is_empty() => result,
        Ok(Ok(Ok(scheduled))) => Ok(scheduled),
        _ => Ok(page),
    }
}

async fn offers(
    state: &AppState,
    auth: &AuthenticatedNode,
    request: &Poll,
    mut page: PollDelivery,
    slot: u32,
    supports_git_author: bool,
    supports_workspace: bool,
) -> Result<PollDelivery, AppError> {
    let diagnostics = PollDiagnostics::new(state, request);
    let (runtime, snapshot, modes) = on_heap(|| runtime(state, auth, request)).await?;
    if request.available.slots == 0 {
        return Ok(page);
    }
    // Preparing/granted rows are not offers; their control owners fence recovery.
    if let Some(native) =
        on_heap(|| super::redelivery::existing(state, auth, request, slot)).await?
    {
        if native.git_author.is_some() && !supports_git_author {
            diagnostics.record(
                Some(native.native.offered.offer.manifest.bindings.task_id),
                "git_author_unsupported",
            );
            return Ok(page);
        }
        let offer = &native.native.offered.offer;
        let binding = &offer.manifest.bindings;
        let mut connection = state.store.pool.acquire().await.map_err(db)?;
        if let Some(reason) = crate::task_workspace_resume::still_current(
            &mut connection,
            binding.project_id,
            binding.task_id,
            offer.binding.attempt_id,
            supports_workspace,
        )
        .await?
        {
            diagnostics.record(Some(binding.task_id), reason);
            return Ok(page);
        }
        page.server_time_ms = native.native.offered.clock.server_time_ms;
        page.offers.push(native);
        page.validate_for(request).map_err(|_| refused())?;
        return Ok(page);
    }
    let mut gate = state.store.pool.acquire().await.map_err(|_| refused())?;
    let hold = aq_machine_control::recent::offer_hold(&mut gate, auth.id());
    if let Some(reason) = hold.await.map_err(|_| refused())? {
        diagnostics.record(None, "node_offer_hold");
        tracing::info!(node_id = %auth.id(), "{reason}; no new offers");
        return Ok(page);
    }
    drop(gate);
    let mut occupancy = state.store.pool.begin().await.map_err(db)?;
    crate::worker_api::lock_current_node(&mut occupancy, auth).await?;
    let free = aq_machine_control::recent::poll_free(&mut occupancy, auth.id(), slot);
    let free = free.await.map_err(crate::machine_api::mapped)?;
    occupancy.commit().await.map_err(db)?;
    if !free {
        diagnostics.record(None, "slot_occupied");
        return Ok(page);
    }
    // Node/server holds end the poll; a project hold skips that project.
    let config = state
        .config
        .profiled_execution
        .as_ref()
        .ok_or_else(|| refused())?;
    let staging = (state.source_retention.as_ref())
        .map(|r| (r.config.max_staging_bytes, r.config.max_bundle_bytes));
    let mut conn = state.store.pool.acquire().await.map_err(|_| refused())?;
    let gate = aq_history_caps::poll_gate(
        &mut conn,
        auth.id(),
        &config.enabled_projects(),
        aq_history_caps::Snapshot::new(now(state)?, staging),
    )
    .await
    .map_err(|_| refused())?;
    drop(conn);
    if let Some(h) = &gate.stop {
        diagnostics.record(None, "history_capacity_hold");
        tracing::info!(node_id = %auth.id(), "{}; no new offers", h.log_line());
        return Ok(page);
    }
    if config
        .projects
        .iter()
        .any(|p| p.enabled && gate.is_held(p.project_id))
    {
        diagnostics.record(None, "project_history_capacity_hold");
    }
    let candidates = config
        .projects
        .iter()
        .filter(|p| p.enabled && !gate.is_held(p.project_id))
        .map(|p| fairness::Candidate {
            project_id: p.project_id,
            priority: p.priority,
        })
        .collect::<Vec<_>>();
    let graph = state.agent_access.as_ref().ok_or_else(|| refused())?;
    let polled_at = now(state)?;
    let recorded = on_heap(|| fairness::recorded(&state.store.pool))
        .await
        .map_err(db)?;
    let served = if recorded {
        on_heap(|| fairness::served(&state.store.pool))
            .await
            .map_err(db)?
    } else {
        Default::default()
    };
    // Fair share (§8.2): priority plus aging. A project is charged a turn
    // before its Graph read, so one with no work or a failing owner read
    // cannot hold the head of the order and starve the others.
    // Review I5: a successor refused at a backstop is skipped and selection
    // goes on with the next eligible task, at most MAX_SKIPPED times a poll.
    let mut skipped = std::collections::BTreeSet::new();
    loop {
        if skips_spent(&skipped) {
            diagnostics.record(None, "poll_skip_limit");
            return Ok(page);
        }
        let mut chosen = None;
        'projects: for candidate in fairness::order(&candidates, &served, polled_at)
            .into_iter()
            .take(MAX_PROJECTS_PER_POLL)
        {
            let policy = config
                .projects
                .iter()
                .find(|p| p.project_id == candidate.project_id)
                .ok_or_else(|| refused())?;
            if recorded {
                on_heap(|| fairness::record(&state.store.pool, policy.project_id, polled_at))
                    .await
                    .map_err(db)?;
            }
            let record =
                on_heap(|| crate::project_bindings::current(state, policy.project_id)).await?;
            if !(record.binding).eligible_node(&state.config.projects, auth.id()) {
                diagnostics.record(None, "project_node_ineligible");
                continue;
            }
            // The head page first, as ever. When nothing on it can be offered
            // and more tasks are ready, one further page a poll, rotating
            // through the rest: tasks that wait (for a route, or held) at the
            // head never hide the ready tasks behind them for good.
            let mut cursor: Option<String> = None;
            for further in [false, true] {
                let read = on_heap(|| {
                    graph.scheduling_ready_for_project(
                        &record.binding,
                        8,
                        cursor.as_deref(),
                        state.clock.as_ref(),
                        &state.store.pool,
                    )
                })
                .await;
                let mut ready = match read {
                    Ok(ready) => ready,
                    // A cursor the graph no longer takes: start over next poll.
                    Err(_) if further => {
                        rotation::set(policy.project_id, None);
                        break;
                    }
                    Err(_) => return Err(refused()),
                };
                let next = ready.next_cursor.clone();
                for candidate in &mut ready.items {
                    if !aq_machine_control::launch::eligible(
                        &state.store.pool,
                        candidate.revision.task_id,
                        auth.id(),
                        &candidate.node["metadata"],
                    )
                    .await
                    .map_err(db)?
                    {
                        diagnostics.record(Some(candidate.revision.task_id), "launch_gate_blocked");
                        skipped.insert(candidate.revision.task_id);
                    }
                }
                // Spec 0011 §4, before any context read: a task's route, or its wait for one.
                let placements =
                    on_heap(|| crate::routing::placements(state, &record, &ready.items));
                let placements = placements.await?;
                // Spec 0014 §5.5, §5.2: a task whose effective role is disabled
                // or missing, or whose level has no document, gets no offer.
                let nodes = ready.items.iter().map(|c| {
                    let class = match placements.get(&c.revision.task_id) {
                        Some(Placement::Routed(route)) => Some(route.c.to_string()),
                        _ => None,
                    };
                    (c.revision.task_id, &c.node, class)
                });
                let role_held = crate::role_settings::held(state, &record.binding, nodes).await?;
                let (revision, project, workspace) = (
                    record.revision,
                    policy.project_id,
                    record.binding.graph_workspace_id,
                );
                let ready_ids: Vec<_> = ready
                    .items
                    .iter()
                    .map(|item| item.revision.task_id)
                    .collect();
                let workspace_tasks = crate::task_workspace_resume::tasks_with_requirements(
                    &state.store.pool,
                    project,
                    &ready_ids,
                )
                .await?;
                let workspace_tasks = &workspace_tasks;
                let (placed, items, role_held) = (&placements, &ready.items, &role_held);
                if let Some((task, assessment)) = on_heap(|| {
                    first_eligible_task(&ready, &skipped, Some(&diagnostics), |task| async move {
                        if let Some(code) = role_held.get(&task) {
                            return Ok(Assessment::Blocked(code));
                        }
                        let placement = placed.get(&task);
                        if placement == Some(&Placement::Wait) {
                            return Ok(Assessment::Blocked("route_missing"));
                        }
                        let (pool, at) = (&state.store.pool, (state.clock)());
                        // A worker's wire support is local to this poll.
                        // Keep scanning without persisting a hold on the task.
                        if !supports_git_author
                            && super::task_git_author(state, pool, project, workspace, task)
                                .await?
                                .is_some()
                        {
                            return Ok(Assessment::Blocked("git_author_unsupported"));
                        }
                        if workspace_tasks.contains(&task)
                            && let Some(reason) =
                                crate::task_workspace_resume::scope_at_poll(pool, project, task)
                                    .await?
                                    .blocker(supports_workspace)
                        {
                            return Ok(Assessment::Blocked(reason));
                        }
                        let seen = holds::history(pool, task).await;
                        let a =
                            replacement::assess_task_for_revision(state, task, polled_at, revision);
                        let a = a.await?;
                        if let Some(replacement) = a.replacement() {
                            let mut connection = pool.acquire().await.map_err(db)?;
                            if replacement::pending_publication_owner(
                                &mut connection,
                                replacement.predecessor,
                            )
                            .await?
                            .is_some()
                            {
                                return Ok(Assessment::Blocked("owner_retry_pending"));
                            }
                        }
                        if let Assessment::Blocked(reason) = a {
                            holds::selection_hold(
                                pool,
                                seen,
                                holds::SelectionRefusal {
                                    project,
                                    task,
                                    reason,
                                },
                                at,
                            )
                            .await;
                        } else if placement == Some(&Placement::Reroute) {
                            let attempt = a.replacement().map(|r| (r.predecessor, seen));
                            let r = crate::routing::reroute(state, project, items, task, attempt);
                            r.await;
                            return Ok(Assessment::Blocked("route_unreadable"));
                        }
                        Ok(a)
                    })
                })
                .await?
                {
                    let route = match placements.get(&task.task_id) {
                        Some(Placement::Routed(route)) => Some(route.clone()),
                        _ => None,
                    };
                    chosen = Some((policy, record, ready, task, assessment, route));
                    break 'projects;
                }
                if further {
                    rotation::set(policy.project_id, next);
                    break;
                }
                cursor = rotation::get(policy.project_id).or(next);
                if cursor.is_none() {
                    break;
                }
            }
        }
        let Some((policy, record, ready, task, assessment, route)) = chosen else {
            diagnostics.empty();
            return Ok(page);
        };
        // A sibling slot is assembling this task: this slot takes the next one.
        let Some(_claim) = aq_machine_control::offer_assembly::claim_task(task.task_id) else {
            diagnostics.record(Some(task.task_id), "task_assembly_contention");
            skipped.insert(task.task_id);
            continue;
        };
        let enrolled = super::super::delivery_enrolled(state, policy.project_id, task.task_id);
        let route = route.as_ref();
        let replacement = assessment.replacement().cloned();
        // R6: a successor's hold is recorded against the attempt count read before its refusal.
        let successor = match &replacement {
            Some(r) => Some(holds::Successor {
                project: policy.project_id,
                task: task.task_id,
                predecessor: r.predecessor,
                seen: holds::history(&state.store.pool, task.task_id).await,
            }),
            None => None,
        };
        if let Some(replacement) = &replacement {
            tracing::info!(
                task_id = %task.task_id,
                predecessor = %replacement.predecessor,
                attempts_started = replacement.attempts_started,
                lineage_elapsed_ms = replacement.elapsed_ms,
                "replacement attempt admitted within the retry budget"
            );
            // The failed predecessor's claim is released under its locks before
            // the successor's generation; a later refusal retries idempotently.
            on_heap(|| replacement::release(state, task.task_id, replacement)).await?;
        }
        let generation =
            on_heap(|| super::super::no_grant::next_generation(&state.store.pool, task.task_id))
                .await?;
        let workspace = crate::task_workspace_resume::scope_at_poll(
            &state.store.pool,
            policy.project_id,
            task.task_id,
        )
        .await?;
        if let Some(reason) = workspace.blocker(supports_workspace) {
            diagnostics.record(Some(task.task_id), reason);
            skipped.insert(task.task_id);
            continue;
        }
        let metadata = (ready.items.iter())
            .find(|r| r.revision == task)
            .map(|r| r.node["metadata"].clone())
            .ok_or_else(|| refused())?;
        #[cfg(feature = "project_environments")]
        let metadata = {
            let mut metadata = metadata;
            workspace.apply_service_needs(&mut metadata);
            metadata
        };
        let context = on_heap(|| {
            graph.scheduling_context_current(&task, 1, 64, 1_048_576, state.clock.as_ref())
        })
        .await
        .map_err(|_| refused())?;
        let compile = || {
            compose::compile(
                state,
                &record,
                auth.id(),
                &snapshot,
                &context,
                &metadata,
                route,
                replacement.as_ref().map(|r| r.predecessor),
                &workspace,
            )
        };
        let compiled = match on_heap(compile).await {
            // Spec 0014 §6.1: a startup text over its byte limit, or an environment
            // without platform text, is refused and recorded for the task's work
            // read; the poll goes on with the next task.
            Err(AppError::Conflict(reason)) if not_offered(&reason) => {
                tracing::warn!(task_id = %task.task_id, %reason, "task not offered");
                diagnostics.record(Some(task.task_id), "composition_refused");
                skipped.insert(task.task_id);
                continue;
            }
            compiled => compiled?,
        };
        if crate::role_settings::tables(state) {
            aq_role_settings::refusals::clear_after_compose(&state.store.pool, task.task_id).await;
        }
        super::observation::ReadProvenance::capture(
            &task,
            aq_profiled_state::native_offer::GraphReads {
                ready: &ready,
                context: &context,
            },
            compiled.projection.artifact.digest.clone(),
            now(state)?,
        )
        .map_err(|_| refused())?;
        let original_ready_node = canonical(
            &ready
                .items
                .iter()
                .find(|r| r.revision == task)
                .ok_or_else(|| refused())?
                .node,
        )?;
        let decision = on_heap(|| selection(state, &task, &compiled, replacement.as_ref())).await?;
        if refused_successor(state, &decision, &diagnostics, successor, &mut skipped).await {
            continue;
        }
        let selected = decision
            .outcome
            .resolved
            .as_ref()
            .ok_or_else(|| refused())?;
        compiled
            .prepared
            .iter()
            .find(|p| p.launch.digest == selected.choice.launch_digest)
            .ok_or_else(|| refused())?;
        let policy_record = config
            .project_policy_record(policy.project_id)
            .map_err(|_| refused())?;
        let handoff = match &replacement {
            Some(r) => {
                on_heap(|| super::super::handoff::successor_base(state, task.task_id, r)).await?
            }
            None => None,
        };
        if handoff.is_none()
            && let Some(r) = &replacement
        {
            let mut connection = state.store.pool.acquire().await.map_err(db)?;
            if crate::task_workspace_resume::checkpoint_required(&mut connection, r.predecessor)
                .await?
                && !super::super::has_delivery_grant(&mut *connection, r.predecessor).await?
            {
                diagnostics.record(Some(task.task_id), crate::task_workspace_resume::CHECKPOINT);
                skipped.insert(task.task_id);
                continue;
            }
        }
        let retained = match replacement.as_ref().filter(|_| handoff.is_none()) {
            Some(r) => {
                Some(on_heap(|| replacement::retained_source(state, task.task_id, r)).await?)
            }
            None => None,
        };
        // Past the fence, a newer revision differs only in placement: export it again.
        let repin = (retained.as_ref().map(|r| r.token().request().clone()))
            .filter(|r| r.project_revision != compiled.frozen.project.revision);
        let (token, fact) = if let Some(retained) = retained.filter(|_| repin.is_none()) {
            // Reuse the exact verified export and its operation identity.
            retained.into_parts()
        } else {
            // Freeze the configured source, or a fresh export of a fixed commit.
            let fresh = async {
                let predecessor = replacement.as_ref().map(|r| r.predecessor);
                let frozen = &compiled.frozen;
                let observed = match (&handoff, &repin) {
                    (Some(base), _) => {
                        on_heap(|| source::observe_handoff(state, frozen, &policy_record, base))
                            .await?
                    }
                    (None, Some(r)) => {
                        let observe = source::observe_retained;
                        on_heap(|| observe(state, frozen, &policy_record, r, predecessor)).await?
                    }
                    (None, None) => {
                        on_heap(|| {
                            source::Observer::new(state, frozen, &policy_record)
                                .for_offer(&context, enrolled)
                        })
                        .await?
                    }
                };
                let frozen = match (&handoff, &repin) {
                    (Some(base), _) => Some(base.oid.as_str()),
                    (None, Some(r)) => Some(r.base.hex.as_str()),
                    (None, None) => frozen_commit(&policy.source, &observed),
                };
                let operation = aq_delivery::offer_source::export_operation(
                    aq_delivery::offer_source::ExportScope {
                        project: policy.project_id,
                        revision: record.revision,
                        generation,
                        enrolled,
                    },
                    &task,
                    config
                        .project_policy_ref(policy.project_id)
                        .map_err(|_| refused())?,
                    frozen,
                )
                .map_err(|_| AppError::Internal)?;
                let reserved =
                    on_heap(|| reservation(state, &compiled, operation, task.task_id, observed));
                Ok(match reserved.await? {
                    source_exports::Reservation::Pending(token) => {
                        let fact = on_heap(|| profiled_base::verify_export(state, &token)).await?;
                        on_heap(|| source_exports::finalize(state, &token, &fact)).await?;
                        (token, fact)
                    }
                    source_exports::Reservation::Verified {
                        reservation,
                        canonical_verified,
                    } => {
                        let fact = profiled_base::VerifiedExport::decode_stored(
                            &reservation,
                            &canonical_verified,
                        )?;
                        (reservation, fact)
                    }
                })
            }
            .await;
            match fresh {
                Err(AppError::Conflict(reason)) if not_offered(&reason) => {
                    diagnostics.record(Some(task.task_id), "source_refused");
                    skipped.insert(task.task_id);
                    continue;
                }
                // Q1 (live acceptance D5): a refused handoff export skips the task.
                // #116 I1, M1: a counted skip; held as `handoff_successor_stuck` once stuck.
                Err(error @ AppError::Conflict(_)) if handoff.is_some() || repin.is_some() => {
                    diagnostics.record(Some(task.task_id), "successor_source_refused");
                    let why = format!("successor export refused: {error:?}");
                    if let Some(s) = successor {
                        holds::skipped(&state.store.pool, s, &why, (state.clock)()).await;
                    }
                    skipped.insert(task.task_id);
                    continue;
                }
                fresh => fresh?,
            }
        };
        let source_operation = token.request().operation_id;
        if !aq_machine_control::offer_assembly::deliverable(&snapshot, now(state)?) {
            diagnostics.record(Some(task.task_id), "stale_capacity");
            return Ok(page);
        }
        let ready = on_heap(|| graph.scheduling_task_ready_current(&task, state.clock.as_ref()))
            .await
            .map_err(|_| refused())?;
        if canonical(
            &ready
                .items
                .iter()
                .find(|r| r.revision == task)
                .ok_or_else(|| refused())?
                .node,
        )? != original_ready_node
        {
            return Err(refused());
        }
        let context = on_heap(|| {
            graph.scheduling_context_current(&task, 1, 64, 1_048_576, state.clock.as_ref())
        })
        .await
        .map_err(|_| refused())?;
        let compile = || {
            compose::compile(
                state,
                &record,
                auth.id(),
                &snapshot,
                &context,
                &metadata,
                route,
                replacement.as_ref().map(|r| r.predecessor),
                &workspace,
            )
        };
        let compiled = match on_heap(compile).await {
            Err(AppError::Conflict(reason)) if reason.starts_with(compose::INELIGIBLE) => {
                diagnostics.record(Some(task.task_id), "composition_refused");
                skipped.insert(task.task_id);
                continue;
            }
            compiled => compiled?,
        };
        let decision = on_heap(|| selection(state, &task, &compiled, replacement.as_ref())).await?;
        if refused_successor(state, &decision, &diagnostics, successor, &mut skipped).await {
            continue;
        }
        if replacement.is_some() {
            holds::cleared(&state.store.pool, task.task_id).await;
        }
        let observed = async {
            let source = if replacement.is_some() {
                on_heap(|| {
                    source::observe_retained(
                        state,
                        &compiled.frozen,
                        &policy_record,
                        token.request(),
                        None,
                    )
                })
                .await?
            } else {
                on_heap(|| {
                    source::Observer::new(state, &compiled.frozen, &policy_record)
                        .for_offer(&context, enrolled)
                })
                .await?
            };
            source::require_base(state, task.task_id, &source, token.request()).await?;
            Ok(source)
        }
        .await;
        let source = match observed {
            Err(AppError::Conflict(reason)) if not_offered(&reason) => {
                diagnostics.record(Some(task.task_id), "source_refused");
                skipped.insert(task.task_id);
                continue;
            }
            observed => observed?,
        };
        let stage = Uuid::new_v4();
        let assemble = async {
            let built = on_heap(|| {
                build::assemble(build::Assembly {
                    stage,
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
                    enrolled,
                    supports_git_author,
                    workspace: &workspace,
                })
            })
            .await?;
            if !built
                .native
                .offered
                .offer
                .manifest
                .harness
                .required_capabilities
                .iter()
                .any(|c| c.version == 1 && modes.contains(&c.id.as_str()))
            {
                return Err(refused());
            }
            let need = &built.native.offered.offer.manifest.resources;
            let need = Slot::new(need.cpu_millis, need.memory_bytes, fact.bundle_bytes());
            let have = &request.available;
            let have = Slot::new(have.cpu_millis, have.memory_bytes, have.disk_bytes);
            if let Some(short) = need.short_on(have, auth.id()) {
                diagnostics.record(Some(task.task_id), "slot_resources_insufficient");
                crate::role_settings::offer_refused(state, task.task_id, policy.project_id, &short)
                    .await;
                return Err(refused());
            }
            let inspection =
                on_heap(|| profiled_base::inspect_retained(state, &token, &fact)).await?;
            // A definitive database rollback permits retry with the same artifacts and expiry.
            let publish =
                || publication::publish(state, auth, &built, &token, &fact, &inspection, slot);
            let mut offer = on_heap(publish).await;
            for delay in [50, 150] {
                if !matches!(offer, Err(AppError::Busy)) {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                offer = on_heap(publish).await;
            }
            offer
        };
        let offer = Box::pin(assemble).await;
        if let Err(error) =
            aq_history_caps::offer_stage::close(&state.store.pool, stage, now(state)?).await
        {
            tracing::warn!(%stage, %error, "offer staging cleanup deferred until expiry");
        }
        // Another slot can take the last provider reservation after selection.
        // The publication transaction rolled back. Keep polling other tasks and
        // reconsider this task with fresh capacity on a later poll.
        let offer = match offer {
            Err(AppError::Conflict(reason))
                if aq_swarm_settings::admission::is_wait_reason(&reason) =>
            {
                diagnostics.record(Some(task.task_id), "admission_wait");
                skipped.insert(task.task_id);
                continue;
            }
            result => result?,
        };
        aq_machine_control::recent::forget(auth.id()).await;
        let offer = match offer {
            publication::Outcome::Published(offer) => *offer,
            publication::Outcome::Unavailable => {
                diagnostics.record(Some(task.task_id), "publication_unavailable");
                return Ok(page);
            }
            publication::Outcome::WorkspaceBlocked(reason) => {
                diagnostics.record(Some(task.task_id), reason);
                skipped.insert(task.task_id);
                continue;
            }
            publication::Outcome::NeedsGitAuthor => {
                diagnostics.record(Some(task.task_id), "git_author_unsupported");
                skipped.insert(task.task_id);
                continue;
            }
        };
        tracing::info!(task_id = %task.task_id, "offer assembly: offer published");
        if crate::role_settings::tables(state) {
            aq_role_settings::refusals::clear(&state.store.pool, task.task_id).await;
        }
        page.server_time_ms = offer.native.offered.clock.server_time_ms;
        page.offers.push(offer);
        page.validate_for(request).map_err(|_| refused())?;
        return Ok(page);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        agent_access::scheduling::{GraphReadStamp, ReadyPage, ReadyTask},
        profiled_execution::no_grant::AttemptCounts,
    };

    fn ready(count: usize) -> ReadyPage {
        let workspace_id = Uuid::new_v4();
        ReadyPage {
            workspace_id,
            items: (0..count)
                .map(|_| ReadyTask {
                    revision: aq_admission::TaskRevision {
                        workspace_id,
                        task_id: Uuid::new_v4(),
                        version: 1,
                        content_revision: 1,
                    },
                    node: serde_json::json!({}),
                })
                .collect(),
            next_cursor: None,
            stamp: GraphReadStamp {
                requested_at_ms: 1,
                received_at_ms: 1,
                expires_at_ms: 2,
                response_digest: "a".repeat(64),
            },
        }
    }

    #[tokio::test]
    async fn ready_selection_uses_descending_priority_and_preserves_graph_order_for_ties() {
        for (priorities, expected) in [
            (vec![0, 10, 30, 20], 2),
            (vec![10, 30, 30, 20], 1),
            (vec![0, 0, 0, 0], 0),
            (vec![-30, -10, -20, -40], 1),
            (vec![i64::MIN, 0, i64::MAX, -1], 2),
        ] {
            let mut ready = ready(priorities.len());
            for (candidate, priority) in ready.items.iter_mut().zip(priorities) {
                candidate.node = serde_json::json!({"metadata":{"aq:priority":priority}});
            }
            let original = serde_json::to_value(&ready).unwrap();
            let selected = first_eligible_task(&ready, &Default::default(), None, |_| {
                std::future::ready(Ok(Assessment::Fresh))
            })
            .await
            .unwrap();
            assert_eq!(
                selected,
                Some((ready.items[expected].revision.clone(), Assessment::Fresh))
            );
            assert_eq!(serde_json::to_value(&ready).unwrap(), original);
        }
    }

    #[tokio::test]
    async fn ready_selection_treats_missing_and_noninteger_priorities_as_zero() {
        use serde_json::json;
        for metadata in [
            json!({}),
            json!({"other:priority":100}),
            json!({"aq:priority":null}),
            json!({"aq:priority":"100"}),
            json!({"aq:priority":1.5}),
            json!({"aq:priority":true}),
            json!({"aq:priority":[]}),
            json!({"aq:priority":{}}),
            json!({"aq:priority":u64::MAX}),
        ] {
            let mut ready = ready(3);
            ready.items[0].node = json!({"metadata":{"aq:priority":-1}});
            ready.items[1].node = json!({"metadata":metadata});
            ready.items[2].node = json!({"metadata":{"aq:priority":0}});
            let selected = first_eligible_task(&ready, &Default::default(), None, |_| {
                std::future::ready(Ok(Assessment::Fresh))
            })
            .await
            .unwrap();
            assert_eq!(
                selected,
                Some((ready.items[1].revision.clone(), Assessment::Fresh))
            );
        }
    }

    #[tokio::test]
    async fn ready_selection_skips_held_and_exhausted_tasks_without_starving_later_retry() {
        let mut ready = ready(4);
        for (candidate, priority) in ready.items.iter_mut().zip([20, 30, 10, 0]) {
            candidate.node = serde_json::json!({"metadata":{"aq:priority":priority}});
        }
        let counts = [
            // `started` includes held offers, not only actual kernel starts.
            AttemptCounts {
                started: 1,
                history: 1,
                never_granted: 0,
                max_never_granted: 8,
            },
            AttemptCounts {
                started: 0,
                history: 8,
                never_granted: 8,
                max_never_granted: 8,
            },
            // An owner-verified never-granted closure may retry below the cap.
            AttemptCounts {
                started: 0,
                history: 1,
                never_granted: 1,
                max_never_granted: 8,
            },
        ];
        let mut observed = Vec::new();
        let selected = first_eligible_task(&ready, &Default::default(), None, |task| {
            let index = ready
                .items
                .iter()
                .position(|r| r.revision.task_id == task)
                .unwrap();
            observed.push(task);
            // A started (held) attempt is not decided by counts; its history
            // shows a running predecessor, which blocks replacement.
            std::future::ready(Ok(replacement::precheck(&counts[index])
                .unwrap_or(Assessment::Blocked("replacement_prior_execution_uncertain"))))
        })
        .await
        .unwrap();
        assert_eq!(
            selected,
            Some((ready.items[2].revision.clone(), Assessment::Fresh))
        );
        assert_eq!(
            observed,
            [1, 0, 2]
                .into_iter()
                .map(|index| ready.items[index].revision.task_id)
                .collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn blocked_decisions_leave_selection_unchanged_with_capture_disabled_or_unavailable() {
        use crate::scheduling_diagnostics::{Capture, Observation};

        let ready = ready(2);
        let blocked = ready.items[0].revision.task_id;
        let clock = crate::system_clock();
        let operation = Uuid::new_v4();
        let (enabled, mut receiver) = Capture::channel(1);
        let (unavailable, closed) = Capture::channel(1);
        drop(closed);
        for capture in [None, Some(&enabled), Some(&unavailable)] {
            let diagnostics = PollDiagnostics {
                capture,
                clock: &clock,
                operation,
                known: Default::default(),
            };
            let selected =
                first_eligible_task(&ready, &Default::default(), Some(&diagnostics), |task| {
                    std::future::ready(Ok(if task == blocked {
                        Assessment::Blocked("route_missing")
                    } else {
                        Assessment::Fresh
                    }))
                })
                .await
                .unwrap();
            assert_eq!(
                selected,
                Some((ready.items[1].revision.clone(), Assessment::Fresh))
            );
        }
        assert!(matches!(
            receiver.try_recv().unwrap(),
            Observation::Decision { operation_id, task_id: Some(task), reason: "route_missing", .. }
                if operation_id == operation && task == blocked
        ));
    }

    #[test]
    fn a_poll_skips_at_most_max_skipped_refused_successors() {
        let mut skipped = std::collections::BTreeSet::new();
        while !skips_spent(&skipped) {
            skipped.insert(Uuid::new_v4());
        }
        assert_eq!(skipped.len(), MAX_SKIPPED);
    }
    #[tokio::test]
    async fn ready_selection_returns_empty_when_blocked_but_never_skips_owner_read_errors() {
        let ready = ready(2);
        let held = AttemptCounts {
            started: 1,
            history: 1,
            never_granted: 0,
            max_never_granted: 8,
        };
        assert!(replacement::precheck(&held).is_none());
        assert!(
            first_eligible_task(&ready, &Default::default(), None, |_| {
                std::future::ready(Ok(Assessment::Blocked(
                    "replacement_prior_execution_uncertain",
                )))
            })
            .await
            .unwrap()
            .is_none()
        );
        let mut reads = 0;
        let result = first_eligible_task(&ready, &Default::default(), None, |_| {
            reads += 1;
            std::future::ready(Err(AppError::Internal))
        })
        .await;
        assert!(matches!(result, Err(AppError::Internal)));
        assert_eq!(
            reads, 1,
            "a failed owner read cannot make another task eligible"
        );
    }
}
