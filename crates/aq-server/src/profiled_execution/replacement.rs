//! Replacement of a proven-failed native attempt by a new attempt, session and
//! generation. A replacement is admitted only after stop proof, retained source
//! and quiescent effects; uncertain prior execution always blocks. Releasing the
//! predecessor's task claim is a durable, immutable owner record, never inferred.
use crate::{AppState, error::AppError};
use aq_admission::ArtifactRef;
pub use aq_profiled_state::replacement::*;
use aq_selection::{ReplacementEvidence, TaskLineage};
use serde_json::Value;
use sqlx::Row;
use uuid::Uuid;

/// The B4 successor's evidence of `r`, fresh for `EVIDENCE_TTL_MS`.
pub fn evidence(
    r: &Replacement,
    lineage: TaskLineage,
    now: u64,
) -> Result<ReplacementEvidence, AppError> {
    Ok(ReplacementEvidence {
        lineage,
        predecessor_attempt_id: r.predecessor,
        stop_receipt: r.stop_receipt.clone(),
        retained_source_receipt: r.retained_source_receipt.clone(),
        effect_reconciliation_receipt: r.effect_reconciliation_receipt.clone(),
        observed_at_ms: now,
        expires_at_ms: now.checked_add(EVIDENCE_TTL_MS).ok_or(AppError::Internal)?,
    })
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
/// From migration 102: a lost attempt released after a reboot is not charged (D2 ruling C).
const PRIORS_102: &str = priors!(
    "EXISTS(SELECT 1 FROM attempt_handoffs x WHERE x.attempt_id=a.attempt_id AND x.reason IN ('owner_stop','service_stop')) OR EXISTS(SELECT 1 FROM lost_attempt_releases l WHERE l.attempt_id=a.attempt_id AND NOT l.charged)"
);

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
    let sql = match aq_machine_control::lost::has_lost_format(conn).await? {
        true => PRIORS_102,
        false if aq_machine_control::has_format(conn).await? => PRIORS,
        false => PRIORS_PRE_101,
    };
    let rows = sqlx::query(sql)
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
mod tests {
    use super::*;
    #[test]
    fn evidence_carries_the_predecessor_receipts_for_its_ttl() {
        let receipt = |id: &str| ArtifactRef {
            id: aq_admission::Text::new(id).unwrap(),
            version: aq_admission::Text::new("1").unwrap(),
            digest: aq_admission::Digest::of(id.as_bytes()),
        };
        let r = Replacement {
            predecessor: Uuid::from_u128(1),
            attempts_started: 1,
            exempt_attempts: 0,
            elapsed_ms: 1,
            sandbox_ms: 0,
            stop_receipt: receipt("stop"),
            retained_source_receipt: receipt("retained"),
            effect_reconciliation_receipt: receipt("effects"),
            needs_release: true,
        };
        let lineage = TaskLineage {
            project_id: Uuid::new_v4(),
            workspace_id: Uuid::new_v4(),
            task_id: Uuid::new_v4(),
        };
        let e = evidence(&r, lineage, 1_000).unwrap();
        assert_eq!(e.predecessor_attempt_id, r.predecessor);
        assert_eq!(e.stop_receipt, receipt("stop"));
        assert_eq!(e.retained_source_receipt, receipt("retained"));
        assert_eq!(e.effect_reconciliation_receipt, receipt("effects"));
        assert_eq!(e.expires_at_ms, 1_000 + EVIDENCE_TTL_MS);
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
