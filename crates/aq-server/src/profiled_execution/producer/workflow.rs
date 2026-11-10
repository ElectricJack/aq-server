//! Bounded native profile: one worker slot, one task per poll from fairly
//! ordered projects, failed-attempt replacement within budget, and an exact
//! commit frozen at offer time. It never calls a model provider.
use super::{
    artifacts::{RetainedSet, canonical},
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
use aq_selection::{CapabilitySnapshot, ExecutionTarget, TaskLineage};
use sqlx::Row;
use std::sync::OnceLock;
use tokio::sync::Semaphore;
use uuid::Uuid;
/// Bounded Graph reads per poll while looking for a project with ready work.
const MAX_PROJECTS_PER_POLL: usize = 4;
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
    // The frozen manifest carries exactly one confinement mode (native or
    // container); the node must qualify at least one of them here, and the
    // exact mode the built manifest requires is checked after assembly.
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
    Ok((runtime, snapshot, modes))
}
async fn reservation(
    state: &AppState,
    compiled: &compose::Compiled,
    operation: Uuid,
    observed: source::Observation,
) -> Result<source_exports::Reservation, AppError> {
    let project = compiled.frozen.project.binding.project_id;
    if let Some(old) = source_exports::recover_existing(state, operation, project).await? {
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
    resolver::resolve_for_project_policy(
        state,
        ResolveRequest {
            operation_id: Uuid::new_v4(),
            lineage,
            project: compiled.frozen.clone(),
            compositions: compiled.candidates.clone(),
            replacement,
            record_refused: false,
        },
    )
    .await
}
/// Review I5: a poll skips at most MAX_SKIPPED refused successors.
fn skips_spent(skipped: &std::collections::BTreeSet<Uuid>) -> bool {
    skipped.len() >= MAX_SKIPPED
}
/// Spec 0007 Q1 backstop: selection refused a successor. That is no offer this
/// poll, never a refused poll; a first attempt keeps the refusal.
/// A refused replacement joins `skipped`: the poll goes on without it, its hold or skip
/// recorded (R6, #116 I1).
async fn refused_successor(
    state: &AppState,
    decision: &resolver::Decision,
    successor: Option<holds::Successor>,
    skipped: &mut std::collections::BTreeSet<Uuid>,
) -> bool {
    let (None, Some(s)) = (&decision.outcome.resolved, successor) else {
        return false;
    };
    let (pool, reasons) = (&state.store.pool, &decision.outcome.reason_codes);
    holds::selection_refused(pool, s, reasons, (state.clock)()).await;
    skipped.insert(s.task)
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
fn source_operation<T: serde::Serialize>(
    project: Uuid,
    task: &aq_admission::TaskRevision,
    project_revision: u64,
    generation: u64,
    policy_ref: T,
    frozen_commit: Option<&str>,
) -> Result<Uuid, AppError> {
    let key = match frozen_commit {
        None => canonical(&(project, task, project_revision, generation, policy_ref))?,
        Some(commit) => canonical(&(
            project,
            task,
            project_revision,
            generation,
            policy_ref,
            commit,
        ))?,
    };
    Ok(Uuid::new_v5(&Uuid::from_bytes(*b"aq-native-export"), &key))
}
async fn first_eligible_task<F, Fut>(
    ready: &crate::agent_access::scheduling::ReadyPage,
    skipped: &std::collections::BTreeSet<Uuid>,
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
    TransportDocument::Poll(request.clone())
        .validate()
        .map_err(|_| AppError::BadRequest("invalid native poll".into()))?;
    let cancellations = super::super::cancellation::pending(state, auth, &request.identity).await?;
    let page = NativePollPage {
        schema_version: 1,
        operation_id: request.operation_id,
        identity: request.identity.clone(),
        server_time_ms: now(state)?,
        offers: vec![],
        cancellations,
        next_after: None,
    };
    // Deliver cancellation promptly, while letting slow healthy offer assembly
    // finish for later redelivery. Dropping the JoinHandle after the wait only
    // detaches the task; the assembler permit bounds its full lifetime.
    if !page.cancellations.is_empty() {
        page.validate_for(request).map_err(|_| refused())?;
        if request.available.slots == 0 {
            return Ok(page);
        }
        let mut assembly = tokio::spawn({
            let state = state.clone();
            let auth = auth.clone();
            let request = request.clone();
            let page = page.clone();
            async move { Box::pin(offers(&state, &auth, &request, page)).await }
        });
        return match tokio::time::timeout(std::time::Duration::from_millis(750), &mut assembly)
            .await
        {
            Ok(Ok(Ok(scheduled))) => Ok(scheduled),
            _ => Ok(page),
        };
    }
    // A zero-capacity poll is a cancellation query (MS1 B3): it asks for no
    // offer, so it never depends on a fresh inventory snapshot. Answering it
    // here keeps "no pending cancellation" distinct from a refused poll for a
    // node whose inventory has aged past its TTL during a long attempt.
    if request.available.slots == 0 {
        page.validate_for(request).map_err(|_| refused())?;
        return Ok(page);
    }
    // Offer assembly has a large future. Keep this shared scheduling boundary
    // on the heap instead of embedding it in every control/fixture caller.
    on_heap(|| offers(state, auth, request, page)).await
}
async fn offers(
    state: &AppState,
    auth: &AuthenticatedNode,
    request: &Poll,
    mut page: NativePollPage,
) -> Result<NativePollPage, AppError> {
    static ASSEMBLERS: OnceLock<Semaphore> = OnceLock::new();
    let _permit = ASSEMBLERS
        .get_or_init(|| Semaphore::new(4))
        .try_acquire()
        .map_err(|_| refused())?;
    let (runtime, snapshot, modes) = on_heap(|| runtime(state, auth, request)).await?;
    if request.available.slots == 0 {
        return Ok(page);
    }
    let mut gate = state.store.pool.acquire().await.map_err(|_| refused())?;
    let hold = aq_machine_control::offer_hold(&mut gate, auth.id());
    if let Some(reason) = hold.await.map_err(|_| refused())? {
        tracing::info!(node_id = %auth.id(), "{reason}; no new offers");
        return Ok(page);
    }
    drop(gate);
    // Redelivery precedes new scheduling. Preparing/granted rows are not offers;
    // those require their independently fenced control/reconciliation owners.
    if let Some(native) = on_heap(|| super::redelivery::existing(state, auth, request)).await? {
        page.server_time_ms = native.offered.clock.server_time_ms;
        page.offers.push(native);
        page.validate_for(request).map_err(|_| refused())?;
        return Ok(page);
    }
    let mut occupancy = state.store.pool.begin().await.map_err(db)?;
    crate::worker_api::lock_current_node(&mut occupancy, auth).await?;
    let busy = super::super::compute::busy_locked(&mut occupancy, auth.id()).await?;
    occupancy.commit().await.map_err(db)?;
    if busy {
        return Ok(page);
    }
    // Spec 0008: one grouped read. A node or server hold ends the poll before any write;
    // a project hold drops that project from the candidates (it is never charged a turn).
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
        now(state)?,
        staging,
    )
    .await
    .map_err(|_| refused())?;
    drop(conn);
    if let Some(h) = &gate.stop {
        tracing::info!(node_id = %auth.id(), "{}; no new offers", h.log_line());
        return Ok(page);
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
            return Ok(page);
        }
        let mut chosen = None;
        for candidate in fairness::order(&candidates, &served, polled_at)
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
            let ready = on_heap(|| {
                graph.scheduling_ready_current(
                    record.binding.graph_workspace_id,
                    8,
                    None,
                    state.clock.as_ref(),
                )
            })
            .await
            .map_err(|_| refused())?;
            let mut ready = ready;
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
                    skipped.insert(candidate.revision.task_id);
                }
            }
            let (revision, project) = (record.revision, policy.project_id);
            if let Some((task, assessment)) = on_heap(|| {
                first_eligible_task(&ready, &skipped, |task| async move {
                    let (pool, at) = (&state.store.pool, (state.clock)());
                    let seen = holds::attempts(pool, task).await;
                    let a = replacement::assess_task_for_revision(state, task, polled_at, revision);
                    let a = a.await?;
                    if let Assessment::Blocked(reason) = a {
                        let d = seen.map(|seen| holds::Decision {
                            project,
                            task,
                            reason,
                            seen,
                            attempt: None,
                        });
                        holds::hold(pool, d, at).await;
                    }
                    Ok(a)
                })
            })
            .await?
            {
                chosen = Some((policy, record, ready, task, assessment));
                break;
            }
        }
        let Some((policy, record, ready, task, assessment)) = chosen else {
            return Ok(page);
        };
        let replacement = assessment.replacement().cloned();
        // R6: a successor's hold is recorded against the attempt count read before its refusal.
        let successor = match &replacement {
            Some(r) => Some(holds::Successor {
                project: policy.project_id,
                task: task.task_id,
                predecessor: r.predecessor,
                seen: holds::attempts(&state.store.pool, task.task_id).await,
            }),
            None => None,
        };
        if let Some(replacement) = &replacement {
            tracing::info!(
                task_id = %task.task_id,
                predecessor = %replacement.predecessor,
                attempts_started = replacement.attempts_started,
                lineage_elapsed_ms = replacement.elapsed_ms,
                sandbox_ms_charged = replacement.sandbox_ms,
                max_attempts = replacement::MAX_ATTEMPTS,
                max_sandbox_ms = replacement::MAX_SANDBOX_MS,
                "replacement attempt admitted within the lineage budget"
            );
            // The failed predecessor's claim is released under its locks before
            // the successor's generation; a later refusal retries idempotently.
            on_heap(|| replacement::release(state, task.task_id, replacement)).await?;
        }
        let generation =
            on_heap(|| super::super::no_grant::next_generation(&state.store.pool, task.task_id))
                .await?;
        let context = on_heap(|| {
            graph.scheduling_context_current(&task, 1, 64, 1_048_576, state.clock.as_ref())
        })
        .await
        .map_err(|_| refused())?;
        let compiled =
            on_heap(|| compose::compile(state, &record, auth.id(), &snapshot, &context)).await?;
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
                .ok_or_else(|| refused())?
                .node,
        )?;
        let decision = on_heap(|| selection(state, &task, &compiled, replacement.as_ref())).await?;
        if refused_successor(state, &decision, successor, &mut skipped).await {
            continue;
        }
        let selected = decision
            .outcome
            .resolved
            .as_ref()
            .ok_or_else(|| refused())?;
        let composition = compiled
            .candidates
            .iter()
            .find(|c| c.digest == selected.choice.composition_digest)
            .ok_or_else(|| refused())?;
        // Qualification and exact startup counting precede expensive source export.
        super::super::context_renewal::evaluate(
            state,
            &runtime,
            selected,
            &composition.composition,
            now(state)?,
        )
        .await?;
        let policy_record = config
            .project_policy_record(policy.project_id)
            .map_err(|_| refused())?;
        // Spec 0007: a handed-off predecessor's successor starts from its saved work.
        let handoff = match &replacement {
            Some(r) => {
                on_heap(|| super::super::handoff::successor_base(state, task.task_id, r)).await?
            }
            None => None,
        };
        let (token, fact) = if let Some(replacement) =
            replacement.as_ref().filter(|_| handoff.is_none())
        {
            // Reuse the original verified export, including its bundle bytes and
            // operation identity. Never resolve a moving ref or re-export on retry.
            on_heap(|| replacement::retained_source(state, task.task_id, replacement))
                .await?
                .into_parts()
        } else {
            // A first attempt freezes the configured source at offer time; a
            // handoff successor freezes the saved commit as a fresh export (a new
            // operation, never the predecessor's export).
            let fresh = async {
                let observed = match &handoff {
                    Some(base) => {
                        on_heap(|| {
                            source::observe_handoff(state, &compiled.frozen, &policy_record, base)
                        })
                        .await?
                    }
                    None => {
                        on_heap(|| source::observe(state, &compiled.frozen, &policy_record)).await?
                    }
                };
                let frozen = match &handoff {
                    Some(base) => Some(base.oid.as_str()),
                    None => frozen_commit(&policy.source, &observed),
                };
                let operation = source_operation(
                    policy.project_id,
                    &task,
                    record.revision,
                    generation,
                    config
                        .project_policy_ref(policy.project_id)
                        .map_err(|_| refused())?,
                    frozen,
                )?;
                let reserved = on_heap(|| reservation(state, &compiled, operation, observed));
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
                // Q1 (live acceptance D5): a refused handoff export skips the task.
                // #116 I1, M1: a counted skip; held as `handoff_successor_stuck` once stuck.
                Err(error @ AppError::Conflict(_)) if handoff.is_some() => {
                    let why = format!("handoff successor export refused: {error:?}");
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
        // External export can consume the original graph/selection freshness. Read
        // actual owners again; a task CAS/content change refuses this original task.
        let ready = on_heap(|| {
            graph.scheduling_ready_current(
                record.binding.graph_workspace_id,
                8,
                None,
                state.clock.as_ref(),
            )
        })
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
        let compiled =
            on_heap(|| compose::compile(state, &record, auth.id(), &snapshot, &context)).await?;
        let decision = on_heap(|| selection(state, &task, &compiled, replacement.as_ref())).await?;
        if refused_successor(state, &decision, successor, &mut skipped).await {
            continue;
        }
        if replacement.is_some() {
            holds::cleared(&state.store.pool, task.task_id).await;
        }
        let source = if replacement.is_some() {
            on_heap(|| {
                source::observe_retained(state, &compiled.frozen, &policy_record, token.request())
            })
            .await?
        } else {
            on_heap(|| source::observe(state, &compiled.frozen, &policy_record)).await?
        };
        if source.admission.resolved_object.hex != token.request().base.hex
            || source.admission.repository_binding_generation != token.request().binding_generation
        {
            return Err(refused());
        }
        let built = on_heap(|| {
            build::assemble(build::Assembly {
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
        })
        .await?;
        // The node's authenticated registration must qualify the exact
        // confinement mode this manifest requires (MS1 review should-fix 1).
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
        let limits = &built.native.offered.offer.manifest.resources;
        if limits.memory_bytes > request.available.memory_bytes
            || limits.cpu_millis > request.available.cpu_millis
            || fact.bundle_bytes() > request.available.disk_bytes
        {
            return Err(refused());
        }
        let inspection = on_heap(|| profiled_base::inspect_retained(state, &token, &fact)).await?;
        let offer =
            on_heap(|| publication::publish(state, auth, built, &token, &fact, &inspection))
                .await?;
        let Some(offer) = offer else { return Ok(page) };
        page.server_time_ms = offer.offered.clock.server_time_ms;
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
        profiled_execution::no_grant::{AttemptCounts, MAX_UNGRANTED_RETRIES},
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
            let selected = first_eligible_task(&ready, &Default::default(), |_| {
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
            let selected = first_eligible_task(&ready, &Default::default(), |_| {
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
            },
            AttemptCounts {
                started: 0,
                history: MAX_UNGRANTED_RETRIES,
                never_granted: MAX_UNGRANTED_RETRIES,
            },
            // An owner-verified never-granted closure may retry below the cap.
            AttemptCounts {
                started: 0,
                history: 1,
                never_granted: 1,
            },
        ];
        let mut observed = Vec::new();
        let selected = first_eligible_task(&ready, &Default::default(), |task| {
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

    #[test]
    fn a_poll_skips_at_most_max_skipped_refused_successors() {
        let mut skipped = std::collections::BTreeSet::new();
        while !skips_spent(&skipped) {
            skipped.insert(Uuid::new_v4());
        }
        assert_eq!(skipped.len(), MAX_SKIPPED);
    }
    #[test]
    fn named_ref_resolves_to_exact_commit_at_offer() {
        let project = Uuid::from_u128(7);
        let task = aq_admission::TaskRevision {
            workspace_id: Uuid::from_u128(8),
            task_id: Uuid::from_u128(9),
            version: 1,
            content_revision: 1,
        };
        let policy_ref = "policy";
        let (a, b) = ("a".repeat(40), "b".repeat(40));
        let key = |commit: Option<&str>| {
            source_operation(project, &task, 3, 1, policy_ref, commit).unwrap()
        };
        // The commit resolved at offer time is part of the export identity:
        // a moved ref is a new operation, never the old one following the ref.
        assert_ne!(key(Some(&a)), key(Some(&b)));
        assert_eq!(key(Some(&a)), key(Some(&a)));
        assert_ne!(key(Some(&a)), key(None));
        // Immutable policies keep their historical operation key.
        assert_eq!(
            key(None),
            Uuid::new_v5(
                &Uuid::from_bytes(*b"aq-native-export"),
                &canonical(&(project, &task, 3_u64, 1_u64, policy_ref)).unwrap()
            )
        );
    }

    #[tokio::test]
    async fn ready_selection_returns_empty_when_blocked_but_never_skips_owner_read_errors() {
        let ready = ready(2);
        let held = AttemptCounts {
            started: 1,
            history: 1,
            never_granted: 0,
        };
        assert!(replacement::precheck(&held).is_none());
        assert!(
            first_eligible_task(&ready, &Default::default(), |_| {
                std::future::ready(Ok(Assessment::Blocked(
                    "replacement_prior_execution_uncertain",
                )))
            })
            .await
            .unwrap()
            .is_none()
        );
        let mut reads = 0;
        let result = first_eligible_task(&ready, &Default::default(), |_| {
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
