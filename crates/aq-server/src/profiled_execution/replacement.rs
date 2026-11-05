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
}

#[derive(Debug, Clone, PartialEq)]
pub struct Replacement {
    pub predecessor: Uuid,
    pub attempts_started: u32,
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
    if attempts_started >= limits.max_attempts {
        return Blocked("lineage_retry_budget_exhausted");
    }
    let first = ordered[0].offered_at_ms.unwrap_or(now);
    let elapsed_ms = now.saturating_sub(first);
    if elapsed_ms >= limits.max_lineage_elapsed_ms {
        return Blocked("lineage_time_budget_exhausted");
    }
    // Conservative sandbox charge: each granted attempt is billed its full
    // admitted window from publication to execution deadline.
    let sandbox_ms = ordered
        .iter()
        .filter(|p| p.granted)
        .map(|p| {
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
        elapsed_ms,
        sandbox_ms,
        stop_receipt: stop.clone(),
        retained_source_receipt: retained.clone(),
        effect_reconciliation_receipt: effects.clone(),
        needs_release: predecessor.holds_task,
    }))
}

const PRIORS: &str = "SELECT a.attempt_id,a.holds_task,h.state AS hot,\
 (l.graph_task_observation#>>'{native_offer,offered,clock,server_time_ms}')::BIGINT AS offered_at_ms,\
 EXISTS(SELECT 1 FROM profiled_replacement_releases r WHERE r.attempt_id=a.attempt_id) AS released,\
 EXISTS(SELECT 1 FROM profiled_settlement_evaluations e WHERE e.attempt_id=a.attempt_id AND e.disposition='accepted') AS accepted,\
 EXISTS(SELECT 1 FROM profiled_settlement_evaluations e WHERE e.attempt_id=a.attempt_id AND e.disposition='rejected') AS rejected,\
 EXISTS(SELECT 1 FROM profiled_settlement_evaluations e WHERE e.attempt_id=a.attempt_id AND e.disposition='needs_reconciliation') AS needs_reconciliation,\
 EXISTS(SELECT 1 FROM profiled_cancelled_settlements s WHERE s.attempt_id=a.attempt_id) AS cancelled,\
 (SELECT c.state FROM profiled_task_completions c WHERE c.attempt_id=a.attempt_id) AS completion \
 FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) LEFT JOIN profiled_launches l USING(attempt_id) \
 WHERE a.task_id=$1 AND a.kind='profiled_v1' AND NOT (NOT a.holds_task AND profiled_never_granted_valid(a.attempt_id)) \
 ORDER BY a.attempt_id";

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
    })
}

/// Loads started-attempt owner facts. A history that does not match the
/// central started count (for example legacy attempts) is returned as `None`.
pub async fn load<'e, E>(
    executor: E,
    task: Uuid,
    started: u32,
) -> Result<Option<Vec<Prior>>, AppError>
where
    E: sqlx::PgExecutor<'e>,
{
    let rows = sqlx::query(PRIORS)
        .bind(task)
        .fetch_all(executor)
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
    let counts = super::no_grant::attempt_counts(&state.store.pool, task).await?;
    if let Some(decided) = precheck(&counts) {
        return Ok(decided);
    }
    Ok(match load(&state.store.pool, task, counts.started).await? {
        Some(priors) => assess(&priors, &Limits::default(), now),
        None => Assessment::Blocked("replacement_lineage_history_unknown"),
    })
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
    let priors = load(&mut *tx, task, counts.started)
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

    #[tokio::test]
    async fn replacement_sql_matches_migrated_schema() {
        let db = crate::test_support::TestDb::create().await;
        let store = db.store().await;
        // The task has no started attempts: the history query plans and runs.
        let none = load(&store.pool, Uuid::new_v4(), 0).await.unwrap();
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
