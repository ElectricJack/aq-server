//! Atomic per-attempt reservations shared by single and batched draft creation.
use super::AttemptIds;
use serde_json::{Value, json};
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

pub struct Reservation {
    pub task: Uuid,
    pub parent: Uuid,
    pub root: Uuid,
    pub depth: u32,
}

pub struct DraftReservation {
    pub ids: AttemptIds,
    pub tasks: Vec<Reservation>,
    pub ceiling: Option<u32>,
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

pub async fn lock_operation(
    tx: &mut Transaction<'_, Postgres>,
    operation: Uuid,
) -> Result<(), sqlx::Error> {
    lock_key(tx, format!("agent-operation:{operation}")).await
}

async fn lock_key(tx: &mut Transaction<'_, Postgres>, key: String) -> Result<(), sqlx::Error> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1::text,0))")
        .bind(key)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
pub struct SingleReservation {
    pub ids: AttemptIds,
    pub operation: Uuid,
    pub parent: Option<Uuid>,
    pub target: super::Target,
}
/// Reserve an agent-created task or recognize an existing reservation for this
/// operation. The caller holds the transaction until its decision is audited.
pub async fn reserve_single(
    tx: &mut Transaction<'_, Postgres>,
    row: SingleReservation,
    ceiling: Option<u32>,
    now: impl FnOnce() -> i64,
) -> Result<bool, sqlx::Error> {
    lock_key(tx, row.ids.attempt_id.to_string()).await?;
    let (count,mine):(i64,bool)=sqlx::query_as("SELECT count(*),COALESCE(bool_or(task_id=$2),FALSE) FROM agent_created_tasks WHERE attempt_id=$1")
        .bind(row.ids.attempt_id).bind(row.operation).fetch_one(&mut **tx).await?;
    if !mine && count >= ceiling.map_or(i64::MAX, i64::from) {
        return Ok(false);
    }
    let depth = match row.target {
        super::Target::Task {
            relation: super::Relation::Descendant(depth),
            ..
        } => Some(depth as i32),
        _ => None,
    };
    sqlx::query("INSERT INTO agent_created_tasks(task_id,attempt_id,project_id,parent_id,root_task_id,depth,operation_id,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,$1,$7) ON CONFLICT DO NOTHING")
        .bind(row.operation).bind(row.ids.attempt_id).bind(row.ids.project_id).bind(row.parent).bind(row.ids.task_id).bind(depth).bind(now())
        .execute(&mut **tx).await?;
    Ok(true)
}
/// Reserve within the caller's canonical operation admission transaction.
pub async fn reserve(
    tx: &mut Transaction<'_, Postgres>,
    operation: Uuid,
    reservation: DraftReservation,
    created_at_ms: i64,
) -> Result<(), ReserveError> {
    let DraftReservation {
        ids,
        mut tasks,
        ceiling,
    } = reservation;
    // Use the same lock as the legacy single-task creation quota.
    lock_key(tx, ids.attempt_id.to_string()).await?;
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM agent_created_tasks c WHERE c.attempt_id=$1 AND NOT EXISTS (
            SELECT 1 FROM dashboard_operations d
            WHERE d.operation_id=c.operation_id AND d.principal_id=c.attempt_id
              AND d.kind IN ('task.create','task.graph.create') AND d.disposition IN ('conflict','rejected')
        )")
            .bind(ids.attempt_id)
            .fetch_one(&mut **tx)
            .await?;
    if count + tasks.len() as i64 > ceiling.map_or(i64::MAX, i64::from) {
        return Err(ReserveError::QuotaExceeded);
    }
    tasks.sort_unstable_by_key(|row| row.task);
    for row in tasks {
        let inserted = sqlx::query("INSERT INTO agent_created_tasks(task_id,attempt_id,project_id,parent_id,root_task_id,depth,operation_id,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8) ON CONFLICT DO NOTHING")
            .bind(row.task).bind(ids.attempt_id).bind(ids.project_id).bind(row.parent).bind(row.root)
            .bind(row.depth as i32).bind(operation).bind(created_at_ms)
            .execute(&mut **tx).await?;
        if inserted.rows_affected() != 1 {
            return Err(ReserveError::Conflict);
        }
    }
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

pub struct EnsuredWork {
    pub kind: Option<String>,
    pub subject: Option<Uuid>,
}

/// Resolve a work task's kind and motivation from the host's retained request.
/// Synchronous profiles retain the identity key and the stamped Graph node.
pub async fn ensured_work(
    pool: &PgPool,
    node: &Value,
    project: Uuid,
) -> Result<Option<EnsuredWork>, sqlx::Error> {
    use super::draft_scope::uuid;
    let metadata = &node["metadata"];
    if metadata["aq:project_id"] != json!(project) {
        return Ok(None);
    }
    let (Some(task), Some(workspace), Some(namespace), Some(key)) = (
        uuid(&node["id"]),
        uuid(&node["workspace_id"]),
        uuid(&metadata["aq:playbook_namespace"]),
        metadata["aq:logical_key"].as_str(),
    ) else {
        return Ok(None);
    };
    let found: Option<Option<Value>> = sqlx::query_scalar(
        "SELECT h.request FROM playbook_work_keys k JOIN playbook_namespaces n USING(namespace_id,generation)
         LEFT JOIN LATERAL (
            SELECT request FROM playbook_host_operations
            WHERE namespace_id=k.namespace_id AND generation=k.generation
              AND command_name='work.ensure' AND subject_id=k.task_id::text
            ORDER BY created_at_ms,operation_id LIMIT 1
         ) h ON true
         WHERE k.namespace_id=$1 AND k.logical_key=$2 AND k.task_id=$3 AND n.project_id=$4 AND n.workspace_id=$5",
    ).bind(namespace).bind(key).bind(task).bind(project).bind(workspace).fetch_optional(pool).await?;
    Ok(found.map(|request| {
        let (kind, subject) = match &request {
            Some(request) => (&request["kind"], &request["motivation"]["id"]),
            None => (&metadata["aq:work_kind"], &metadata["aq:subject"]["id"]),
        };
        EnsuredWork {
            kind: kind.as_str().map(str::to_owned),
            subject: uuid(subject),
        }
    }))
}
