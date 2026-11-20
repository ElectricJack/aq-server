//! Atomic per-attempt reservations shared by single and batched draft creation.
use super::AttemptIds;
use sqlx::PgPool;
use uuid::Uuid;

pub struct Reservation {
    pub task: Uuid,
    pub parent: Uuid,
    pub root: Uuid,
    pub depth: u32,
}

#[derive(Debug)]
pub enum ReserveError {
    QuotaExceeded,
    Conflict,
    Store(sqlx::Error),
}
impl From<sqlx::Error> for ReserveError {
    fn from(error: sqlx::Error) -> Self {
        Self::Store(error)
    }
}

pub async fn reserve(
    pool: &PgPool,
    ids: AttemptIds,
    operation: Uuid,
    tasks: Vec<Reservation>,
    ceiling: Option<u32>,
    created_at_ms: i64,
) -> Result<(), ReserveError> {
    let mut tx = pool.begin().await?;
    // Use the same lock as the legacy single-task creation quota.
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text,0))")
        .bind(ids.attempt_id)
        .execute(&mut *tx)
        .await?;
    let saved: Vec<Uuid> = sqlx::query_scalar(
        "SELECT task_id FROM agent_created_tasks WHERE attempt_id=$1 AND operation_id=$2 ORDER BY task_id",
    )
    .bind(ids.attempt_id)
    .bind(operation)
    .fetch_all(&mut *tx)
    .await?;
    if !saved.is_empty() {
        let mut requested: Vec<_> = tasks.iter().map(|r| r.task).collect();
        requested.sort_unstable();
        return if saved == requested {
            Ok(())
        } else {
            Err(ReserveError::Conflict)
        };
    }
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM agent_created_tasks c WHERE c.attempt_id=$1 AND NOT EXISTS (
            SELECT 1 FROM dashboard_operations d
            WHERE d.operation_id=c.operation_id AND d.principal_id=c.attempt_id
              AND d.kind IN ('task.create','task.graph.create') AND d.disposition IN ('conflict','rejected')
        )")
            .bind(ids.attempt_id)
            .fetch_one(&mut *tx)
            .await?;
    if count + tasks.len() as i64 > ceiling.map_or(i64::MAX, i64::from) {
        return Err(ReserveError::QuotaExceeded);
    }
    for row in tasks {
        let inserted = sqlx::query("INSERT INTO agent_created_tasks(task_id,attempt_id,project_id,parent_id,root_task_id,depth,operation_id,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT DO NOTHING")
            .bind(row.task).bind(ids.attempt_id).bind(ids.project_id).bind(row.parent).bind(row.root)
            .bind(row.depth as i32).bind(operation).bind(created_at_ms)
            .execute(&mut *tx).await?;
        if inserted.rows_affected() != 1 {
            return Err(ReserveError::Conflict);
        }
    }
    tx.commit().await?;
    Ok(())
}

/// Older single-task grants used reservations as subtree evidence. Draft
/// batches can name many caller-supplied IDs, so their rows confer authority
/// only after the matching create operation confirms a Graph mutation.
const CREATED: &str = r#"(
    NOT COALESCE(g.grants @> '[{"operation":"task.create.subtask","limits":{"draft_only":true}}]', false)
    OR EXISTS (
        SELECT 1 FROM dashboard_operations d
        WHERE d.operation_id=c.operation_id AND d.principal_id=c.attempt_id
          AND d.kind IN ('task.create','task.graph.create') AND d.disposition='confirmed'
    )
)"#;

pub async fn depth(pool: &PgPool, root: Uuid, task: Uuid) -> Result<Option<i32>, sqlx::Error> {
    let sql = format!(
        "SELECT c.depth FROM agent_created_tasks c LEFT JOIN agent_launch_grants g USING(attempt_id)
         WHERE c.task_id=$1 AND c.root_task_id=$2 AND c.depth IS NOT NULL AND {CREATED}"
    );
    sqlx::query_scalar(&sql)
        .bind(task)
        .bind(root)
        .fetch_optional(pool)
        .await
}

pub async fn subtree<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    root: Uuid,
    project: Uuid,
) -> Result<Vec<Uuid>, sqlx::Error> {
    let sql = format!(
        "SELECT c.task_id FROM agent_created_tasks c LEFT JOIN agent_launch_grants g USING(attempt_id)
         WHERE c.root_task_id=$1 AND c.project_id=$2 AND {CREATED}
         ORDER BY c.task_id LIMIT 256"
    );
    sqlx::query_scalar(&sql)
        .bind(root)
        .bind(project)
        .fetch_all(executor)
        .await
}

pub async fn creator<'e>(
    executor: impl sqlx::PgExecutor<'e>,
    task: Uuid,
) -> Result<Option<serde_json::Value>, sqlx::Error> {
    let sql = format!(
        "SELECT g.grants FROM agent_created_tasks c JOIN agent_launch_grants g USING(attempt_id)
         WHERE c.task_id=$1 AND {CREATED}"
    );
    sqlx::query_scalar(&sql)
        .bind(task)
        .fetch_optional(executor)
        .await
}
