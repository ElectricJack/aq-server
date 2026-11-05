//! H4 durable waits registry. One `aq_wait_engine::Snapshot` per wait, mutated
//! only under the row lock with a compare-and-swap on the engine revision. The
//! outcome and its resume intent commit in the same transaction as the
//! snapshot that produced them.
use crate::{
    AppState,
    execution::authority::{AuthorityError, Policy, load_attempt_locked},
};
use aq_wait_engine::{
    self as engine, Action, AuthorityFence, DependencyKey, Input, KnownGraph, PolicyBody,
    ResolvedPolicy, Snapshot, StableIds, TransitionPlan, wire,
};
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;
pub mod routes;
mod sources;

/// Stable error code for transport failures; wire refusals are responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WaitError(pub &'static str);
pub type Result<T> = std::result::Result<T, WaitError>;
impl From<sqlx::Error> for WaitError {
    fn from(_: sqlx::Error) -> Self {
        Self("internal")
    }
}
impl From<serde_json::Error> for WaitError {
    fn from(_: serde_json::Error) -> Self {
        Self("internal")
    }
}
impl From<wire::ContractError> for WaitError {
    fn from(_: wire::ContractError) -> Self {
        Self("internal")
    }
}
impl From<engine::Error> for WaitError {
    fn from(_: engine::Error) -> Self {
        Self("internal")
    }
}

/// Bounded register/drive rounds inside one transaction (install + reobserve).
const ROUNDS: usize = 4;
const HOLD_RECHECK_MS: u64 = 60_000;

pub(crate) fn text(s: impl Into<String>) -> wire::Text {
    wire::Text::new(s).expect("static wait text")
}
pub(crate) fn artifact(id: impl Into<String>, digest: wire::Digest) -> wire::ArtifactRef {
    wire::ArtifactRef {
        id: text(id),
        version: text("1"),
        digest,
    }
}

/// The only policy this server resolves. Any other reference is refused.
pub fn default_policy() -> ResolvedPolicy {
    let body = PolicyBody {
        maximum_age_ms: engine::MAX_AGE_MS,
        check_interval_ms: 60_000,
        retry_base_ms: 1_000,
        retry_cap_ms: 60_000,
        max_retries: 64,
        max_work_attempts: 65_536,
    };
    ResolvedPolicy {
        reference: artifact(
            "aq-server-wait-policy-default",
            wire::canonical_digest(&body).expect("policy digest"),
        ),
        body,
    }
}

/// The authenticated per-attempt caller, re-derived from authority each time.
pub(crate) struct Caller {
    pub fence: AuthorityFence,
    pub key: String,
}
impl Caller {
    fn scope(&self) -> &wire::Scope {
        &self.fence.scope
    }
    fn binding(&self) -> &wire::Binding {
        &self.fence.binding
    }
}
/// The lifecycle Binding of an attempt: its graph workspace fixes the lineage.
/// Shared by waits, hibernation and lifecycle artifact retention so every
/// surface derives the same binding digest for one attempt.
pub(crate) fn binding_for(
    view: &crate::execution::authority::AuthorityView,
) -> Result<wire::Binding> {
    let graph = view.graph.as_ref().ok_or(WaitError("scope_denied"))?;
    let binding = wire::Binding {
        project_id: view.project_id,
        task_id: view.task_id,
        lineage_id: aq_admission::transport::task_lineage_id(
            view.project_id,
            graph.workspace_id,
            view.task_id,
        )
        .map_err(|_| WaitError("scope_denied"))?,
        attempt_id: view.attempt.attempt_id,
        attempt_generation: view.attempt.generation,
        session: view.session.clone(),
    };
    binding.validate().map_err(|_| WaitError("scope_denied"))?;
    Ok(binding)
}
pub(crate) async fn load_caller(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    attempt: Uuid,
) -> Result<Caller> {
    let view = load_attempt_locked(state, tx, node, attempt, Policy::IdentityOnly)
        .await
        .map_err(|error| match error {
            AuthorityError::NotFound | AuthorityError::Identity => WaitError("unauthorized"),
            AuthorityError::Revoked => WaitError("session_revoked"),
            AuthorityError::Expired => WaitError("session_expired"),
            AuthorityError::StaleBoot => WaitError("stale_generation"),
            AuthorityError::Unsupported => WaitError("scope_denied"),
            _ => WaitError("internal"),
        })?;
    let graph = view.graph.as_ref().ok_or(WaitError("scope_denied"))?;
    let binding = binding_for(&view)?;
    let scope = wire::Scope {
        project_id: view.project_id,
        workspace_id: graph.workspace_id,
    };
    let principal = wire::Digest::of(&serde_json::to_vec(&binding)?);
    let source_access = wire::Digest::of(
        format!(
            "agent-wait-source-access:{}:{}",
            scope.project_id, scope.workspace_id
        )
        .as_bytes(),
    );
    Ok(Caller {
        key: principal.as_str().to_owned(),
        fence: AuthorityFence {
            scope,
            binding,
            principal,
            source_access,
            task_cancelled: view.cancellation_id.is_some(),
        },
    })
}

/// The wire scope and binding an attempt's credential authorizes; agents and
/// tests use it to build a registration the server will accept.
pub async fn caller_identity(
    state: &AppState,
    node: Uuid,
    attempt: Uuid,
) -> Result<(wire::Scope, wire::Binding)> {
    let mut tx = state.store.pool.begin().await?;
    let caller = load_caller(state, &mut tx, node, attempt).await?;
    tx.rollback().await?;
    Ok((caller.scope().clone(), caller.binding().clone()))
}

fn respond(request: &wire::Request, body: wire::ResponseBody) -> wire::Response {
    wire::Response {
        api_version: wire::API_VERSION,
        operation_id: request.body.operation_id,
        request_digest: request.body_digest.clone(),
        body,
    }
}
fn refused(request: &wire::Request, code: wire::RefusalCode) -> wire::Response {
    respond(
        request,
        wire::ResponseBody::Refused {
            code,
            effect: wire::Effect::NotApplied,
        },
    )
}
fn contract_refusal(error: &wire::ContractError) -> wire::RefusalCode {
    match error {
        wire::ContractError::Invalid("self_wait") => wire::RefusalCode::SelfWait,
        wire::ContractError::Unsupported => wire::RefusalCode::UnsupportedCapability,
        wire::ContractError::Stale => wire::RefusalCode::StaleGeneration,
        _ => wire::RefusalCode::InvalidRequest,
    }
}
fn supported(condition: &wire::Condition) -> bool {
    matches!(
        condition,
        wire::Condition::Task { .. }
            | wire::Condition::Dependency { .. }
            | wire::Condition::Message { .. }
            | wire::Condition::HumanDecision { .. }
    )
}
fn phase_name(snapshot: &Snapshot) -> Result<String> {
    Ok(serde_json::to_value(snapshot.phase())?
        .as_str()
        .ok_or(WaitError("internal"))?
        .to_owned())
}
/// Row scheduling: held waits only need a recheck for cancellation/deadline.
fn row_due(snapshot: &Snapshot, now: u64) -> u64 {
    match snapshot.phase() {
        engine::Phase::RecoveryHeld => now.saturating_add(HOLD_RECHECK_MS),
        _ => snapshot.next_due_ms(),
    }
    .min(snapshot.deadline_ms())
}
pub(crate) fn i64_of(v: u64) -> Result<i64> {
    i64::try_from(v).map_err(|_| WaitError("internal"))
}

fn task_dependencies(registration: &wire::WaitRegistration) -> Vec<DependencyKey> {
    let project = registration.binding.project_id;
    registration
        .predicate
        .members()
        .iter()
        .filter_map(|member| match member.condition {
            wire::Condition::Task { task_id, .. } => Some(DependencyKey::Task {
                project,
                task: task_id,
            }),
            wire::Condition::Dependency {
                depends_on_task_id, ..
            } => Some(DependencyKey::Task {
                project,
                task: depends_on_task_id,
            }),
            _ => None,
        })
        .collect()
}
/// Known component: the task edges of every active wait in the project. The
/// caller holds the per-project advisory lock, so the proof is serialized with
/// the registration write.
async fn known_graph(
    tx: &mut Transaction<'_, Postgres>,
    scope: &wire::Scope,
) -> Result<KnownGraph> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 76))")
        .bind(format!("agent-waits:{}", scope.project_id))
        .execute(&mut **tx)
        .await?;
    let rows = sqlx::query(
        "SELECT snapshot FROM agent_waits WHERE project_id=$1 AND phase<>'terminal' ORDER BY wait_id",
    )
    .bind(scope.project_id)
    .fetch_all(&mut **tx)
    .await?;
    let mut edges = Vec::new();
    for row in rows {
        let snapshot = Snapshot::decode(&row.get::<Vec<u8>, _>("snapshot"))?;
        let registration = snapshot.registration();
        let owner = DependencyKey::Task {
            project: registration.binding.project_id,
            task: registration.binding.task_id,
        };
        for dependency in task_dependencies(registration) {
            edges.push((owner.clone(), dependency));
        }
    }
    Ok(KnownGraph {
        scope: scope.clone(),
        complete_known_component: true,
        revision: artifact(
            "agent-wait-graph",
            wire::Digest::of(&serde_json::to_vec(&edges)?),
        ),
        edges,
        external_unknown: false,
    })
}

/// Runs bounded source rounds. Returns the final snapshot plus every
/// non-source action (outcome/intent effects) that must commit with it.
async fn settle(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    mut plan: TransitionPlan,
    fence: &AuthorityFence,
    now: u64,
) -> Result<(Snapshot, Vec<Action>)> {
    let mut effects = Vec::new();
    for round in 0..=ROUNDS {
        let mut work = None;
        for action in std::mem::take(&mut plan.actions) {
            match action {
                Action::Install { ticket, sources } | Action::Reobserve { ticket, sources } => {
                    work = Some((ticket, sources))
                }
                other => effects.push(other),
            }
        }
        let Some((ticket, requests)) = work.filter(|_| round < ROUNDS) else {
            return Ok((plan.next, effects));
        };
        let mut replies = Vec::with_capacity(requests.len());
        for request in &requests {
            replies.push(sources::observe(state, tx, request, now).await?);
        }
        plan = plan
            .next
            .step(Input::SourceBatch { ticket, replies }, fence, now)?;
    }
    Ok((plan.next, effects))
}
async fn persist_effects(
    tx: &mut Transaction<'_, Postgres>,
    wait_id: Uuid,
    effects: &[Action],
) -> Result<()> {
    for effect in effects {
        match effect {
            Action::PersistOutcome { outcome, resume } => {
                let kind = serde_json::to_value(&outcome.kind)?;
                sqlx::query("INSERT INTO agent_wait_outcomes(wait_id,outcome_id,kind,outcome,digest,recorded_at_ms) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(wait_id) DO NOTHING")
                    .bind(wait_id)
                    .bind(outcome.outcome.outcome_id)
                    .bind(kind.as_str().unwrap_or("unknown"))
                    .bind(serde_json::to_value(outcome)?)
                    .bind(outcome.outcome.digest.as_str())
                    .bind(i64_of(outcome.recorded_at_ms)?)
                    .execute(&mut **tx)
                    .await?;
                if let Some(resume) = resume {
                    sqlx::query("INSERT INTO agent_wait_resume_intents(intent_id,wait_id,continuation_id,continuation_generation,outcome_id,state) VALUES($1,$2,$3,$4,$5,'ready') ON CONFLICT(wait_id) DO NOTHING")
                        .bind(resume.intent_id)
                        .bind(wait_id)
                        .bind(resume.continuation.continuation_id)
                        .bind(i64_of(resume.continuation.generation)?)
                        .bind(resume.outcome.outcome_id)
                        .execute(&mut **tx)
                        .await?;
                }
            }
            Action::RetireResume { intent_id, .. } => {
                sqlx::query("UPDATE agent_wait_resume_intents SET state='retired' WHERE intent_id=$1 AND state='ready'")
                    .bind(intent_id)
                    .execute(&mut **tx)
                    .await?;
            }
            Action::Install { .. } | Action::Reobserve { .. } => {}
        }
    }
    Ok(())
}

fn registered(snapshot: &Snapshot) -> Result<wire::RegisteredWait> {
    let registration = snapshot.registration();
    let wait = registration.wait_ref();
    let hold = wire::Digest::of(
        format!(
            "agent-wait-hold:{}:{}:{}",
            wait.wait_id,
            registration.wait_generation,
            snapshot.original_created_at_ms()
        )
        .as_bytes(),
    );
    let value = wire::RegisteredWait {
        scope: registration.scope.clone(),
        binding: registration.binding.clone(),
        wait: wait.clone(),
        predicate: registration.predicate.clone(),
        continuation: wire::ContinuationRef {
            continuation_id: snapshot.ids().continuation_id,
            generation: registration.continuation_generation,
            wait,
        },
        original_created_at_ms: snapshot.original_created_at_ms(),
        central_hold_receipt: artifact(format!("agent-wait-hold-{}", registration.wait_id), hold),
        boundaries: snapshot.boundaries().ok_or(WaitError("internal"))?,
        cycle_assessment: snapshot.cycle().clone(),
        execution: wire::WaitExecutionState::PolicyPending,
    };
    value.validate()?;
    Ok(value)
}
use wire::Validate;

/// Register (or replay) a wait for the authenticated attempt.
pub async fn register(
    state: &AppState,
    node: Uuid,
    attempt: Uuid,
    request: &wire::Request,
) -> Result<wire::Response> {
    let mut tx = state.store.pool.begin().await?;
    let caller = load_caller(state, &mut tx, node, attempt).await?;
    let response = register_locked(state, &mut tx, &caller, request).await?;
    tx.commit().await?;
    Ok(response)
}
pub(crate) async fn register_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    caller: &Caller,
    request: &wire::Request,
) -> Result<wire::Response> {
    if let Err(error) = request.validate() {
        return Ok(refused(request, contract_refusal(&error)));
    }
    let wire::Command::RegisterWait(registration) = &request.body.command else {
        return Ok(refused(request, wire::RefusalCode::InvalidRequest));
    };
    if let Some(row) = sqlx::query(
        "SELECT snapshot,response FROM agent_waits WHERE caller_key=$1 AND operation_id=$2 FOR UPDATE",
    )
    .bind(&caller.key)
    .bind(request.body.operation_id)
    .fetch_optional(&mut **tx)
    .await?
    {
        let snapshot = Snapshot::decode(&row.get::<Vec<u8>, _>("snapshot"))?;
        return Ok(match snapshot.replay_registration(request, &caller.fence) {
            Ok(_) => serde_json::from_value(row.get("response"))?,
            Err(engine::Error::Conflict) => refused(request, wire::RefusalCode::ChangedPayload),
            Err(_) => refused(request, wire::RefusalCode::StaleGeneration),
        });
    }
    if &registration.scope != caller.scope() || &registration.binding != caller.binding() {
        return Ok(refused(request, wire::RefusalCode::ScopeDenied));
    }
    if caller.fence.task_cancelled {
        return Ok(refused(request, wire::RefusalCode::StaleGeneration));
    }
    if !registration
        .predicate
        .members()
        .iter()
        .all(|member| supported(&member.condition))
    {
        return Ok(refused(request, wire::RefusalCode::UnsupportedCapability));
    }
    if let Err(error) = registration.reject_self(Some(request.body.operation_id), None) {
        return Ok(refused(request, contract_refusal(&error)));
    }
    let policy = default_policy();
    if registration.policy != policy.reference {
        return Ok(refused(request, wire::RefusalCode::InvalidRequest));
    }
    let taken: Option<i32> = sqlx::query_scalar(
        "SELECT 1 FROM agent_waits WHERE wait_id=$1 OR (attempt_id=$2 AND phase<>'terminal') LIMIT 1",
    )
    .bind(registration.wait_id)
    .bind(registration.binding.attempt_id)
    .fetch_optional(&mut **tx)
    .await?;
    if taken.is_some() {
        // Another wait id, or a second active wait for this attempt.
        return Ok(refused(request, wire::RefusalCode::InvalidRequest));
    }
    let graph = known_graph(tx, caller.scope()).await?;
    let now = super::now(state);
    let ids = StableIds {
        continuation_id: Uuid::new_v4(),
        outcome_id: Uuid::new_v4(),
        resume_intent_id: Uuid::new_v4(),
    };
    let plan = match Snapshot::begin(request.clone(), &caller.fence, policy, ids, &graph, now) {
        Ok(plan) => plan,
        Err(engine::Error::Cycle | engine::Error::Capacity) => {
            return Ok(refused(request, wire::RefusalCode::KnownCycle));
        }
        Err(engine::Error::Stale) => {
            return Ok(refused(request, wire::RefusalCode::StaleGeneration));
        }
        Err(_) => return Ok(refused(request, wire::RefusalCode::InvalidRequest)),
    };
    let (snapshot, effects) = settle(state, tx, plan, &caller.fence, now).await?;
    if snapshot.boundaries().is_none() {
        // Central hold could not be installed against every source: nothing
        // is persisted and the caller may retry the same operation.
        return Ok(refused(request, wire::RefusalCode::SourceUnavailable));
    }
    let response = respond(
        request,
        wire::ResponseBody::WaitRegistered(Box::new(registered(&snapshot)?)),
    );
    response.validate_against(request)?;
    sqlx::query("INSERT INTO agent_waits(wait_id,caller_key,operation_id,project_id,workspace_id,task_id,attempt_id,node_id,wait_generation,continuation_generation,predicate_digest,request_digest,phase,revision,next_due_ms,deadline_ms,created_at_ms,snapshot,response) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17,$18,$19)")
        .bind(registration.wait_id)
        .bind(&caller.key)
        .bind(request.body.operation_id)
        .bind(registration.scope.project_id)
        .bind(registration.scope.workspace_id)
        .bind(registration.binding.task_id)
        .bind(registration.binding.attempt_id)
        .bind(session_node(&registration.binding)?)
        .bind(i64_of(registration.wait_generation)?)
        .bind(i64_of(registration.continuation_generation)?)
        .bind(registration.predicate_digest.as_str())
        .bind(request.body_digest.as_str())
        .bind(phase_name(&snapshot)?)
        .bind(i64_of(snapshot.revision())?)
        .bind(i64_of(row_due(&snapshot, now))?)
        .bind(i64_of(snapshot.deadline_ms())?)
        .bind(i64_of(snapshot.original_created_at_ms())?)
        .bind(snapshot.encode()?)
        .bind(serde_json::to_value(&response)?)
        .execute(&mut **tx)
        .await?;
    persist_effects(tx, registration.wait_id, &effects).await?;
    Ok(response)
}
fn session_node(binding: &wire::Binding) -> Result<Uuid> {
    match serde_json::to_value(&binding.session)?["node_id"].as_str() {
        Some(node) => Uuid::parse_str(node).map_err(|_| WaitError("scope_denied")),
        None => Err(WaitError("scope_denied")),
    }
}

/// Drive one due wait under its row lock. Returns whether a step committed.
pub async fn drive(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    wait_id: Uuid,
    now: u64,
) -> Result<bool> {
    let Some(row) = sqlx::query("SELECT snapshot,node_id,attempt_id,revision FROM agent_waits WHERE wait_id=$1 AND phase<>'terminal' FOR UPDATE SKIP LOCKED")
        .bind(wait_id)
        .fetch_optional(&mut **tx)
        .await?
    else {
        return Ok(false);
    };
    let snapshot = Snapshot::decode(&row.get::<Vec<u8>, _>("snapshot"))?;
    let revision: i64 = row.get("revision");
    let caller = match load_caller(state, tx, row.get("node_id"), row.get("attempt_id")).await {
        Ok(caller) => caller,
        Err(WaitError("internal")) => return Err(WaitError("internal")),
        Err(_) => {
            // Authority unreadable now: keep the wait, recheck later.
            sqlx::query("UPDATE agent_waits SET next_due_ms=$2 WHERE wait_id=$1")
                .bind(wait_id)
                .bind(i64_of(
                    now.saturating_add(HOLD_RECHECK_MS)
                        .min(snapshot.deadline_ms()),
                )?)
                .execute(&mut **tx)
                .await?;
            return Ok(false);
        }
    };
    let mut effects = Vec::new();
    let mut current = snapshot;
    if let Some(ticket) = current.inflight().cloned() {
        // The row lock proves no other worker owns the old ticket.
        let reclaimed = current.step(Input::Reclaim { ticket }, &caller.fence, now)?;
        effects.extend(reclaimed.actions);
        current = reclaimed.next;
    }
    let plan = current.step(Input::Drive, &caller.fence, now)?;
    let (next, more) = settle(state, tx, plan, &caller.fence, now).await?;
    effects.extend(more);
    let updated = sqlx::query("UPDATE agent_waits SET snapshot=$2,phase=$3,revision=$4,next_due_ms=$5 WHERE wait_id=$1 AND revision=$6")
        .bind(wait_id)
        .bind(next.encode()?)
        .bind(phase_name(&next)?)
        .bind(i64_of(next.revision())?)
        .bind(i64_of(row_due(&next, now))?)
        .bind(revision)
        .execute(&mut **tx)
        .await?;
    if updated.rows_affected() != 1 {
        return Err(WaitError("stale_generation"));
    }
    persist_effects(tx, wait_id, &effects).await?;
    Ok(true)
}
/// Periodic reconciler: drive at most `limit` due waits, one transaction each.
pub async fn reconcile_due(state: &AppState, limit: i64) -> Result<usize> {
    let now = super::now(state);
    let due: Vec<Uuid> = sqlx::query_scalar("SELECT wait_id FROM agent_waits WHERE phase<>'terminal' AND next_due_ms<=$1 ORDER BY next_due_ms,wait_id LIMIT $2")
        .bind(i64_of(now)?)
        .bind(limit)
        .fetch_all(&state.store.pool)
        .await?;
    let mut driven = 0;
    for wait_id in due {
        let mut tx = state.store.pool.begin().await?;
        if drive(state, &mut tx, wait_id, now).await? {
            driven += 1;
        }
        tx.commit().await?;
    }
    Ok(driven)
}
/// Background loop; the periodic reconcile is the source of liveness.
pub async fn run(state: AppState, mut stop: tokio::sync::watch::Receiver<bool>) {
    loop {
        if *stop.borrow() {
            return;
        }
        if let Err(error) = reconcile_due(&state, 64).await {
            tracing::warn!(code = error.0, "agent wait reconcile failed");
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            _ = stop.changed() => {}
        }
    }
}

pub(crate) struct Stored {
    pub snapshot: Snapshot,
    pub registered: wire::RegisteredWait,
    pub intent_state: Option<String>,
    pub ack_receipt: Option<wire::ArtifactRef>,
}
pub(crate) async fn load_stored(
    tx: &mut Transaction<'_, Postgres>,
    wait_id: Uuid,
    lock: bool,
) -> Result<Option<Stored>> {
    let sql = if lock {
        "SELECT snapshot,response FROM agent_waits WHERE wait_id=$1 FOR UPDATE"
    } else {
        "SELECT snapshot,response FROM agent_waits WHERE wait_id=$1"
    };
    let Some(row) = sqlx::query(sql)
        .bind(wait_id)
        .fetch_optional(&mut **tx)
        .await?
    else {
        return Ok(None);
    };
    let snapshot = Snapshot::decode(&row.get::<Vec<u8>, _>("snapshot"))?;
    let response: wire::Response = serde_json::from_value(row.get("response"))?;
    let wire::ResponseBody::WaitRegistered(registered) = response.body else {
        return Err(WaitError("internal"));
    };
    let intent = sqlx::query("SELECT i.state,a.response FROM agent_wait_resume_intents i LEFT JOIN agent_wait_acks a USING(intent_id) WHERE i.wait_id=$1")
        .bind(wait_id)
        .fetch_optional(&mut **tx)
        .await?;
    let (intent_state, ack_receipt) = match intent {
        Some(row) => {
            let ack: Option<Value> = row.get("response");
            let receipt = ack
                .map(|v| {
                    serde_json::from_value(v["body"]["body"]["acknowledgment_receipt"].clone())
                })
                .transpose()?;
            (Some(row.get::<String, _>("state")), receipt)
        }
        None => (None, None),
    };
    Ok(Some(Stored {
        snapshot,
        registered: *registered,
        intent_state,
        ack_receipt,
    }))
}
/// H5: one wait's registered continuation and its recorded outcome, if any.
pub(crate) struct WaitContinuation {
    pub binding: wire::Binding,
    pub node_id: Uuid,
    pub continuation: wire::ContinuationRef,
    pub terminal: bool,
    pub outcome: Option<wire::OutcomeRef>,
    pub intent_ready: bool,
}
pub(crate) async fn continuation_of(
    tx: &mut Transaction<'_, Postgres>,
    wait_id: Uuid,
) -> Result<Option<WaitContinuation>> {
    let Some(stored) = load_stored(tx, wait_id, false).await? else {
        return Ok(None);
    };
    let row = sqlx::query("SELECT node_id,phase FROM agent_waits WHERE wait_id=$1")
        .bind(wait_id)
        .fetch_one(&mut **tx)
        .await?;
    Ok(Some(WaitContinuation {
        binding: stored.registered.binding.clone(),
        node_id: row.get("node_id"),
        continuation: stored.registered.continuation.clone(),
        terminal: row.get::<String, _>("phase") == "terminal",
        outcome: stored.snapshot.outcome().map(|o| o.outcome.clone()),
        intent_ready: stored.intent_state.as_deref() == Some("ready"),
    }))
}
fn view(stored: &Stored) -> Result<wire::WaitView> {
    let outcome = stored.snapshot.outcome().cloned();
    let cancelled = matches!(stored.intent_state.as_deref(), Some("retired"))
        || outcome
            .as_ref()
            .is_some_and(|o| o.kind == wire::WaitOutcomeKind::Cancelled);
    let continuation = if let Some(receipt) = &stored.ack_receipt {
        wire::ContinuationState::BootstrapAcknowledged {
            receipt: receipt.clone(),
        }
    } else if cancelled {
        wire::ContinuationState::Cancelled
    } else if let Some(reason) = stored.snapshot.hold_reason() {
        wire::ContinuationState::Held {
            reason: reason.clone(),
        }
    } else {
        wire::ContinuationState::PendingPolicy
    };
    let value = wire::WaitView {
        registration: stored.registered.clone(),
        outcome,
        continuation,
        parking_report: None,
    };
    value.validate()?;
    Ok(value)
}

/// Agent inspection of its own task's wait (same task lineage, any attempt).
pub async fn inspect(
    state: &AppState,
    node: Uuid,
    attempt: Uuid,
    request: &wire::Request,
) -> Result<wire::Response> {
    let mut tx = state.store.pool.begin().await?;
    let caller = load_caller(state, &mut tx, node, attempt).await?;
    if let Err(error) = request.validate() {
        return Ok(refused(request, contract_refusal(&error)));
    }
    let wire::Command::Inspect(inspect) = &request.body.command else {
        return Ok(refused(request, wire::RefusalCode::InvalidRequest));
    };
    let wire::InspectTarget::Wait { wait } = &inspect.target else {
        return Ok(refused(request, wire::RefusalCode::UnsupportedCapability));
    };
    if &inspect.scope != caller.scope() {
        return Ok(refused(request, wire::RefusalCode::ScopeDenied));
    }
    let Some(stored) = load_stored(&mut tx, wait.wait_id, false).await? else {
        return Ok(refused(request, wire::RefusalCode::ScopeDenied));
    };
    let registration = stored.snapshot.registration();
    if !same_lineage(&registration.binding, caller.binding()) {
        return Ok(refused(request, wire::RefusalCode::ScopeDenied));
    }
    if &registration.wait_ref() != wait {
        return Ok(refused(request, wire::RefusalCode::StaleGeneration));
    }
    let page = wire::InspectionPage {
        scope: caller.scope().clone(),
        items: vec![wire::InspectionItem::Wait(Box::new(view(&stored)?))],
        next_cursor: None,
        complete_for_authorized_scope: true,
        reasons: Vec::new(),
    };
    tx.commit().await?;
    Ok(respond(request, wire::ResponseBody::Inspection(page)))
}
fn same_lineage(a: &wire::Binding, b: &wire::Binding) -> bool {
    a.project_id == b.project_id && a.task_id == b.task_id && a.lineage_id == b.lineage_id
}

/// Consume the resume intent exactly once. A duplicate admission returns the
/// original receipt; a stale continuation or outcome consumes nothing.
pub async fn ack(
    state: &AppState,
    node: Uuid,
    attempt: Uuid,
    request: &wire::Request,
) -> Result<wire::Response> {
    let mut tx = state.store.pool.begin().await?;
    let caller = load_caller(state, &mut tx, node, attempt).await?;
    if let Err(error) = request.validate() {
        return Ok(refused(request, contract_refusal(&error)));
    }
    let wire::Command::ContinuationAck(ack) = &request.body.command else {
        return Ok(refused(request, wire::RefusalCode::InvalidRequest));
    };
    if &ack.successor != caller.binding() {
        return Ok(refused(request, wire::RefusalCode::ScopeDenied));
    }
    let ack_digest = wire::canonical_digest(ack)?;
    let Some(stored) = load_stored(&mut tx, ack.continuation.wait.wait_id, true).await? else {
        return Ok(refused(request, wire::RefusalCode::ScopeDenied));
    };
    if !same_lineage(&stored.snapshot.registration().binding, &ack.successor) {
        return Ok(refused(request, wire::RefusalCode::ScopeDenied));
    }
    if let Some(row) =
        sqlx::query("SELECT request_digest,response FROM agent_wait_acks WHERE admission_id=$1")
            .bind(ack.admission_id)
            .fetch_optional(&mut *tx)
            .await?
    {
        if row.get::<String, _>("request_digest") != ack_digest.as_str() {
            return Ok(refused(request, wire::RefusalCode::ChangedPayload));
        }
        let original: wire::Response = serde_json::from_value(row.get("response"))?;
        return Ok(respond(request, original.body));
    }
    let intent = sqlx::query("SELECT intent_id,continuation_id,continuation_generation,outcome_id FROM agent_wait_resume_intents WHERE wait_id=$1 AND state='ready' FOR UPDATE")
        .bind(ack.continuation.wait.wait_id)
        .fetch_optional(&mut *tx)
        .await?;
    let registration = stored.snapshot.registration();
    let current = intent.filter(|row| {
        row.get::<Uuid, _>("continuation_id") == ack.continuation.continuation_id
            && row.get::<i64, _>("continuation_generation") as u64 == ack.continuation.generation
            && row.get::<Uuid, _>("outcome_id") == ack.outcome.outcome_id
            && ack.continuation.wait == registration.wait_ref()
            && stored
                .snapshot
                .outcome()
                .is_some_and(|o| o.outcome == ack.outcome)
    });
    let Some(intent) = current else {
        return Ok(refused(request, wire::RefusalCode::StaleGeneration));
    };
    let intent_id: Uuid = intent.get("intent_id");
    let receipt = artifact(
        format!("agent-wait-ack-{}", ack.admission_id),
        wire::Digest::of(
            format!(
                "agent-wait-ack:{}:{}:{}",
                ack.admission_id,
                intent_id,
                ack.outcome.digest.as_str()
            )
            .as_bytes(),
        ),
    );
    let response = respond(
        request,
        wire::ResponseBody::ContinuationAcknowledged {
            admission_id: ack.admission_id,
            continuation: ack.continuation.clone(),
            acknowledgment_receipt: receipt,
        },
    );
    response.validate_against(request)?;
    sqlx::query("INSERT INTO agent_wait_acks(admission_id,intent_id,request_digest,response,created_at_ms) VALUES($1,$2,$3,$4,$5)")
        .bind(ack.admission_id)
        .bind(intent_id)
        .bind(ack_digest.as_str())
        .bind(serde_json::to_value(&response)?)
        .bind(i64_of(super::now(state))?)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE agent_wait_resume_intents SET state='consumed',admission_id=$2 WHERE intent_id=$1",
    )
    .bind(intent_id)
    .bind(ack.admission_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(response)
}

/// Owner (G10) list: newest first, optionally one project.
pub async fn owner_list(state: &AppState, project: Option<Uuid>) -> Result<Value> {
    let rows = sqlx::query("SELECT w.wait_id,w.project_id,w.workspace_id,w.task_id,w.attempt_id,w.phase,w.revision,w.next_due_ms,w.deadline_ms,w.created_at_ms,o.kind AS outcome_kind,i.state AS intent_state FROM agent_waits w LEFT JOIN agent_wait_outcomes o USING(wait_id) LEFT JOIN agent_wait_resume_intents i USING(wait_id) WHERE ($1::uuid IS NULL OR w.project_id=$1) ORDER BY w.created_at_ms DESC,w.wait_id LIMIT 200")
        .bind(project)
        .fetch_all(&state.store.pool)
        .await?;
    let items: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "wait_id": r.get::<Uuid, _>("wait_id"),
                "project_id": r.get::<Uuid, _>("project_id"),
                "workspace_id": r.get::<Uuid, _>("workspace_id"),
                "task_id": r.get::<Uuid, _>("task_id"),
                "attempt_id": r.get::<Uuid, _>("attempt_id"),
                "phase": r.get::<String, _>("phase"),
                "revision": r.get::<i64, _>("revision"),
                "next_due_ms": r.get::<i64, _>("next_due_ms"),
                "deadline_ms": r.get::<i64, _>("deadline_ms"),
                "created_at_ms": r.get::<i64, _>("created_at_ms"),
                "outcome_kind": r.get::<Option<String>, _>("outcome_kind"),
                "intent_state": r.get::<Option<String>, _>("intent_state"),
            })
        })
        .collect();
    Ok(json!({ "items": items }))
}
/// Owner (G10) detail, including per-member freshness and hold reason.
pub async fn owner_show(state: &AppState, wait_id: Uuid) -> Result<Option<Value>> {
    let mut tx = state.store.pool.begin().await?;
    let Some(stored) = load_stored(&mut tx, wait_id, false).await? else {
        return Ok(None);
    };
    tx.commit().await?;
    Ok(Some(json!({
        "view": view(&stored)?,
        "phase": stored.snapshot.phase(),
        "revision": stored.snapshot.revision(),
        "deadline_ms": stored.snapshot.deadline_ms(),
        "next_due_ms": stored.snapshot.next_due_ms(),
        "hold_reason": stored.snapshot.hold_reason(),
        "members": stored.snapshot.member_states(),
        "intent_state": stored.intent_state,
    })))
}
