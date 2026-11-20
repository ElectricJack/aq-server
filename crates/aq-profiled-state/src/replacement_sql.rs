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
pub async fn predecessor_project_revision(
    pool: &sqlx::PgPool,
    attempt: uuid::Uuid,
) -> Result<Option<u64>, aq_machine_control::Error> {
    let pinned: Option<Option<i64>> = sqlx::query_scalar(
        "SELECT (convert_from(e.request_bytes,'UTF8')::jsonb->>'project_revision')::BIGINT FROM profiled_launch_source_exports x JOIN profiled_source_exports e USING(operation_id) WHERE x.attempt_id=$1",
    )
    .bind(attempt)
    .fetch_optional(pool)
    .await?;
    Ok(pinned.flatten().and_then(|r| u64::try_from(r).ok()))
}
