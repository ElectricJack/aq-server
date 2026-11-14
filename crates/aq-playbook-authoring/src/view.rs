//! What the owner is shown about a source (spec 0013 §4.4): the playbook in
//! effect with its cases, whether a rollback is offered, and the latest run.
//! Text here is for the owner: sentences, never step ids or digests.
use crate::{Result, SourceRecord, complete::rollback_target, store};
use serde_json::{Value, json};
use sqlx::{PgPool, Row, types::Json};

fn column<T>(row: &sqlx::postgres::PgRow, name: &str) -> Result<Option<T>>
where
    T: for<'r> sqlx::Decode<'r, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    Ok(row.try_get::<Option<T>, _>(name)?)
}

fn json_column(row: &sqlx::postgres::PgRow, name: &str) -> Result<Value> {
    Ok(column::<Json<Value>>(row, name)?.map_or(Value::Null, |v| v.0))
}

async fn active(pool: &PgPool, source: &str) -> Result<Value> {
    let row = sqlx::query(
        "SELECT artifact_digest,comparison,cases,activated_at_ms,finished_at_ms
         FROM playbook_authoring_runs WHERE source_id=$1 AND status='saved' AND active",
    )
    .bind(source)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(Value::Null);
    };
    let since = column::<i64>(&row, "activated_at_ms")?.or(column(&row, "finished_at_ms")?);
    let previous = rollback_target(pool, source, false).await?;
    let cases = match json_column(&row, "cases")? {
        Value::Array(cases) => Value::Array(cases),
        _ => json!([]),
    };
    Ok(json!({
        "artifact": column::<String>(&row, "artifact_digest")?,
        "since_ms": since.unwrap_or(0).max(0),
        "comparison": json_column(&row, "comparison")?,
        "can_rollback": previous.is_some(),
        "previous": previous.map(|(_, _, since)| json!({"since_ms": since.unwrap_or(0).max(0)})),
        "cases": cases,
    }))
}

async fn latest(pool: &PgPool, source: &str, now_ms: i64) -> Result<Value> {
    let row = sqlx::query(
        "SELECT id,status,question,unable,refusal,internal_task_id,created_at_ms,finished_at_ms
         FROM playbook_authoring_runs WHERE source_id=$1 ORDER BY id DESC LIMIT 1",
    )
    .bind(source)
    .fetch_optional(pool)
    .await?;
    let Some(row) = row else {
        return Ok(Value::Null);
    };
    let created: i64 = row.try_get("created_at_ms")?;
    let finished = column::<i64>(&row, "finished_at_ms")?;
    Ok(json!({
        "run": row.try_get::<i64, _>("id")?.to_string(),
        "status": row.try_get::<String, _>("status")?,
        "question": json_column(&row, "question")?,
        "unable": json_column(&row, "unable")?,
        "refusal": column::<String>(&row, "refusal")?,
        "internal_task_id": column::<uuid::Uuid>(&row, "internal_task_id")?,
        "created_at_ms": created.max(0),
        "finished_at_ms": finished,
        "elapsed_ms": (finished.unwrap_or(now_ms) - created).max(0),
    }))
}

/// The `SourceView` of one source; `now_ms` is the server clock.
pub async fn view(pool: &PgPool, source: &SourceRecord, now_ms: i64) -> Result<Value> {
    Ok(json!({
        "id": source.id,
        "kind": source.kind,
        "binding": source.binding,
        "purpose": source.purpose,
        "revision": source.revision,
        "active": active(pool, &source.id).await?,
        "latest": latest(pool, &source.id, now_ms).await?,
    }))
}

/// `{items: [SourceView]}` of every source.
pub async fn list(pool: &PgPool, now_ms: i64) -> Result<Value> {
    let mut items = vec![];
    for source in store::list(pool).await? {
        items.push(view(pool, &source, now_ms).await?);
    }
    Ok(json!({"items": items}))
}
