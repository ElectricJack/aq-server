//! Replacement of a proven-failed native attempt by a new attempt, session and
//! generation. A replacement is admitted only after stop proof, retained source
//! and quiescent effects; uncertain prior execution always blocks. Releasing the
//! predecessor's task claim is a durable, immutable owner record, never inferred.
use crate::{
    AppState,
    error::AppError,
    execution::profiled::{Effects, Retention, Status, Stop},
};
use aq_admission::ArtifactRef;
use aq_selection::{ReplacementEvidence, TaskLineage};
use serde_json::Value;
use sqlx::Row;
use uuid::Uuid;

/// Server-owned lineage budgets until per-project budget configuration exists.
/// The selected intelligence policy's own attempt/time limits still fence.
pub const MAX_ATTEMPTS: u32 = 3;
pub const MAX_LINEAGE_ELAPSED_MS: u64 = 24 * 3_600_000;
pub const MAX_SANDBOX_MS: u64 = 240 * 60_000;
/// Replacement evidence is a fresh observation, matching the budget snapshot.
pub const EVIDENCE_TTL_MS: u64 = 2_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_attempts: u32,
    pub max_lineage_elapsed_ms: u64,
    pub max_sandbox_ms: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_attempts: MAX_ATTEMPTS,
            max_lineage_elapsed_ms: MAX_LINEAGE_ELAPSED_MS,
            max_sandbox_ms: MAX_SANDBOX_MS,
        }
    }
}

/// Owner facts for one started (not never-granted) attempt of a task.
#[derive(Debug, Clone)]
pub struct Prior {
    pub attempt_id: Uuid,
    /// Original publication instant; `None` means lineage history is unknown.
    pub offered_at_ms: Option<u64>,
    pub holds_task: bool,
    pub released: bool,
    pub status: Status,
    pub granted: bool,
    pub execution_deadline_ms: Option<u64>,
    pub stop: Stop,
    pub retention: Retention,
    pub effects: Effects,
    pub accepted: bool,
    pub rejected: bool,
    pub needs_reconciliation: bool,
    pub cancelled_settled: bool,
    pub completion: Option<String>,
    /// An owner_stop or service_stop handoff (spec 0007 Q1): not charged to
    /// any lineage budget.
    pub exempt: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Replacement {
    pub predecessor: Uuid,
    pub attempts_started: u32,
    /// Started attempts exempt from the budgets (spec 0007 Q1).
    pub exempt_attempts: u32,
    pub elapsed_ms: u64,
    pub sandbox_ms: u64,
    pub stop_receipt: ArtifactRef,
    pub retained_source_receipt: ArtifactRef,
    pub effect_reconciliation_receipt: ArtifactRef,
    /// The predecessor still holds the task claim and must be released first.
    pub needs_release: bool,
}
impl Replacement {
    pub fn evidence(
        &self,
        lineage: TaskLineage,
        now: u64,
    ) -> Result<ReplacementEvidence, AppError> {
        Ok(ReplacementEvidence {
            lineage,
            predecessor_attempt_id: self.predecessor,
            stop_receipt: self.stop_receipt.clone(),
            retained_source_receipt: self.retained_source_receipt.clone(),
            effect_reconciliation_receipt: self.effect_reconciliation_receipt.clone(),
            observed_at_ms: now,
            expires_at_ms: now.checked_add(EVIDENCE_TTL_MS).ok_or(AppError::Internal)?,
        })
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Assessment {
    /// No started attempt exists; an ordinary first attempt.
    Fresh,
    Replace(Box<Replacement>),
    Blocked(&'static str),
}
impl Assessment {
    pub fn replacement(&self) -> Option<&Replacement> {
        match self {
            Self::Replace(r) => Some(r.as_ref()),
            _ => None,
        }
    }
}

fn failed(prior: &Prior, now: u64) -> bool {
    prior.cancelled_settled
        || prior.rejected
        || matches!(prior.status, Status::Finished | Status::Declined)
        || prior.execution_deadline_ms.is_some_and(|d| now >= d)
}

/// Pure replacement decision over the complete started-attempt history.
pub fn assess(priors: &[Prior], limits: &Limits, now: u64) -> Assessment {
    use Assessment::Blocked;
    let mut ordered = priors.iter().collect::<Vec<_>>();
    if ordered.is_empty() {
        return Assessment::Fresh;
    }
    if ordered.iter().any(|p| p.offered_at_ms.is_none()) {
        return Blocked("replacement_lineage_history_unknown");
    }
    ordered.sort_by_key(|p| (p.offered_at_ms, p.attempt_id));
    if ordered.iter().any(|p| p.accepted) {
        return Blocked("task_already_settled_accepted");
    }
    if ordered.iter().filter(|p| p.holds_task).count() > 1 {
        return Blocked("replacement_prior_execution_uncertain");
    }
    for prior in &ordered {
        if !prior.stop.known_stopped() {
            return Blocked("replacement_prior_execution_uncertain");
        }
        if !matches!(prior.effects, Effects::Quiescent { .. })
            || prior.needs_reconciliation
            || prior.completion.is_some()
        {
            return Blocked("replacement_effects_unreconciled");
        }
    }
    let predecessor = ordered[ordered.len() - 1];
    // Every earlier attempt must have released its claim through this owner.
    if ordered[..ordered.len() - 1]
        .iter()
        .any(|p| p.holds_task || !p.released)
    {
        return Blocked("replacement_prior_execution_uncertain");
    }
    if !predecessor.holds_task && !predecessor.released {
        return Blocked("replacement_prior_execution_uncertain");
    }
    if !failed(predecessor, now) {
        return Blocked("replacement_predecessor_not_failed");
    }
    let (Stop::Stopped { receipt: stop } | Stop::NotStarted { receipt: stop }) = &predecessor.stop
    else {
        return Blocked("replacement_prior_execution_uncertain");
    };
    let Retention::Retained { receipt: retained } = &predecessor.retention else {
        return Blocked("replacement_source_retention_unknown");
    };
    let Effects::Quiescent { receipt: effects } = &predecessor.effects else {
        return Blocked("replacement_effects_unreconciled");
    };
    let attempts_started = u32::try_from(ordered.len()).unwrap_or(u32::MAX);
    let exempt = aq_machine_control::cap_exempt(ordered.iter().map(|p| p.exempt));
    let exempt_attempts = u32::try_from(exempt.iter().filter(|e| **e).count()).unwrap_or(0);
    if attempts_started - exempt_attempts >= limits.max_attempts {
        return Blocked("lineage_retry_budget_exhausted");
    }
    // Lineage time is the sum of the charged attempts' windows, each from its
    // publication to the next one (or now); without exemptions, now - first.
    let elapsed_ms = (0..ordered.len())
        .filter(|&i| !exempt[i])
        .map(|i| {
            let end = ordered.get(i + 1).and_then(|n| n.offered_at_ms);
            end.unwrap_or(now)
                .saturating_sub(ordered[i].offered_at_ms.unwrap_or(now))
        })
        .fold(0u64, u64::saturating_add);
    if elapsed_ms >= limits.max_lineage_elapsed_ms {
        return Blocked("lineage_time_budget_exhausted");
    }
    // Conservative sandbox charge: each granted attempt is billed its full
    // admitted window from publication to execution deadline.
    let sandbox_ms = (ordered.iter().zip(&exempt))
        .filter(|(p, e)| p.granted && !**e)
        .map(|(p, _)| {
            p.execution_deadline_ms
                .unwrap_or(u64::MAX)
                .saturating_sub(p.offered_at_ms.unwrap_or(0))
        })
        .fold(0u64, u64::saturating_add);
    if sandbox_ms >= limits.max_sandbox_ms {
        return Blocked("task_sandbox_minutes_budget_exhausted");
    }
    Assessment::Replace(Box::new(Replacement {
        predecessor: predecessor.attempt_id,
        attempts_started,
        exempt_attempts,
        elapsed_ms,
        sandbox_ms,
        stop_receipt: stop.clone(),
        retained_source_receipt: retained.clone(),
        effect_reconciliation_receipt: effects.clone(),
        needs_release: predecessor.holds_task,
    }))
}

/// `$exempt` is the spec 0007 Q1 handoff exemption, `FALSE` before format 101.
macro_rules! priors {
    ($exempt:literal) => {
        concat!("SELECT a.attempt_id,a.holds_task,h.state AS hot,", $exempt, " AS exempt,\
 (l.graph_task_observation#>>'{native_offer,offered,clock,server_time_ms}')::BIGINT AS offered_at_ms,\
 EXISTS(SELECT 1 FROM profiled_replacement_releases r WHERE r.attempt_id=a.attempt_id) AS released,\
 EXISTS(SELECT 1 FROM profiled_settlement_evaluations e WHERE e.attempt_id=a.attempt_id AND e.disposition='accepted') AS accepted,\
 EXISTS(SELECT 1 FROM profiled_settlement_evaluations e WHERE e.attempt_id=a.attempt_id AND e.disposition='rejected') AS rejected,\
 EXISTS(SELECT 1 FROM profiled_settlement_evaluations e WHERE e.attempt_id=a.attempt_id AND e.disposition='needs_reconciliation') AS needs_reconciliation,\
 EXISTS(SELECT 1 FROM profiled_cancelled_settlements s WHERE s.attempt_id=a.attempt_id) AS cancelled,\
 (SELECT c.state FROM profiled_task_completions c WHERE c.attempt_id=a.attempt_id) AS completion \
 FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) LEFT JOIN profiled_launches l USING(attempt_id) \
 WHERE a.task_id=$1 AND a.kind='profiled_v1' AND NOT (NOT a.holds_task AND profiled_never_granted_valid(a.attempt_id)) \
 ORDER BY a.attempt_id")
    };
}
const PRIORS: &str = priors!(
    "EXISTS(SELECT 1 FROM attempt_handoffs x WHERE x.attempt_id=a.attempt_id AND x.reason IN ('owner_stop','service_stop'))"
);
const PRIORS_PRE_101: &str = priors!("FALSE");

fn decode(row: sqlx::postgres::PgRow) -> Result<Prior, AppError> {
    let hot: crate::execution::authority::hot::HotState =
        serde_json::from_value(row.get::<Value, _>("hot")).map_err(|_| AppError::Internal)?;
    Ok(Prior {
        attempt_id: row.get("attempt_id"),
        offered_at_ms: row
            .get::<Option<i64>, _>("offered_at_ms")
            .and_then(|v| u64::try_from(v).ok()),
        holds_task: row.get("holds_task"),
        released: row.get("released"),
        status: hot.status,
        granted: hot.grant.is_some() || hot.execution_deadline_ms.is_some(),
        execution_deadline_ms: hot.execution_deadline_ms,
        stop: hot.stop,
        retention: hot.retention,
        effects: hot.effects,
        accepted: row.get("accepted"),
        rejected: row.get("rejected"),
        needs_reconciliation: row.get("needs_reconciliation"),
        cancelled_settled: row.get("cancelled"),
        completion: row.get("completion"),
        exempt: row.get("exempt"),
    })
}

/// Loads started-attempt owner facts. A history that does not match the
/// central started count (for example legacy attempts) is returned as `None`.
pub async fn load(
    conn: &mut sqlx::PgConnection,
    task: Uuid,
    started: u32,
) -> Result<Option<Vec<Prior>>, AppError> {
    let handoffs = aq_machine_control::has_format(conn).await?;
    let rows = sqlx::query(if handoffs { PRIORS } else { PRIORS_PRE_101 })
        .bind(task)
        .fetch_all(conn)
        .await
        .map_err(|_| AppError::Internal)?;
    let priors = rows
        .into_iter()
        .map(decode)
        .collect::<Result<Vec<_>, _>>()?;
    Ok((priors.len() == started as usize).then_some(priors))
}

/// Scheduling assessment for one Graph-ready task.
/// The decision attempt counts alone settle: an exhausted never-granted
/// retry bound blocks, and no started attempt is an ordinary first attempt.
pub fn precheck(counts: &super::no_grant::AttemptCounts) -> Option<Assessment> {
    if counts.check_retry_bound().is_err() {
        return Some(Assessment::Blocked("never_granted_retry_bound_exhausted"));
    }
    (counts.started == 0).then_some(Assessment::Fresh)
}
/// First store format carrying `profiled_auto_settlements` (migration 59, D3).
pub const AUTO_SETTLEMENT_FORMAT: u32 = 59;
/// First store format carrying `stall_quarantines` (migration 81, H3).
pub const STALL_QUARANTINE_FORMAT: u32 = 81;

/// Whether the H3 stall ladder quarantined this lineage. A store held at an
/// earlier migration prefix has no quarantine table and so records no
/// quarantine, instead of failing every poll and replacement assessment.
async fn lineage_quarantined(conn: &mut sqlx::PgConnection, task: Uuid) -> Result<bool, AppError> {
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&mut *conn)
            .await
            .map_err(|_| AppError::Internal)?;
    if version.parse::<u32>().map_err(|_| AppError::Internal)? < STALL_QUARANTINE_FORMAT {
        return Ok(false);
    }
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM stall_quarantines WHERE lineage_id=$1)")
        .bind(task)
        .fetch_one(&mut *conn)
        .await
        .map_err(|_| AppError::Internal)
}
pub async fn assess_task(state: &AppState, task: Uuid, now: u64) -> Result<Assessment, AppError> {
    let mut conn = state
        .store
        .pool
        .acquire()
        .await
        .map_err(|_| AppError::Internal)?;
    let disposed = super::disposition::task_disposed(&mut conn, task).await?;
    if disposed {
        return Ok(Assessment::Blocked("task_terminally_disposed"));
    }
    // H3 rung 5: a quarantined lineage is never replaced until an owner acts.
    let quarantined = lineage_quarantined(&mut conn, task).await?;
    drop(conn);
    if quarantined {
        return Ok(Assessment::Blocked("stall_quarantined"));
    }
    // D1: a successor reuses the predecessor's immutable export, so a result
    // refused because its publication was definitively not applied (moved
    // target, conflicted candidate) would recur. Such a lineage needs a new
    // source admission (a new task attempt from the current source head).
    // A store held at an earlier migration prefix has no automatic settlement
    // table and so records no unpublished delivery (as for quarantines).
    let format: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&state.store.pool)
            .await
            .map_err(|_| AppError::Internal)?;
    let unpublished: bool = if format.parse::<u32>().map_err(|_| AppError::Internal)?
        >= AUTO_SETTLEMENT_FORMAT
    {
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM profiled_auto_settlements s JOIN execution_attempts a USING(attempt_id) WHERE a.task_id=$1 AND s.disposition='rework_required' AND s.reason='delivery_unpublished')")
            .bind(task)
            .fetch_one(&state.store.pool)
            .await
            .map_err(|_| AppError::Internal)?
    } else {
        false
    };
    if unpublished {
        return Ok(Assessment::Blocked(
            "replacement_after_unpublished_delivery_needs_new_source",
        ));
    }
    let counts = super::no_grant::attempt_counts(&state.store.pool, task).await?;
    if let Some(decided) = precheck(&counts) {
        return Ok(decided);
    }
    let mut conn = state
        .store
        .pool
        .acquire()
        .await
        .map_err(|_| AppError::Internal)?;
    Ok(match load(&mut conn, task, counts.started).await? {
        Some(priors) => assess(&priors, &Limits::default(), now),
        None => Assessment::Blocked("replacement_lineage_history_unknown"),
    })
}

/// Scheduling assessment against the current project binding revision. A
/// successor reuses the predecessor's export, whose request pins the project
/// revision it was offered under; source observation refuses any other
/// revision. Such a lineage is reported blocked here, so one unreplaceable
/// task cannot fail the whole poll and starve the project's other tasks.
pub async fn assess_task_for_revision(
    state: &AppState,
    task: Uuid,
    now: u64,
    project_revision: u64,
) -> Result<Assessment, AppError> {
    let assessment = assess_task(state, task, now).await?;
    let Some(replacement) = assessment.replacement() else {
        return Ok(assessment);
    };
    let pinned = predecessor_project_revision(&state.store.pool, replacement.predecessor).await?;
    if pinned != Some(project_revision) {
        return Ok(Assessment::Blocked("replacement_project_revision_changed"));
    }
    // Spec 0007 Q1: a successor the predecessor's pinned selection policy
    // would refuse is skipped here, so it never fails the poll.
    let limits =
        super::continuation_policy::pinned_limits(&state.store.pool, replacement.predecessor)
            .await?;
    let charged = replacement.attempts_started - replacement.exempt_attempts;
    Ok(
        match limits.and_then(|l| {
            super::continuation_policy::pinned_block(l, charged, replacement.elapsed_ms)
        }) {
            Some(reason) => Assessment::Blocked(reason),
            None => assessment,
        },
    )
}

/// The project revision pinned by an attempt's linked source export request.
pub async fn predecessor_project_revision(
    pool: &sqlx::PgPool,
    attempt: Uuid,
) -> Result<Option<u64>, AppError> {
    let pinned: Option<Option<i64>> = sqlx::query_scalar(
        "SELECT (convert_from(e.request_bytes,'UTF8')::jsonb->>'project_revision')::BIGINT FROM profiled_launch_source_exports x JOIN profiled_source_exports e USING(operation_id) WHERE x.attempt_id=$1",
    )
    .bind(attempt)
    .fetch_optional(pool)
    .await
    .map_err(|_| AppError::Internal)?;
    Ok(pinned.flatten().and_then(|r| u64::try_from(r).ok()))
}

/// Releases the failed predecessor's task claim under the common node →
/// attempt/hot locks, after re-assessing the locked history. Idempotent.
pub async fn release(
    state: &AppState,
    task: Uuid,
    replacement: &Replacement,
) -> Result<(), AppError> {
    if !replacement.needs_release {
        return Ok(());
    }
    let db = |_: sqlx::Error| AppError::Internal;
    let node: Uuid = sqlx::query_scalar("SELECT node_id FROM execution_attempts WHERE attempt_id=$1 AND task_id=$2 AND kind='profiled_v1'")
        .bind(replacement.predecessor)
        .bind(task)
        .fetch_one(&state.store.pool)
        .await
        .map_err(db)?;
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    for setting in [
        "SET LOCAL lock_timeout='2s'",
        "SET LOCAL statement_timeout='5s'",
    ] {
        sqlx::query(setting).execute(&mut *tx).await.map_err(db)?;
    }
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(node)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("SELECT 1 FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) WHERE a.attempt_id=$1 FOR UPDATE OF a,h")
        .bind(replacement.predecessor)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
    let counts = super::no_grant::attempt_counts_locked(&mut tx, task).await?;
    let priors = load(&mut tx, task, counts.started)
        .await?
        .ok_or_else(|| AppError::Conflict("replacement history changed".into()))?;
    let now = u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|n| *n > 0)
        .ok_or(AppError::Internal)?;
    let Assessment::Replace(locked) = assess(&priors, &Limits::default(), now) else {
        return Err(AppError::Conflict(
            "replacement no longer admissible".into(),
        ));
    };
    if locked.predecessor != replacement.predecessor {
        return Err(AppError::Conflict("replacement predecessor changed".into()));
    }
    if locked.needs_release {
        sqlx::query("INSERT INTO profiled_replacement_releases(attempt_id,task_id,reason,stop_receipt_digest,retained_source_receipt_digest,effect_reconciliation_receipt_digest,attempts_started,recorded_at_ms) VALUES($1,$2,'failed_attempt_replacement',$3,$4,$5,$6,$7)")
            .bind(locked.predecessor)
            .bind(task)
            .bind(locked.stop_receipt.digest.as_str())
            .bind(locked.retained_source_receipt.digest.as_str())
            .bind(locked.effect_reconciliation_receipt.digest.as_str())
            .bind(i32::try_from(locked.attempts_started).map_err(|_| AppError::Internal)?)
            .bind(i64::try_from(now).map_err(|_| AppError::Internal)?)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
        sqlx::query(
            "UPDATE execution_attempts SET holds_task=FALSE WHERE attempt_id=$1 AND holds_task",
        )
        .bind(locked.predecessor)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    }
    tx.commit().await.map_err(db)?;
    Ok(())
}

/// Read the immutable release receipt before selecting the predecessor's owned
/// export. A missing or mismatched release must never fall back to fresh source.
pub(crate) async fn retained_source_locked(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    task: Uuid,
    predecessor: Uuid,
    receipt: &ArtifactRef,
) -> Result<super::source_exports::LinkedExport, AppError> {
    let digest: Option<String> = sqlx::query_scalar(
        "SELECT retained_source_receipt_digest FROM profiled_replacement_releases WHERE attempt_id=$1 AND task_id=$2 FOR SHARE",
    )
    .bind(predecessor)
    .bind(task)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|_| AppError::Internal)?;
    if digest.as_deref() != Some(receipt.digest.as_str()) {
        return Err(AppError::Conflict(
            "replacement retained source receipt changed or missing".into(),
        ));
    }
    super::source_exports::load_linked_locked(tx, predecessor).await
}

pub async fn retained_source(
    state: &AppState,
    task: Uuid,
    replacement: &Replacement,
) -> Result<super::source_exports::LinkedExport, AppError> {
    let mut tx = state
        .store
        .pool
        .begin()
        .await
        .map_err(|_| AppError::Internal)?;
    let linked = retained_source_locked(
        &mut tx,
        task,
        replacement.predecessor,
        &replacement.retained_source_receipt,
    )
    .await?;
    tx.commit().await.map_err(|_| AppError::Internal)?;
    Ok(linked)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use aq_admission::{Digest, Text};

    pub(crate) fn receipt(id: &str) -> ArtifactRef {
        ArtifactRef {
            id: Text::new(id).unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(id.as_bytes()),
        }
    }
    pub(crate) fn failed_prior(offered_at_ms: u64) -> Prior {
        Prior {
            attempt_id: Uuid::new_v4(),
            offered_at_ms: Some(offered_at_ms),
            holds_task: true,
            released: false,
            status: Status::Finished,
            granted: true,
            execution_deadline_ms: Some(offered_at_ms + 60_000),
            stop: Stop::Stopped {
                receipt: receipt("stop"),
            },
            retention: Retention::Retained {
                receipt: receipt("retained"),
            },
            effects: Effects::Quiescent {
                receipt: receipt("effects"),
            },
            accepted: false,
            rejected: true,
            needs_reconciliation: false,
            cancelled_settled: false,
            completion: None,
            exempt: false,
        }
    }
    fn released(mut prior: Prior) -> Prior {
        prior.holds_task = false;
        prior.released = true;
        prior
    }

    #[test]
    fn failed_attempt_is_replaced_within_retry_budget() {
        let now = 1_000_000;
        let limits = Limits::default();
        assert_eq!(assess(&[], &limits, now), Assessment::Fresh);
        let first = failed_prior(900_000);
        let Assessment::Replace(replacement) = assess(std::slice::from_ref(&first), &limits, now)
        else {
            panic!("a stopped, retained, quiescent failed attempt is replaceable");
        };
        assert_eq!(replacement.predecessor, first.attempt_id);
        assert_eq!(replacement.attempts_started, 1);
        assert_eq!(replacement.elapsed_ms, 100_000);
        assert!(replacement.needs_release);
        let lineage = TaskLineage {
            project_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            task_id: Uuid::new_v4(),
        };
        let evidence = replacement.evidence(lineage.clone(), now).unwrap();
        assert_eq!(evidence.predecessor_attempt_id, first.attempt_id);
        assert_eq!(evidence.stop_receipt, receipt("stop"));
        assert_eq!(evidence.retained_source_receipt, receipt("retained"));
        assert_eq!(evidence.effect_reconciliation_receipt, receipt("effects"));
        assert_eq!(evidence.expires_at_ms, now + EVIDENCE_TTL_MS);

        // After the durable release, the second failure replaces from the newest
        // predecessor; history order is by publication, not attempt id.
        let second = failed_prior(950_000);
        let Assessment::Replace(again) =
            assess(&[second.clone(), released(first.clone())], &limits, now)
        else {
            panic!("second attempt remains within the default budget");
        };
        assert_eq!(again.predecessor, second.attempt_id);
        assert_eq!(again.attempts_started, 2);
        // An already released predecessor needs no second release.
        let Assessment::Replace(idem) = assess(&[released(first.clone())], &limits, now) else {
            panic!("released predecessor stays replaceable");
        };
        assert!(!idem.needs_release);

        // Cancellation settlement and an expired deadline also prove failure.
        let mut cancelled = failed_prior(900_000);
        cancelled.rejected = false;
        cancelled.status = Status::Cancelling;
        cancelled.cancelled_settled = true;
        assert!(matches!(
            assess(&[cancelled], &limits, now),
            Assessment::Replace(_)
        ));
        let mut expired = failed_prior(900_000);
        expired.rejected = false;
        expired.status = Status::Granted;
        assert!(matches!(
            assess(&[expired.clone()], &limits, now),
            Assessment::Replace(_)
        ));
        expired.execution_deadline_ms = Some(now + 1);
        assert_eq!(
            assess(&[expired], &limits, now),
            Assessment::Blocked("replacement_predecessor_not_failed")
        );
    }

    #[test]
    fn closed_unstarted_attempt_is_replaceable() {
        // B5b/AQ-08: a settled launch closure (not started, its own export
        // retained, no effects) is a failed predecessor like any other.
        let now = 1_000_000;
        let mut closed = failed_prior(900_000);
        closed.rejected = false;
        closed.stop = Stop::NotStarted {
            receipt: receipt("closure"),
        };
        let Assessment::Replace(r) = assess(std::slice::from_ref(&closed), &Limits::default(), now)
        else {
            panic!("settled closure must be replaceable");
        };
        assert_eq!(r.stop_receipt, receipt("closure"));
        assert!(r.needs_release);
        // An unsettled closure (stop still unknown) blocks.
        closed.stop = Stop::Unknown;
        closed.status = Status::Cancelling;
        assert_eq!(
            assess(&[closed], &Limits::default(), now),
            Assessment::Blocked("replacement_prior_execution_uncertain")
        );
    }

    #[test]
    fn retry_budget_exhaustion_blocks_task() {
        let now = 1_000_000;
        let limits = Limits {
            max_attempts: 2,
            ..Limits::default()
        };
        let history = [released(failed_prior(800_000)), failed_prior(900_000)];
        assert_eq!(
            assess(&history, &limits, now),
            Assessment::Blocked("lineage_retry_budget_exhausted")
        );
        assert_eq!(
            assess(
                &history,
                &Limits {
                    max_lineage_elapsed_ms: 200_000,
                    ..Limits::default()
                },
                now
            ),
            Assessment::Blocked("lineage_time_budget_exhausted")
        );
        assert_eq!(
            assess(
                &history,
                &Limits {
                    max_sandbox_ms: 120_000,
                    ..Limits::default()
                },
                now
            ),
            Assessment::Blocked("task_sandbox_minutes_budget_exhausted")
        );
        // Uncertain prior execution blocks before any budget question.
        let mut running = failed_prior(900_000);
        running.stop = Stop::Running;
        assert_eq!(
            assess(&[running], &Limits::default(), now),
            Assessment::Blocked("replacement_prior_execution_uncertain")
        );
        let mut pending = failed_prior(900_000);
        pending.effects = Effects::Pending { count: 1 };
        assert_eq!(
            assess(&[pending], &Limits::default(), now),
            Assessment::Blocked("replacement_effects_unreconciled")
        );
        let mut unretained = failed_prior(900_000);
        unretained.retention = Retention::Pending;
        assert_eq!(
            assess(&[unretained], &Limits::default(), now),
            Assessment::Blocked("replacement_source_retention_unknown")
        );
        let mut accepted = failed_prior(900_000);
        accepted.accepted = true;
        assert_eq!(
            assess(&[accepted], &Limits::default(), now),
            Assessment::Blocked("task_already_settled_accepted")
        );
        let mut unreleased = failed_prior(800_000);
        unreleased.holds_task = false;
        assert_eq!(
            assess(
                &[unreleased, failed_prior(900_000)],
                &Limits::default(),
                now
            ),
            Assessment::Blocked("replacement_prior_execution_uncertain")
        );
    }

    fn exempt(mut prior: Prior) -> Prior {
        prior.exempt = true;
        prior
    }
    const MIN: u64 = 60_000;
    const HOUR: u64 = 3_600_000;

    /// Review Focus 6: owner/service handoffs never exhaust the retry budget;
    /// a crash restart (not exempt) is still charged.
    #[test]
    fn owner_and_service_handoffs_are_exempt_restart_counts() {
        let now = 30 * MIN;
        let history = |exempt_first_two: bool| {
            let mark = |p: Prior| if exempt_first_two { exempt(p) } else { p };
            [
                released(mark(failed_prior(0))),
                released(mark(failed_prior(10 * MIN))),
                failed_prior(20 * MIN),
            ]
        };
        let Assessment::Replace(r) = assess(&history(true), &Limits::default(), now) else {
            panic!("two exempt handoffs leave one charged attempt");
        };
        assert_eq!((r.attempts_started, r.exempt_attempts), (3, 2));
        assert_eq!(
            assess(&history(false), &Limits::default(), now),
            Assessment::Blocked("lineage_retry_budget_exhausted")
        );
    }
    #[test]
    fn exempt_windows_are_not_lineage_time() {
        let now = 25 * HOUR;
        let history = |first: Prior| [released(first), failed_prior(23 * HOUR)];
        let Assessment::Replace(r) =
            assess(&history(exempt(failed_prior(0))), &Limits::default(), now)
        else {
            panic!("the exempt window is not charged");
        };
        assert_eq!(r.elapsed_ms, 2 * HOUR);
        assert_eq!(
            assess(&history(failed_prior(0)), &Limits::default(), now),
            Assessment::Blocked("lineage_time_budget_exhausted")
        );
    }
    #[test]
    fn exempt_attempts_cost_no_sandbox_minutes() {
        let now = 10 * HOUR;
        let long = |offered: u64| {
            let mut p = failed_prior(offered);
            p.execution_deadline_ms = Some(offered + 150 * MIN);
            p
        };
        let history = |e: bool| {
            let first = if e { exempt(long(0)) } else { long(0) };
            [released(first), long(3 * HOUR)]
        };
        let Assessment::Replace(r) = assess(&history(true), &Limits::default(), now) else {
            panic!("an exempt attempt costs no sandbox minutes");
        };
        assert_eq!(r.sandbox_ms, 150 * MIN);
        assert_eq!(
            assess(&history(false), &Limits::default(), now),
            Assessment::Blocked("task_sandbox_minutes_budget_exhausted")
        );
    }
    #[test]
    fn no_exempt_priors_match_previous_values() {
        let now = 1_000_000;
        let (first, second) = (failed_prior(800_000), failed_prior(900_000));
        let Assessment::Replace(r) = assess(&[released(first), second], &Limits::default(), now)
        else {
            panic!("within the default budget");
        };
        assert_eq!(r.exempt_attempts, 0);
        assert_eq!(r.elapsed_ms, now - 800_000);
        assert_eq!(r.sandbox_ms, 2 * 60_000);
    }

    #[tokio::test]
    async fn replacement_sql_matches_migrated_schema() {
        let db = crate::test_support::TestDb::create().await;
        let store = db.store().await;
        // The task has no started attempts: the history query plans and runs.
        let mut conn = store.pool.acquire().await.unwrap();
        let none = load(&mut conn, Uuid::new_v4(), 0).await.unwrap();
        assert_eq!(none.map(|priors| priors.len()), Some(0));
        // The guard body plans against the real schema and refuses a release
        // for an attempt that does not exist, before any foreign key check.
        let digest = "a".repeat(64);
        let err = sqlx::query("INSERT INTO profiled_replacement_releases VALUES($1,$2,'failed_attempt_replacement',$3,$3,$3,1,1)")
            .bind(Uuid::new_v4())
            .bind(Uuid::new_v4())
            .bind(&digest)
            .execute(&store.pool)
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("replacement release requires"), "{err}");
    }
}
