//! The work inspector's Graph history entries and project-scoped execution timeline.
use serde_json::{Value, json};
use sqlx::{PgPool, Row};
use uuid::Uuid;

const ATTEMPT_LIMIT: i64 = 16;

pub async fn items(
    pool: &PgPool,
    service: Option<&str>,
    scope: (Uuid, Uuid),
    task: Uuid,
    rows: &[Value],
    prior: Option<&Value>,
) -> Option<Vec<Value>> {
    let changes = crate::changes(rows, task, prior);
    let actors = crate::attribution::history(pool, service, scope, rows)
        .await
        .ok()?;
    rows
        .iter()
        .zip(changes.into_iter().zip(actors))
        .map(|(h, (change, actor))| {
            let sequence = h["sequence"].as_u64().filter(|n| *n > 0)?;
            Some(json!({
                "sequence": sequence.to_string(),
                "entry_id": h["entry_id"], "category": h["category"], "operation": h["operation"],
                "actor_id": actor, "reason": h["reason"], "created_at": h["created_at"], "change": change,
            }))
        })
        .collect()
}

pub async fn attempts(
    pool: &PgPool,
    project: Uuid,
    task: Uuid,
) -> Result<(Vec<Value>, bool), sqlx::Error> {
    let attempts = sqlx::query(
        "SELECT e.attempt_id,e.session_id,e.holds_task,COALESCE(h.state->>'status',e.state->>'status','unknown') AS status \
         FROM execution_attempts e LEFT JOIN profiled_authority_hot h USING(attempt_id) \
         WHERE e.task_id=$1 AND e.kind='profiled_v1' \
         AND e.state->'offer'->'manifest'->'bindings'->>'project_id'=$2 \
         ORDER BY e.holds_task DESC,e.attempt_id LIMIT $3",
    )
    .bind(task)
    .bind(project.to_string())
    .bind(ATTEMPT_LIMIT + 1)
    .fetch_all(pool)
    .await?;
    let attempts_truncated = attempts.len() as i64 > ATTEMPT_LIMIT;
    let ran_with = aq_role_settings::attempts::ran_with_rows(pool, &attempts).await;
    let attempts: Vec<Value> = attempts
        .iter()
        .take(ATTEMPT_LIMIT as usize)
        .map(|row| {
            let attempt: Uuid = row.try_get("attempt_id")?;
            let mut shown = json!({
                "attempt_id": attempt,
                "session_id": row.try_get::<Uuid, _>("session_id")?,
                "holds_task": row.try_get::<bool, _>("holds_task")?,
                "status": row.try_get::<String, _>("status")?,
            });
            if let Some(summary) = ran_with.get(&attempt) {
                shown["ran_with"] = json!(summary);
            }
            Ok(shown)
        })
        .collect::<Result<_, sqlx::Error>>()?;
    Ok((attempts, attempts_truncated))
}
