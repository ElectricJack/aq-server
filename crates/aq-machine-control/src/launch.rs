//! Initial human launch placement; started attempts can use normal recovery routing.
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

pub async fn eligible(
    pool: &PgPool,
    task: Uuid,
    node: Uuid,
    metadata: &Value,
) -> Result<bool, sqlx::Error> {
    let target = metadata
        .get("aq:requested_node_id")
        .and_then(Value::as_str)
        .and_then(|s| s.parse::<Uuid>().ok());
    if target.is_none_or(|target| target == node) {
        return Ok(true);
    }
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM execution_attempts a JOIN profiled_started_receipts s USING(attempt_id) WHERE a.task_id=$1)")
        .bind(task).fetch_one(pool).await
}
