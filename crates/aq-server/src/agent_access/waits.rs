//! H4 durable waits registry. One `aq_wait_engine::Snapshot` per wait, mutated
//! only under the row lock with a compare-and-swap on the engine revision. The
//! outcome and its resume intent commit in the same transaction as the
//! snapshot that produced them.
use crate::{
    AppState,
    execution::authority::{AuthorityError, Policy, load_attempt_locked},
};
use aq_wait_engine::{
    self as engine, Action, AuthorityFence, DependencyKey, Input, KnownGraph, ResolvedPolicy,
    Snapshot, StableIds, TransitionPlan, wire,
};
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;
use wire::Validate;
mod acknowledgment;
mod inspection;
mod registration;
mod storage;
pub use inspection::{inspect, owner_list, owner_show};
pub(crate) use storage::{Stored, continuation_of, load_stored};
pub mod routes;
pub use engine::receipts::default_policy;
pub(crate) use engine::receipts::{artifact, text};
use engine::receipts::{contract_refusal, refused, respond};
mod sources;
pub use acknowledgment::ack;
pub(crate) use registration::register_locked;

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

/// Runs bounded source rounds. Returns the final snapshot plus every
/// non-source action (outcome/intent effects) that must commit with it.
struct SourceRounds<'a> {
    state: &'a AppState,
    fence: &'a AuthorityFence,
    now: u64,
}
impl SourceRounds<'_> {
    async fn settle(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        mut plan: TransitionPlan,
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
                replies.push(sources::observe(self.state, tx, request, self.now).await?);
            }
            plan = plan
                .next
                .step(Input::SourceBatch { ticket, replies }, self.fence, self.now)?;
        }
        Ok((plan.next, effects))
    }
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
    let (next, more) = SourceRounds {
        state,
        fence: &caller.fence,
        now,
    }
    .settle(tx, plan)
    .await?;
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

fn same_lineage(a: &wire::Binding, b: &wire::Binding) -> bool {
    a.project_id == b.project_id && a.task_id == b.task_id && a.lineage_id == b.lineage_id
}
