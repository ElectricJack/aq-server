use super::*;

/// Counts a supervisor's retry of the failed run `id` and returns the count
/// the retry carries.
pub async fn count_supervisor_attempt(pool: &PgPool, id: Uuid) -> Result<i32> {
    sqlx::query_scalar(
        "UPDATE internal_tasks SET supervisor_attempts=supervisor_attempts+1
         WHERE id=$1 AND status='failed' RETURNING supervisor_attempts",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| Error::Conflict("Only a failed run can be retried".into()))
}

/// Moves failed runs on (spec 0012 §8) and returns those now with a
/// supervisor, each to be told once:
/// - a run that asked to escalate, or a retry of an escalated run, goes to a
///   supervisor; one carrying [`SUPERVISOR_ATTEMPTS`] goes to the owner;
/// - a purpose with [`FAILURE_BURST_RUNS`] failed runs within
///   [`FAILURE_BURST_WINDOW_MS`] sends its newest failed run to a supervisor,
///   unless a run of that purpose escalated within the window;
/// - a run with a supervisor for [`SUPERVISOR_SILENCE_MS`] that nobody
///   retried, escalated or resolved goes to the owner.
///
/// Every move is conditional on the run's current state, so two servers
/// never move, or tell a supervisor about, the same run twice.
pub async fn escalate_due(pool: &PgPool, now_ms: i64) -> Result<Vec<Row>> {
    const ASKED: &str = "t.status='failed' AND t.escalation='none'
        AND (t.escalate OR t.retry_of IS NOT NULL AND EXISTS(SELECT 1 FROM internal_tasks o
            WHERE o.id=t.retry_of AND o.escalation IN ('supervisor','owner')))";
    sqlx::query(&format!(
        "UPDATE internal_tasks t SET escalation='owner', escalated_at_ms=$1, diagnosis=$3
         WHERE {ASKED} AND t.supervisor_attempts>=$2"
    ))
    .bind(now_ms)
    .bind(SUPERVISOR_ATTEMPTS)
    .bind("supervisor attempts exhausted")
    .execute(pool)
    .await?;
    let mut moved = sqlx::query(&format!(
        "UPDATE internal_tasks t SET escalation='supervisor', escalated_at_ms=$1
         WHERE {ASKED} AND t.supervisor_attempts<$2 RETURNING t.*"
    ))
    .bind(now_ms)
    .bind(SUPERVISOR_ATTEMPTS)
    .fetch_all(pool)
    .await?;
    let burst = sqlx::query(
        "WITH recent AS (
             SELECT id,purpose,finished_at_ms FROM internal_tasks
             WHERE status='failed' AND finished_at_ms>$1-$3),
         newest AS (
             SELECT DISTINCT ON (r.purpose) r.id FROM recent r
             WHERE r.purpose IN (SELECT purpose FROM recent GROUP BY purpose HAVING count(*)>=$2)
               AND NOT EXISTS(SELECT 1 FROM internal_tasks e
                   WHERE e.purpose=r.purpose AND e.escalated_at_ms>$1-$3)
             ORDER BY r.purpose, r.finished_at_ms DESC, r.id DESC)
         UPDATE internal_tasks t SET escalation='supervisor', escalated_at_ms=$1
         FROM newest WHERE t.id=newest.id AND t.escalation='none' RETURNING t.*",
    )
    .bind(now_ms)
    .bind(FAILURE_BURST_RUNS)
    .bind(FAILURE_BURST_WINDOW_MS)
    .fetch_all(pool)
    .await?;
    moved.extend(burst);
    sqlx::query(
        "UPDATE internal_tasks t SET escalation='owner', escalated_at_ms=$1, diagnosis=$3
         WHERE t.escalation='supervisor' AND t.escalated_at_ms<=$1-$2
           AND NOT EXISTS(SELECT 1 FROM internal_tasks r WHERE r.retry_of=t.id)",
    )
    .bind(now_ms)
    .bind(SUPERVISOR_SILENCE_MS)
    .bind("supervisor did not respond")
    .execute(pool)
    .await?;
    moved.iter().map(Row::decode).collect()
}

/// Sends the failed run `id` to the owner with `diagnosis` (a supervisor's
/// `/escalate`).
pub async fn mark_owner(pool: &PgPool, id: Uuid, diagnosis: &str, now_ms: i64) -> Result<()> {
    let moved = sqlx::query(
        "UPDATE internal_tasks SET escalation='owner', diagnosis=$2, escalated_at_ms=$3
         WHERE id=$1 AND status='failed' AND escalation<>'resolved'",
    )
    .bind(id)
    .bind(diagnosis)
    .bind(now_ms)
    .execute(pool)
    .await?;
    if moved.rows_affected() == 0 {
        found(pool, id).await?;
        return Err(Error::Conflict(
            "Only an unresolved failed run can be escalated".into(),
        ));
    }
    Ok(())
}

/// Marks the escalated run `id`, and every escalated run it retried, resolved.
pub async fn resolve(pool: &PgPool, id: Uuid) -> Result<()> {
    let mut tx = pool.begin().await?;
    let escalated: Option<String> =
        sqlx::query_scalar("SELECT escalation FROM internal_tasks WHERE id=$1 FOR UPDATE")
            .bind(id)
            .fetch_optional(&mut *tx)
            .await?;
    match escalated.as_deref() {
        None => return Err(not_found()),
        Some("supervisor" | "owner") => {}
        Some(_) => {
            return Err(Error::Conflict(
                "Only a run with a supervisor or the owner can be resolved".into(),
            ));
        }
    }
    resolve_chain(&mut tx, id).await?;
    tx.commit().await?;
    Ok(())
}

/// `id` and the runs it retried, back to the first: those with a supervisor
/// or the owner become `resolved`.
pub(super) async fn resolve_chain(tx: &mut sqlx::PgConnection, id: Uuid) -> Result<()> {
    sqlx::query(
        "WITH RECURSIVE chain(id) AS (
             SELECT $1::uuid
             UNION SELECT t.retry_of FROM internal_tasks t JOIN chain c ON t.id=c.id
                 WHERE t.retry_of IS NOT NULL)
         UPDATE internal_tasks SET escalation='resolved'
         WHERE id IN (SELECT id FROM chain) AND escalation IN ('supervisor','owner')",
    )
    .bind(id)
    .execute(tx)
    .await?;
    Ok(())
}

async fn found(pool: &PgPool, id: Uuid) -> Result<()> {
    get(pool, id).await?.map(drop).ok_or_else(not_found)
}

fn not_found() -> Error {
    Error::NotFound("Internal run not found".into())
}
