use super::*;

/// Inserts an `owner_stop` row for every profiled attempt on the machine's live
/// nodes that holds its task and whose hot stop kind is 'unknown' or 'running'.
/// Returns the attempt ids now pending.
pub async fn request_handoffs(
    tx: &mut PgConnection,
    key: &str,
    now: DateTime<Utc>,
) -> Result<Vec<Uuid>, Error> {
    let rows: Vec<(Uuid, Uuid)> = sqlx::query_as(RUNNING_ATTEMPTS)
        .bind(key)
        .fetch_all(&mut *tx)
        .await?;
    for (attempt, node) in &rows {
        sqlx::query(
            "INSERT INTO attempt_handoffs(attempt_id,node_id,reason,requested_at) \
             VALUES($1,$2,'owner_stop',$3) ON CONFLICT DO NOTHING",
        )
        .bind(attempt)
        .bind(node)
        .bind(now)
        .execute(&mut *tx)
        .await?;
    }
    Ok(rows.into_iter().map(|(a, _)| a).collect())
}

/// Attempts with any handoff row that still hold their task and are running.
pub async fn pending_handoffs(conn: &mut PgConnection, key: &str) -> Result<Vec<Uuid>, Error> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT f.attempt_id {PENDING_FILTER} AND n.machine_key=$1 ORDER BY f.attempt_id LIMIT 64"
    ))
    .bind(key)
    .fetch_all(&mut *conn)
    .await?)
}

/// Pending handoff counts per key, for the swarm read.
pub async fn pending_counts(
    conn: &mut PgConnection,
    keys: &[String],
) -> Result<BTreeMap<String, u32>, Error> {
    let rows: Vec<(String, i64)> = sqlx::query_as(&format!(
        "SELECT n.machine_key, count(*) {PENDING_FILTER} AND n.machine_key = ANY($1) GROUP BY n.machine_key"
    ))
    .bind(keys)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows.into_iter().map(|(k, n)| (k, n as u32)).collect())
}

/// Node-announced handoff (review I1). The attempt must belong to `node`. An
/// exempt reason (`owner_stop`, `service_stop`) is refused unless the attempt
/// still holds its task and runs. A node-posted `owner_stop` counts only while
/// the machine is desired stopped by a stop requested before the handoff (one
/// minute of slack); otherwise it is stored as `restart`. Returns the reason
/// on record (an earlier row, such as the owner's own Stop, wins).
pub async fn record_handoff(
    conn: &mut PgConnection,
    node: Uuid,
    notice: &aq_protocol::HandoffNotice,
    now: DateTime<Utc>,
) -> Result<HandoffReason, Error> {
    let attempt = notice.attempt_id;
    let reason = notice.reason;
    let row = sqlx::query(
        "SELECT a.node_id, COALESCE(a.holds_task AND h.state->'stop'->>'kind' IN ('unknown','running'), FALSE) AS running, \
         COALESCE(c.desired = 'stopped' AND c.updated_at <= $2 + interval '1 minute', FALSE) AS stopped \
         FROM execution_attempts a JOIN nodes n ON n.id = a.node_id \
         LEFT JOIN profiled_authority_hot h ON h.attempt_id = a.attempt_id \
         LEFT JOIN machine_controls c ON c.machine_key = n.machine_key WHERE a.attempt_id=$1",
    )
    .bind(attempt)
    .bind(now)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row.filter(|r| r.get::<Uuid, _>("node_id") == node) else {
        return Err(Error::Invalid("attempt_not_owned"));
    };
    if reason != HandoffReason::Restart && !row.try_get::<bool, _>("running")? {
        return Err(Error::Refused(
            "handoff_not_running: the attempt is no longer running",
        ));
    }
    let reason = match reason {
        HandoffReason::OwnerStop if !row.try_get::<bool, _>("stopped")? => HandoffReason::Restart,
        r => r,
    };
    sqlx::query(
        "INSERT INTO attempt_handoffs(attempt_id,node_id,reason,requested_at) \
         VALUES($1,$2,$3,$4) ON CONFLICT (attempt_id) DO NOTHING",
    )
    .bind(attempt)
    .bind(node)
    .bind(reason_str(reason))
    .bind(now)
    .execute(&mut *conn)
    .await?;
    let stored: String =
        sqlx::query_scalar("SELECT reason FROM attempt_handoffs WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(&mut *conn)
            .await?;
    reason_of(&stored)
}

/// The commit a task's lineage started from (review C-1). A handoff successor is admitted on
/// its predecessor's saved commit, the handoff candidate; that candidate was built on the
/// predecessor's own admitted base (its frozen `base` input), followed back through every
/// handoff of the task. An attempt not admitted on a handoff candidate starts from `base`.
/// `None` (R1-M1): more than [`MAX_LINEAGE_HOPS`] handoffs; the caller holds, never guesses.
pub async fn lineage_base(
    conn: &mut PgConnection,
    task: Uuid,
    base: &str,
) -> Result<Option<String>, Error> {
    let mut current = base.to_owned();
    if !has_format(conn).await? {
        return Ok(Some(current));
    }
    for _ in 0..=MAX_LINEAGE_HOPS {
        let earlier: Option<Option<String>> = sqlx::query_scalar(
            "SELECT i.inputs->'base'->>'hex' FROM attempt_handoffs h \
             JOIN execution_attempts a ON a.attempt_id=h.attempt_id \
             JOIN delivery_intents i ON i.operation_id=h.candidate_operation_id \
             WHERE a.task_id=$1 AND h.candidate_oid=$2 AND i.operation='candidate.create' \
             ORDER BY h.requested_at LIMIT 1",
        )
        .bind(task)
        .bind(&current)
        .fetch_optional(&mut *conn)
        .await?;
        match earlier.flatten() {
            Some(earlier) if earlier != current => current = earlier,
            _ => return Ok(Some(current)),
        }
    }
    Ok(None)
}
/// Handoffs [`lineage_base`] follows back; a task has a few (5 exempt, then charged).
pub const MAX_LINEAGE_HOPS: usize = 64;
pub async fn handoff_of(
    conn: &mut PgConnection,
    attempt: Uuid,
) -> Result<Option<HandoffRow>, Error> {
    let Some(r) = sqlx::query(
        "SELECT attempt_id,node_id,reason,candidate_operation_id,candidate_oid,note_comment_id \
         FROM attempt_handoffs WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_optional(&mut *conn)
    .await?
    else {
        return Ok(None);
    };
    Ok(Some(HandoffRow {
        attempt_id: r.try_get("attempt_id")?,
        node_id: r.try_get("node_id")?,
        reason: reason_of(&r.try_get::<String, _>("reason")?)?,
        candidate_operation_id: r.try_get("candidate_operation_id")?,
        candidate_oid: r.try_get("candidate_oid")?,
        note_comment_id: r.try_get("note_comment_id")?,
    }))
}

/// Live acceptance D5: the project, repository and binding generation of
/// `attempt`'s own source export, which its handoff candidate must share.
pub async fn export_scope(
    conn: &mut PgConnection,
    attempt: Uuid,
) -> Result<Option<(Uuid, Uuid, u64)>, Error> {
    let row = sqlx::query(
        "SELECT e.project_id, (q.v->>'repository_id')::uuid AS repository, \
         (q.v->>'binding_generation')::bigint AS generation \
         FROM profiled_launch_source_exports x JOIN profiled_source_exports e USING(operation_id), \
         LATERAL (SELECT convert_from(e.request_bytes,'UTF8')::jsonb AS v) q \
         WHERE x.attempt_id=$1 LIMIT 1",
    )
    .bind(attempt)
    .fetch_optional(&mut *conn)
    .await?;
    let Some(row) = row else { return Ok(None) };
    let generation = u64::try_from(row.try_get::<i64, _>("generation")?)
        .map_err(|_| Error::Invalid("source export scope"))?;
    Ok(Some((
        row.try_get("project_id")?,
        row.try_get("repository")?,
        generation,
    )))
}

/// Only fills NULL columns; equal values are a no-op.
pub async fn set_handoff_candidate(
    conn: &mut PgConnection,
    attempt: Uuid,
    candidate: HandoffCandidate<'_>,
) -> Result<(), Error> {
    let HandoffCandidate {
        operation,
        oid,
        note_comment,
    } = candidate;
    let n = sqlx::query(
        "UPDATE attempt_handoffs SET candidate_operation_id=COALESCE(candidate_operation_id,$2), \
         candidate_oid=COALESCE(candidate_oid,$3), note_comment_id=COALESCE(note_comment_id,$4) \
         WHERE attempt_id=$1",
    )
    .bind(attempt)
    .bind(operation)
    .bind(oid)
    .bind(note_comment)
    .execute(&mut *conn)
    .await?
    .rows_affected();
    if n == 0 { Err(Error::NotFound) } else { Ok(()) }
}

pub struct HandoffCandidate<'a> {
    pub operation: Uuid,
    pub oid: &'a str,
    pub note_comment: Option<Uuid>,
}
