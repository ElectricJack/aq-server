//! The started-attempt history query of the B4 replacement decision (moved out of the
//! aq-server lib).
/// `$exempt` is the spec 0007 Q1 handoff exemption, `FALSE` before format 101. `$forgiven` is
/// "offered before the owner's latest override of a budget hold", `FALSE` before format 151.
/// `$fresh` is an extra condition of the history: the attempts an owner's fresh start left out.
macro_rules! priors {
    ($exempt:literal) => {
        priors!($exempt, "FALSE")
    };
    ($exempt:literal, $forgiven:literal) => {
        priors!($exempt, $forgiven, "")
    };
    ($exempt:literal, $forgiven:literal, $fresh:literal) => {
        concat!("SELECT a.attempt_id,a.holds_task,h.state AS hot,", $exempt, " AS exempt,", $forgiven, " AS forgiven,\
 (l.graph_task_observation#>>'{native_offer,offered,clock,server_time_ms}')::BIGINT AS offered_at_ms,\
 EXISTS(SELECT 1 FROM profiled_replacement_releases r WHERE r.attempt_id=a.attempt_id) AS released,\
 EXISTS(SELECT 1 FROM profiled_settlement_evaluations e WHERE e.attempt_id=a.attempt_id AND e.disposition='accepted') AS accepted,\
 EXISTS(SELECT 1 FROM profiled_settlement_evaluations e WHERE e.attempt_id=a.attempt_id AND e.disposition='rejected') AS rejected,\
 EXISTS(SELECT 1 FROM profiled_settlement_evaluations e WHERE e.attempt_id=a.attempt_id AND e.disposition='needs_reconciliation') AS needs_reconciliation,\
 EXISTS(SELECT 1 FROM profiled_cancelled_settlements s WHERE s.attempt_id=a.attempt_id) AS cancelled,\
 (SELECT c.state FROM profiled_task_completions c WHERE c.attempt_id=a.attempt_id) AS completion \
 FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) LEFT JOIN profiled_launches l USING(attempt_id) \
 WHERE a.task_id=$1 AND a.kind='profiled_v1' AND NOT (NOT a.holds_task AND profiled_never_granted_valid(a.attempt_id))", $fresh, " \
 ORDER BY a.attempt_id")
    };
}
pub const PRIORS: &str = priors!(
    "EXISTS(SELECT 1 FROM attempt_handoffs x WHERE x.attempt_id=a.attempt_id AND x.reason IN ('owner_stop','service_stop'))"
);
pub const PRIORS_PRE_101: &str = priors!("FALSE");
/// From migration 102: a lost attempt released after a reboot is not charged (D2 ruling C),
/// nor is a granted attempt that never started (`aq_machine_control::lapse`).
pub const PRIORS_102: &str = priors!(
    "EXISTS(SELECT 1 FROM attempt_handoffs x WHERE x.attempt_id=a.attempt_id AND x.reason IN ('owner_stop','service_stop')) OR EXISTS(SELECT 1 FROM lost_attempt_releases l WHERE l.attempt_id=a.attempt_id AND NOT l.charged) OR COALESCE(h.state#>>'{stop,receipt,id}'='aq.granted-never-started/'||a.attempt_id::TEXT,FALSE)"
);
/// From migration 151: the owner's override of a budget hold starts the budget again.
pub const PRIORS_151: &str = priors!(
    "EXISTS(SELECT 1 FROM attempt_handoffs x WHERE x.attempt_id=a.attempt_id AND x.reason IN ('owner_stop','service_stop')) OR EXISTS(SELECT 1 FROM lost_attempt_releases l WHERE l.attempt_id=a.attempt_id AND NOT l.charged) OR COALESCE(h.state#>>'{stop,receipt,id}'='aq.granted-never-started/'||a.attempt_id::TEXT,FALSE)",
    "COALESCE((l.graph_task_observation#>>'{native_offer,offered,clock,server_time_ms}')::BIGINT<=(SELECT max(o.recorded_at_ms) FROM task_limit_overrides o WHERE o.task_id=a.task_id),FALSE)"
);
/// From migration 157: an owner's fresh start (`aq_machine_control::fresh`) leaves the attempts
/// it released out of the history, so the task's next attempt is a first attempt.
pub const PRIORS_157: &str = priors!(
    "EXISTS(SELECT 1 FROM attempt_handoffs x WHERE x.attempt_id=a.attempt_id AND x.reason IN ('owner_stop','service_stop')) OR EXISTS(SELECT 1 FROM lost_attempt_releases l WHERE l.attempt_id=a.attempt_id AND NOT l.charged) OR COALESCE(h.state#>>'{stop,receipt,id}'='aq.granted-never-started/'||a.attempt_id::TEXT,FALSE)",
    "COALESCE((l.graph_task_observation#>>'{native_offer,offered,clock,server_time_ms}')::BIGINT<=(SELECT max(o.recorded_at_ms) FROM task_limit_overrides o WHERE o.task_id=a.task_id),FALSE)",
    " AND NOT EXISTS(SELECT 1 FROM task_fresh_starts f WHERE f.attempt_id=a.attempt_id)"
);

fn decode(
    row: sqlx::postgres::PgRow,
) -> Result<crate::replacement::Prior, aq_machine_control::Error> {
    use sqlx::Row;
    let hot: crate::hot::HotState = serde_json::from_value(row.get("hot"))
        .map_err(|_| aq_machine_control::Error::Invalid("hot state"))?;
    Ok(crate::replacement::Prior {
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
        forgiven: row.get("forgiven"),
    })
}
/// The started-attempt owner facts of `task`, by the query of the store's format.
pub async fn load(
    conn: &mut sqlx::PgConnection,
    task: uuid::Uuid,
) -> Result<Vec<crate::replacement::Prior>, aq_machine_control::Error> {
    let sql = match aq_machine_control::lost::has_lost_format(conn).await? {
        true if aq_machine_control::fresh::has_fresh_starts(conn).await? => PRIORS_157,
        true if aq_machine_control::holds::has_overrides(conn).await? => PRIORS_151,
        true => PRIORS_102,
        false if aq_machine_control::has_format(conn).await? => PRIORS,
        false => PRIORS_PRE_101,
    };
    let rows = sqlx::query(sql).bind(task).fetch_all(conn).await?;
    rows.into_iter().map(decode).collect()
}

/// Loads started-attempt owner facts only when the history matches the central count.
pub async fn load_complete(
    conn: &mut sqlx::PgConnection,
    task: uuid::Uuid,
    started: u32,
) -> Result<Option<Vec<crate::replacement::Prior>>, aq_machine_control::Error> {
    let priors = load(conn, task).await?;
    Ok((priors.len() == started as usize).then_some(priors))
}

/// First `state_format_version` carrying `profiled_auto_settlements` (migration 59, D3).
pub const AUTO_SETTLEMENT_FORMAT: u32 = 59;
/// First `state_format_version` carrying `stall_quarantines` (migration 81, H3).
pub const STALL_QUARANTINE_FORMAT: u32 = 81;
async fn format(conn: &mut sqlx::PgConnection) -> Result<u32, aq_machine_control::Error> {
    let version: String =
        sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&mut *conn)
            .await?;
    version
        .parse()
        .map_err(|_| aq_machine_control::Error::Invalid("format"))
}
/// Whether the H3 stall ladder quarantined this lineage. A store held at an earlier migration
/// prefix has no quarantine table and so records no quarantine, instead of failing every poll
/// and replacement assessment.
pub async fn lineage_quarantined(
    conn: &mut sqlx::PgConnection,
    task: uuid::Uuid,
) -> Result<bool, aq_machine_control::Error> {
    if format(conn).await? < STALL_QUARANTINE_FORMAT {
        return Ok(false);
    }
    Ok(
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM stall_quarantines WHERE lineage_id=$1)")
            .bind(task)
            .fetch_one(&mut *conn)
            .await?,
    )
}
/// Whether a result of this task was refused because its delivery was definitively not
/// published. A store at an earlier migration prefix has no automatic settlement table and so
/// records no unpublished delivery (as for quarantines).
pub async fn delivery_unpublished(
    conn: &mut sqlx::PgConnection,
    task: uuid::Uuid,
) -> Result<bool, aq_machine_control::Error> {
    if format(conn).await? < AUTO_SETTLEMENT_FORMAT {
        return Ok(false);
    }
    Ok(sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM profiled_auto_settlements s JOIN execution_attempts a USING(attempt_id) WHERE a.task_id=$1 AND s.disposition='rework_required' AND s.reason='delivery_unpublished' AND s.evidence->>'decided_by' IS DISTINCT FROM 'playbook')")
        .bind(task)
        .fetch_one(&mut *conn)
        .await?)
}

/// The project revision pinned by an attempt's linked source export request.
pub async fn predecessor_project_revision<'e>(
    executor: impl sqlx::Executor<'e, Database = sqlx::Postgres>,
    attempt: uuid::Uuid,
) -> Result<Option<u64>, aq_machine_control::Error> {
    let pinned: Option<Option<i64>> = sqlx::query_scalar(
        "SELECT (convert_from(e.request_bytes,'UTF8')::jsonb->>'project_revision')::BIGINT FROM profiled_launch_source_exports x JOIN profiled_source_exports e USING(operation_id) WHERE x.attempt_id=$1",
    )
    .bind(attempt)
    .fetch_optional(executor)
    .await?;
    Ok(pinned.flatten().and_then(|r| u64::try_from(r).ok()))
}

/// `attempt.show` for a profiled attempt: the owner facts B4 decides on, read-only and
/// server-owned (no worker text, no input). `None` when `attempt` is not a profiled attempt.
/// `blockers` names each fact that keeps this attempt from being replaced
/// ([`crate::replacement::blockers`]); `in_history` is false for an attempt B4 does not count
/// (never granted, or left out by an owner's fresh start).
pub async fn inspect(
    conn: &mut sqlx::PgConnection,
    attempt: uuid::Uuid,
    now: u64,
) -> Result<Option<serde_json::Value>, aq_machine_control::Error> {
    use sqlx::Row;
    let Some(row) = sqlx::query("SELECT a.task_id,a.holds_task,h.state FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) WHERE a.attempt_id=$1 AND a.kind='profiled_v1'")
        .bind(attempt).fetch_optional(&mut *conn).await?
    else {
        return Ok(None);
    };
    let task: uuid::Uuid = row.get("task_id");
    let hot: serde_json::Value = row.get("state");
    let prior = load(&mut *conn, task).await?;
    let prior = prior.into_iter().find(|p| p.attempt_id == attempt);
    let evaluations: Vec<String> = sqlx::query_scalar("SELECT disposition FROM profiled_settlement_evaluations WHERE attempt_id=$1 ORDER BY operation_id")
        .bind(attempt).fetch_all(&mut *conn).await?;
    let settlements: Vec<serde_json::Value> = if format(conn).await? >= AUTO_SETTLEMENT_FORMAT {
        sqlx::query_scalar("SELECT jsonb_build_object('report_id',report_id,'disposition',disposition,'reason',reason,'decided_by',evidence->>'decided_by') FROM profiled_auto_settlements WHERE attempt_id=$1 ORDER BY created_at_ms,report_id")
            .bind(attempt).fetch_all(&mut *conn).await?
    } else {
        Vec::new()
    };
    let facts = prior.as_ref().map(|p| {
        serde_json::json!({"released": p.released, "accepted": p.accepted, "rejected": p.rejected,
            "needs_reconciliation": p.needs_reconciliation, "cancelled_settled": p.cancelled_settled,
            "completion": p.completion, "exempt": p.exempt, "forgiven": p.forgiven,
            "blockers": crate::replacement::blockers(p, now)})
    });
    Ok(Some(serde_json::json!({
        "kind": "profiled_v1", "attempt_id": attempt, "task_id": task,
        "holds_task": row.get::<bool, _>("holds_task"), "status": hot["status"],
        "execution_deadline_ms": hot["execution_deadline_ms"], "stop": hot["stop"],
        "retention": hot["retention"], "effects": hot["effects"], "in_history": prior.is_some(),
        "replacement": facts, "settlement_evaluations": evaluations, "auto_settlements": settlements,
    })))
}

use crate::replacement::Assessment;
pub async fn fence_revision<'e>(
    executor: impl sqlx::Executor<'e, Database = sqlx::Postgres>,
    assessment: Assessment,
    project_revision: u64,
) -> Result<Assessment, aq_machine_control::Error> {
    let Some(replacement) = assessment.replacement() else {
        return Ok(assessment);
    };
    let pinned = predecessor_project_revision(executor, replacement.predecessor);
    let pinned = pinned.await?;
    if pinned != Some(project_revision) {
        return Ok(Assessment::Blocked("replacement_project_revision_changed"));
    }
    Ok(assessment)
}

/// Persist the predecessor's immutable release receipt and clear its claim on the caller's transaction.
pub async fn record_release(
    conn: &mut sqlx::PgConnection,
    task: uuid::Uuid,
    release: &crate::replacement::Replacement,
    now: u64,
) -> Result<(), aq_machine_control::Error> {
    sqlx::query("INSERT INTO profiled_replacement_releases(attempt_id,task_id,reason,stop_receipt_digest,retained_source_receipt_digest,effect_reconciliation_receipt_digest,attempts_started,recorded_at_ms) VALUES($1,$2,'failed_attempt_replacement',$3,$4,$5,$6,$7)")
            .bind(release.predecessor)
            .bind(task)
            .bind(release.stop_receipt.digest.as_str())
            .bind(release.retained_source_receipt.digest.as_str())
            .bind(release.effect_reconciliation_receipt.digest.as_str())
            .bind(i32::try_from(release.attempts_started).map_err(|_| aq_machine_control::Error::Invalid("release counter out of range"))?)
            .bind(i64::try_from(now).map_err(|_| aq_machine_control::Error::Invalid("release counter out of range"))?)
            .execute(&mut *conn)
            .await?;
    sqlx::query(
        "UPDATE execution_attempts SET holds_task=FALSE WHERE attempt_id=$1 AND holds_task",
    )
    .bind(release.predecessor)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Acquire the standard node then attempt locks before validating a predecessor release.
pub async fn lock_release(
    conn: &mut sqlx::PgConnection,
    task: uuid::Uuid,
    attempt: uuid::Uuid,
) -> Result<(), sqlx::Error> {
    let node: uuid::Uuid = sqlx::query_scalar("SELECT node_id FROM execution_attempts WHERE attempt_id=$1 AND task_id=$2 AND kind='profiled_v1'")
        .bind(attempt)
        .bind(task)
        .fetch_one(&mut *conn)
        .await?;
    for setting in [
        "SET LOCAL lock_timeout='2s'",
        "SET LOCAL statement_timeout='5s'",
    ] {
        sqlx::query(setting).execute(&mut *conn).await?;
    }
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(node)
        .fetch_one(&mut *conn)
        .await?;
    sqlx::query("SELECT 1 FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) WHERE a.attempt_id=$1 FOR UPDATE OF a,h")
        .bind(attempt)
        .fetch_one(&mut *conn)
        .await?;
    Ok(())
}
