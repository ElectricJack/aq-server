//! H6 hibernation wake scheduling. A resolved H4 wait schedules exactly one
//! fresh continuation (new attempt, session and credentials, with lineage to
//! the predecessor) on any eligible live node. The predecessor's node and
//! sandbox are never required, and placement never prefers a warm start.
//!
//! `resume_pending` is never a running state: a wake runs only once its
//! dispatch generation has a durable admission winner. Each admission
//! serializes against cancellation, completion and supersession under the
//! wake row lock plus the predecessor's hot-authority row lock.
use super::waits::{Result, WaitError, artifact, i64_of, load_caller, load_stored};
use crate::{
    AppState,
    profiled_execution::{producer::fairness, replacement},
};
use aq_wait_engine::wire::{self, Validate, worker_lifecycle as lc};
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
use std::collections::BTreeMap;
use uuid::Uuid;

/// Launch or ack failures tolerated after admission before escalation.
pub const MAX_LAUNCH_FAILURES: i32 = 3;
/// Retention holds tolerated before escalation (retention never self-heals).
pub const MAX_RETENTION_HOLDS: i32 = 3;
/// Recheck delay for a held wake (no eligible node, missing retention).
pub const HOLD_RECHECK_MS: u64 = 30_000;
/// Suspension directive bound (lifecycle protocol maximum).
pub const SUSPEND_DEADLINE_MS: u64 = 300_000;
const MAX_SUMMARY_BYTES: u32 = 16_384;
const MAX_BATCH: i64 = 256;

/// One scheduling pass.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Scheduled {
    pub admitted: usize,
    pub held: usize,
    pub cancelled: usize,
    pub superseded: usize,
    pub escalated: usize,
}

/// The durable winner of one dispatch generation.
#[derive(Debug, Clone)]
pub struct Admission {
    pub admission_id: Uuid,
    pub node_id: Option<Uuid>,
    pub dispatch_generation: u64,
    pub state: String,
    pub attachment: wire::ContinuationAttachment,
    pub differences: Value,
}

fn now(state: &AppState) -> u64 {
    super::now(state)
}

fn retention_policy() -> wire::ArtifactRef {
    artifact(
        "aq-server-retention-policy-default",
        wire::Digest::of(b"agent-wait-retention:central-receipts:source+checkpoint"),
    )
}

/// Issue (or replay) the suspension directive for the caller's own wait.
pub async fn suspend(
    state: &AppState,
    node: Uuid,
    attempt: Uuid,
    wait_id: Uuid,
) -> Result<lc::SuspensionIntent> {
    let mut tx = state.store.pool.begin().await?;
    let caller = load_caller(state, &mut tx, node, attempt).await?;
    let stored = load_stored(&mut tx, wait_id, true)
        .await?
        .ok_or(WaitError("scope_denied"))?;
    if stored.snapshot.registration().binding != caller.fence.binding {
        return Err(WaitError("scope_denied"));
    }
    if let Some(existing) = sqlx::query_scalar::<_, Value>(
        "SELECT directive FROM agent_wait_suspensions WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_optional(&mut *tx)
    .await?
    {
        tx.rollback().await?;
        return Ok(serde_json::from_value(existing)?);
    }
    let issued = now(state);
    let directive = lc::SuspensionIntent {
        binding: caller.fence.binding.clone(),
        continuation: stored.registered.continuation.clone(),
        issued_at_ms: issued,
        deadline_ms: issued + SUSPEND_DEADLINE_MS,
        max_summary_bytes: MAX_SUMMARY_BYTES,
        retention_policy: retention_policy(),
    };
    directive.validate()?;
    sqlx::query("INSERT INTO agent_wait_suspensions(attempt_id,wait_id,directive,disposition,updated_at_ms) VALUES($1,$2,$3,'directed',$4)")
        .bind(attempt)
        .bind(wait_id)
        .bind(serde_json::to_value(&directive)?)
        .bind(i64_of(issued)?)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(directive)
}

/// Accept the predecessor's parking report. `suspended` is neither success
/// nor failure: it writes no result report and no settlement input, and once
/// recorded it is never downgraded by a later report.
pub async fn accept_parking(
    state: &AppState,
    node: Uuid,
    attempt: Uuid,
    report: &lc::ParkingReport,
) -> Result<String> {
    report
        .validate()
        .map_err(|_| WaitError("invalid_request"))?;
    let mut tx = state.store.pool.begin().await?;
    let caller = load_caller(state, &mut tx, node, attempt).await?;
    if report.binding != caller.fence.binding {
        return Err(WaitError("scope_denied"));
    }
    let row = sqlx::query(
        "SELECT directive,disposition FROM agent_wait_suspensions WHERE attempt_id=$1 FOR UPDATE",
    )
    .bind(attempt)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(WaitError("not_directed"))?;
    let directive: lc::SuspensionIntent = serde_json::from_value(row.get("directive"))?;
    if directive.continuation != report.continuation {
        return Err(WaitError("stale_generation"));
    }
    let current: String = row.get("disposition");
    if current == "suspended" {
        tx.rollback().await?;
        return Ok(current);
    }
    let next = match report.disposition {
        lc::SuspensionDisposition::RetainedSuspended => "suspended",
        lc::SuspensionDisposition::Held { .. } => "held",
        lc::SuspensionDisposition::Preparing | lc::SuspensionDisposition::StoppedRetaining => {
            current.as_str()
        }
    }
    .to_owned();
    sqlx::query("UPDATE agent_wait_suspensions SET report=$2,disposition=$3,updated_at_ms=$4 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(serde_json::to_value(report)?)
        .bind(&next)
        .bind(i64_of(now(state))?)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(next)
}

/// Project newly ready H4 intents into wakes, then admit due wakes in fair
/// project order. Retrying after a winner exists inserts nothing.
pub async fn schedule(state: &AppState, limit: i64) -> Result<Scheduled> {
    let limit = limit.clamp(1, MAX_BATCH);
    let now = now(state);
    let at = i64_of(now)?;
    let pool = &state.store.pool;
    sqlx::query("INSERT INTO agent_wait_wakes(intent_id,wait_id,project_id,task_id,predecessor_attempt_id,predecessor_node_id,dispatch_generation,state,wait_created_at_ms,ready_at_ms,next_due_ms,updated_at_ms) \
        SELECT i.intent_id,w.wait_id,w.project_id,w.task_id,w.attempt_id,w.node_id,1,'resume_pending',w.created_at_ms,GREATEST(o.recorded_at_ms,1),0,$1 \
        FROM agent_wait_resume_intents i JOIN agent_waits w USING(wait_id) JOIN agent_wait_outcomes o USING(wait_id) \
        JOIN agent_wait_suspensions s ON s.wait_id=w.wait_id \
        WHERE i.state='ready' AND NOT EXISTS(SELECT 1 FROM agent_wait_wakes x WHERE x.intent_id=i.intent_id) \
        ORDER BY w.created_at_ms,w.wait_id LIMIT $2 ON CONFLICT DO NOTHING")
        .bind(at)
        .bind(limit)
        .execute(pool)
        .await?;
    // Oldest wait first within a project; age is the wait's, never reset.
    let due: Vec<(Uuid, Uuid)> = sqlx::query_as("SELECT intent_id,project_id FROM agent_wait_wakes WHERE state='resume_pending' AND next_due_ms<=$1 ORDER BY manual_override DESC,wait_created_at_ms,intent_id LIMIT $2")
        .bind(at)
        .bind(limit)
        .fetch_all(pool)
        .await?;
    let mut queues: BTreeMap<Uuid, std::collections::VecDeque<Uuid>> = BTreeMap::new();
    for (intent, project) in due {
        queues.entry(project).or_default().push_back(intent);
    }
    let fair = fairness::recorded(pool)
        .await
        .map_err(|_| WaitError("internal"))?;
    let served = if fair {
        fairness::served(pool)
            .await
            .map_err(|_| WaitError("internal"))?
    } else {
        BTreeMap::new()
    };
    let priority = |project: Uuid| {
        state
            .config
            .profiled_execution
            .as_ref()
            .and_then(|c| c.projects.iter().find(|p| p.project_id == project))
            .map_or(0, |p| p.priority)
    };
    let candidates: Vec<fairness::Candidate> = queues
        .keys()
        .map(|&project_id| fairness::Candidate {
            project_id,
            priority: priority(project_id),
        })
        .collect();
    let order = fairness::order(&candidates, &served, now);
    let mut out = Scheduled::default();
    // Round-robin across projects in fair order so one project cannot drain
    // the batch while another waits.
    loop {
        let mut progressed = false;
        for candidate in &order {
            let Some(intent) = queues
                .get_mut(&candidate.project_id)
                .and_then(|q| q.pop_front())
            else {
                continue;
            };
            progressed = true;
            match admit_one(state, intent, now).await? {
                Step::Admitted => {
                    out.admitted += 1;
                    if fair {
                        fairness::record(pool, candidate.project_id, now)
                            .await
                            .map_err(|_| WaitError("internal"))?;
                    }
                }
                Step::Held => out.held += 1,
                Step::Cancelled => out.cancelled += 1,
                Step::Superseded => out.superseded += 1,
                Step::Escalated => out.escalated += 1,
                Step::Skipped => {}
            }
        }
        if !progressed {
            break;
        }
    }
    Ok(out)
}

enum Step {
    Admitted,
    Held,
    Cancelled,
    Superseded,
    Escalated,
    Skipped,
}

async fn finish(
    tx: &mut Transaction<'_, Postgres>,
    intent: Uuid,
    wake_state: &str,
    reason: &str,
    at: i64,
) -> Result<()> {
    sqlx::query(
        "UPDATE agent_wait_wakes SET state=$2,reason=$3,updated_at_ms=$4 WHERE intent_id=$1",
    )
    .bind(intent)
    .bind(wake_state)
    .bind(reason)
    .bind(at)
    .execute(&mut **tx)
    .await?;
    if matches!(wake_state, "cancelled" | "superseded") {
        sqlx::query(
            "UPDATE agent_wait_resume_intents SET state='retired' WHERE intent_id=$1 AND state='ready'",
        )
        .bind(intent)
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn hold(
    tx: &mut Transaction<'_, Postgres>,
    intent: Uuid,
    reason: &str,
    now: u64,
) -> Result<()> {
    sqlx::query("UPDATE agent_wait_wakes SET reason=$2,holds=holds+1,next_due_ms=$3,updated_at_ms=$4 WHERE intent_id=$1")
        .bind(intent)
        .bind(reason)
        .bind(i64_of(now + HOLD_RECHECK_MS)?)
        .bind(i64_of(now)?)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

/// Admit one wake in one transaction. Every early return commits the
/// recorded disposition; only the winner insert makes the wake running.
async fn admit_one(state: &AppState, intent: Uuid, now: u64) -> Result<Step> {
    let at = i64_of(now)?;
    let mut tx = state.store.pool.begin().await?;
    let Some(wake) = sqlx::query("SELECT wait_id,task_id,predecessor_attempt_id,predecessor_node_id,dispatch_generation,launch_failures,holds,wait_created_at_ms FROM agent_wait_wakes WHERE intent_id=$1 AND state='resume_pending' FOR UPDATE SKIP LOCKED")
        .bind(intent)
        .fetch_optional(&mut *tx)
        .await?
    else {
        return Ok(Step::Skipped);
    };
    let wait_id: Uuid = wake.get("wait_id");
    let task_id: Uuid = wake.get("task_id");
    let predecessor_attempt: Uuid = wake.get("predecessor_attempt_id");
    let predecessor_node: Option<Uuid> = wake.get("predecessor_node_id");
    let generation: i64 = wake.get("dispatch_generation");
    let launch_failures: i32 = wake.get("launch_failures");
    let holds: i32 = wake.get("holds");
    let wait_created: i64 = wake.get("wait_created_at_ms");
    // A retry after a durable winner returns that winner, not a second one.
    if sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM agent_wait_wake_admissions WHERE intent_id=$1 AND dispatch_generation=$2)")
        .bind(intent)
        .bind(generation)
        .fetch_one(&mut *tx)
        .await?
    {
        finish(&mut tx, intent, "admitted", "winner_exists", at).await?;
        tx.commit().await?;
        return Ok(Step::Skipped);
    }
    // Serialize against cancellation: its FK insert needs this row's key lock.
    sqlx::query("SELECT 1 FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE")
        .bind(predecessor_attempt)
        .fetch_optional(&mut *tx)
        .await?;
    let intent_state: Option<String> = sqlx::query_scalar(
        "SELECT state FROM agent_wait_resume_intents WHERE intent_id=$1 FOR UPDATE",
    )
    .bind(intent)
    .fetch_optional(&mut *tx)
    .await?;
    match intent_state.as_deref() {
        Some("ready") => {}
        Some("consumed") => {
            finish(&mut tx, intent, "acknowledged", "intent_consumed", at).await?;
            tx.commit().await?;
            return Ok(Step::Skipped);
        }
        _ => {
            finish(&mut tx, intent, "cancelled", "intent_retired", at).await?;
            tx.commit().await?;
            return Ok(Step::Cancelled);
        }
    }
    let cancelled: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM profiled_cancellations WHERE attempt_id=$1) OR EXISTS(SELECT 1 FROM execution_attempts WHERE attempt_id=$1 AND COALESCE(state->'cancellation_id','null'::jsonb)<>'null'::jsonb)")
        .bind(predecessor_attempt)
        .fetch_one(&mut *tx)
        .await?;
    if cancelled {
        finish(&mut tx, intent, "cancelled", "task_cancelled", at).await?;
        tx.commit().await?;
        return Ok(Step::Cancelled);
    }
    let superseded: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM profiled_task_completions WHERE task_id=$1 AND state='confirmed') OR EXISTS(SELECT 1 FROM execution_attempts WHERE task_id=$1 AND holds_task AND attempt_id<>$2)")
        .bind(task_id)
        .bind(predecessor_attempt)
        .fetch_one(&mut *tx)
        .await?;
    if superseded {
        finish(
            &mut tx,
            intent,
            "superseded",
            "task_completed_or_replaced",
            at,
        )
        .await?;
        tx.commit().await?;
        return Ok(Step::Superseded);
    }
    let stored = load_stored(&mut tx, wait_id, true)
        .await?
        .ok_or(WaitError("internal"))?;
    let predecessor = stored.snapshot.registration().binding.clone();
    let outcome = stored
        .snapshot
        .outcome()
        .map(|o| o.outcome.clone())
        .ok_or(WaitError("internal"))?;
    // Retention: the parked predecessor's unexpired central receipts.
    let report: Option<Value> = sqlx::query_scalar(
        "SELECT report FROM agent_wait_suspensions WHERE wait_id=$1 AND disposition='suspended'",
    )
    .bind(wait_id)
    .fetch_optional(&mut *tx)
    .await?
    .flatten();
    let retained: Vec<lc::RetentionReceipt> = match report {
        Some(v) => serde_json::from_value::<lc::ParkingReport>(v)?
            .central_receipts
            .into_iter()
            .filter(|r| r.retained_until_ms > now)
            .collect(),
        None => Vec::new(),
    };
    if !lc::has_migratable_refs(&retained) {
        return held_or_escalated(tx, intent, holds, "retention_missing", now).await;
    }
    // Budgets carry forward from the lineage; they are never reset.
    // Owner and service handoffs are not charged (spec 0007 Q1), as in B4,
    // up to MAX_EXEMPT_HANDOFFS per lineage (review I1).
    let format: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&mut *tx)
            .await?;
    let exempt = if format
        .parse()
        .is_ok_and(|v: u32| v >= aq_machine_control::FORMAT)
    {
        format!(
            "-LEAST({},count(*) FILTER (WHERE EXISTS(SELECT 1 FROM attempt_handoffs x WHERE x.attempt_id=a.attempt_id AND x.reason IN ('owner_stop','service_stop'))))",
            aq_machine_control::MAX_EXEMPT_HANDOFFS
        )
    } else {
        String::new()
    };
    let started: i64 = sqlx::query_scalar(&format!("SELECT count(*){exempt} FROM profiled_launches l JOIN execution_attempts a USING(attempt_id) WHERE a.task_id=$1"))
        .bind(task_id)
        .fetch_one(&mut *tx)
        .await?;
    let spent = u32::try_from(started.max(1))
        .unwrap_or(u32::MAX)
        .saturating_add(u32::try_from(launch_failures).unwrap_or(0));
    let retries = replacement::MAX_ATTEMPTS.saturating_sub(spent);
    let lineage_start: Option<i64> = sqlx::query_scalar("SELECT min((l.graph_task_observation#>>'{native_offer,offered,clock,server_time_ms}')::BIGINT) FROM profiled_launches l JOIN execution_attempts a USING(attempt_id) WHERE a.task_id=$1")
        .bind(task_id)
        .fetch_one(&mut *tx)
        .await?;
    let start = u64::try_from(lineage_start.unwrap_or(wait_created).min(wait_created)).unwrap_or(0);
    let elapsed = replacement::MAX_LINEAGE_ELAPSED_MS.saturating_sub(now.saturating_sub(start));
    if retries == 0 || elapsed == 0 {
        finish(&mut tx, intent, "escalated", "budget_exhausted", at).await?;
        tx.commit().await?;
        return Ok(Step::Escalated);
    }
    // Placement: any live, unrevoked registered node, least loaded first.
    // Central (F1a) predecessors stay central; workers never get them.
    let admission_id = Uuid::new_v4();
    let attempt_id = Uuid::new_v4();
    let session_id = Uuid::new_v4();
    let attempt_generation = predecessor.attempt_generation + 1;
    let (session, node_id) = match &predecessor.session {
        wire::SessionIdentity::Central {
            authority_id,
            generation,
            ..
        } => (
            wire::SessionIdentity::Central {
                authority_id: *authority_id,
                session_id,
                generation: generation + 1,
            },
            None,
        ),
        wire::SessionIdentity::Remote { .. } => {
            let cutoff = (state.clock)()
                - chrono::Duration::seconds(
                    i64::try_from(state.config.workers.offline_after_secs).unwrap_or(i64::MAX / 2),
                );
            let Some(row) = sqlx::query("SELECT n.id,COALESCE((SELECT b.boot_id FROM delegated_node_boots b WHERE b.node_id=n.id),(e.state#>>'{identity,boot_id}')::uuid) AS boot_id FROM nodes n JOIN execution_nodes e ON e.node_id=n.id WHERE n.revoked_at IS NULL AND n.last_seen_at>=$1 AND e.state#>>'{identity,boot_id}' IS NOT NULL ORDER BY (SELECT count(*) FROM agent_wait_wake_admissions x WHERE x.node_id=n.id AND x.state='offered'),n.id LIMIT 1")
                .bind(cutoff)
                .fetch_optional(&mut *tx)
                .await?
            else {
                hold(&mut tx, intent, "no_eligible_node", now).await?;
                tx.commit().await?;
                return Ok(Step::Held);
            };
            let node: Uuid = row.get("id");
            (
                wire::SessionIdentity::Remote {
                    node_id: node,
                    boot_id: row.get("boot_id"),
                    attempt_id,
                    assignment_generation: attempt_generation,
                    session_id,
                },
                Some(node),
            )
        }
    };
    let successor = wire::Binding {
        project_id: predecessor.project_id,
        task_id: predecessor.task_id,
        lineage_id: predecessor.lineage_id,
        attempt_id,
        attempt_generation,
        session,
    };
    let generation_u = u64::try_from(generation).map_err(|_| WaitError("internal"))?;
    let ledger = wire::Digest::of(
        format!("agent-wait-wake-ledger:{task_id}:{started}:{launch_failures}:{now}").as_bytes(),
    );
    let attachment = wire::ContinuationAttachment {
        admission_id,
        continuation: stored.registered.continuation.clone(),
        predecessor: predecessor.clone(),
        successor,
        fresh_offer_digest: wire::Digest::of(
            format!("agent-wait-wake-offer:{admission_id}:{intent}:{generation}").as_bytes(),
        ),
        outcome,
        retained,
        pending_operation_ids: Vec::new(),
        held_task_message_ids: Vec::new(),
        remaining: lc::RemainingBudgets {
            lineage_id: predecessor.lineage_id,
            policy: artifact(
                "aq-server-replacement-policy",
                wire::Digest::of(
                    format!(
                        "replacement:{}:{}",
                        replacement::MAX_ATTEMPTS,
                        replacement::MAX_LINEAGE_ELAPSED_MS
                    )
                    .as_bytes(),
                ),
            ),
            ledger_checkpoint: artifact(format!("agent-wait-wake-ledger-{admission_id}"), ledger),
            remaining_failure_retries: retries,
            remaining_elapsed_ms: elapsed,
            cost: lc::CostBudget::NotConfigured,
        },
        native_resume: None,
    };
    if attachment.validate().is_err() {
        return held_or_escalated(tx, intent, holds, "attachment_invalid", now).await;
    }
    // Profile re-resolution: the server resolves no per-wake profile change
    // yet, so the recorded differences are placement and session identity.
    let boot = |s: &wire::SessionIdentity| match s {
        wire::SessionIdentity::Remote { boot_id, .. } => json!(boot_id),
        wire::SessionIdentity::Central { .. } => Value::Null,
    };
    let differences = json!({
        "node": {"from": predecessor_node, "to": node_id, "changed": predecessor_node != node_id},
        "boot": {"from": boot(&predecessor.session), "to": boot(&attachment.successor.session)},
        "session_kind": if node_id.is_some() { "remote" } else { "central" },
        "profile": {"changed": false},
        "dispatch_generation": generation_u,
    });
    sqlx::query("INSERT INTO agent_wait_wake_admissions(admission_id,intent_id,dispatch_generation,node_id,central,successor_attempt_id,successor_session_id,attachment,attachment_digest,differences,state,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,'offered',$11)")
        .bind(admission_id)
        .bind(intent)
        .bind(generation)
        .bind(node_id)
        .bind(node_id.is_none())
        .bind(attempt_id)
        .bind(session_id)
        .bind(serde_json::to_value(&attachment)?)
        .bind(wire::canonical_digest(&attachment)?.as_str())
        .bind(&differences)
        .bind(at)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE agent_wait_wakes SET state='admitted',reason=NULL,updated_at_ms=$2 WHERE intent_id=$1")
        .bind(intent)
        .bind(at)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Step::Admitted)
}

async fn held_or_escalated(
    mut tx: Transaction<'_, Postgres>,
    intent: Uuid,
    holds: i32,
    reason: &str,
    now: u64,
) -> Result<Step> {
    let step = if holds + 1 >= MAX_RETENTION_HOLDS {
        finish(&mut tx, intent, "escalated", reason, i64_of(now)?).await?;
        Step::Escalated
    } else {
        hold(&mut tx, intent, reason, now).await?;
        Step::Held
    };
    tx.commit().await?;
    Ok(step)
}

/// The latest dispatch generation's winner for a wait, if any.
pub async fn winner(state: &AppState, wait_id: Uuid) -> Result<Option<Admission>> {
    let Some(row) = sqlx::query("SELECT a.admission_id,a.node_id,a.dispatch_generation,a.state,a.attachment,a.differences FROM agent_wait_wake_admissions a JOIN agent_wait_wakes w USING(intent_id) WHERE w.wait_id=$1 ORDER BY a.dispatch_generation DESC LIMIT 1")
        .bind(wait_id)
        .fetch_optional(&state.store.pool)
        .await?
    else {
        return Ok(None);
    };
    Ok(Some(Admission {
        admission_id: row.get("admission_id"),
        node_id: row.get("node_id"),
        dispatch_generation: u64::try_from(row.get::<i64, _>("dispatch_generation"))
            .map_err(|_| WaitError("internal"))?,
        state: row.get("state"),
        attachment: serde_json::from_value(row.get("attachment"))?,
        differences: row.get("differences"),
    }))
}

/// Bounded launch/ack recovery: a failed winner opens the next dispatch
/// generation until `MAX_LAUNCH_FAILURES`, then the wake escalates. Returns
/// the wake's resulting state.
pub async fn record_launch_failure(
    state: &AppState,
    admission_id: Uuid,
    reason: &str,
) -> Result<String> {
    let reason = if reason.is_empty() || reason.len() > 256 {
        "launch_failed"
    } else {
        reason
    };
    let at = i64_of(now(state))?;
    let mut tx = state.store.pool.begin().await?;
    let row = sqlx::query("SELECT a.intent_id,a.dispatch_generation,a.state AS admission_state,w.dispatch_generation AS current,w.state,w.launch_failures FROM agent_wait_wake_admissions a JOIN agent_wait_wakes w USING(intent_id) WHERE a.admission_id=$1 FOR UPDATE OF w")
        .bind(admission_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(WaitError("not_found"))?;
    let intent: Uuid = row.get("intent_id");
    let wake_state: String = row.get("state");
    if row.get::<String, _>("admission_state") != "offered"
        || row.get::<i64, _>("dispatch_generation") != row.get::<i64, _>("current")
        || wake_state != "admitted"
    {
        tx.rollback().await?;
        return Ok(wake_state);
    }
    sqlx::query(
        "UPDATE agent_wait_wake_admissions SET state='failed',failure=$2 WHERE admission_id=$1",
    )
    .bind(admission_id)
    .bind(reason)
    .execute(&mut *tx)
    .await?;
    let failures = row.get::<i32, _>("launch_failures") + 1;
    let next = if failures >= MAX_LAUNCH_FAILURES {
        "escalated"
    } else {
        "resume_pending"
    };
    sqlx::query("UPDATE agent_wait_wakes SET launch_failures=$2,state=$3,reason=$4,dispatch_generation=dispatch_generation+CASE WHEN $3='resume_pending' THEN 1 ELSE 0 END,next_due_ms=0,updated_at_ms=$5 WHERE intent_id=$1")
        .bind(intent)
        .bind(failures)
        .bind(next)
        .bind(reason)
        .bind(at)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(next.to_owned())
}

/// Owner early resume: labelled `manual_override`, due now. Age (the wait's
/// creation time) and the lineage budgets are unchanged.
pub async fn manual_resume(state: &AppState, wait_id: Uuid) -> Result<bool> {
    let updated = sqlx::query("UPDATE agent_wait_wakes SET manual_override=TRUE,next_due_ms=0,updated_at_ms=$2 WHERE wait_id=$1 AND state='resume_pending'")
        .bind(wait_id)
        .bind(i64_of(now(state))?)
        .execute(&state.store.pool)
        .await?;
    Ok(updated.rows_affected() == 1)
}

/// Background loop; the periodic pass is the source of wake liveness.
pub async fn run(state: AppState, mut stop: tokio::sync::watch::Receiver<bool>) {
    loop {
        if *stop.borrow() {
            return;
        }
        if let Err(error) = schedule(&state, 64).await {
            tracing::warn!(code = error.0, "agent wait wake scheduling failed");
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {}
            _ = stop.changed() => {}
        }
    }
}
