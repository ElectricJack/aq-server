//! A durable needs edit fences both native offers and delivery for older Graph observations.
use sqlx::PgConnection;
use uuid::Uuid;

pub async fn blocks_observation(
    db: &mut PgConnection,
    (project, task, version): (Uuid, Uuid, i64),
) -> Result<bool, sqlx::Error> {
    let supported: bool =
        sqlx::query_scalar("SELECT to_regclass('dashboard_graph_intents') IS NOT NULL")
            .fetch_one(&mut *db)
            .await?;
    if !supported {
        return Ok(false);
    }
    sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM dashboard_graph_intents i JOIN dashboard_operations d USING(operation_id)
         WHERE i.project_id=$1 AND d.kind='task.needs.set'
           AND d.response#>>'{subject_refs,0,id}'=$2::uuid::text
           AND (d.disposition IN ('pending','unknown') OR (d.disposition='confirmed'
             AND d.response#>>'{subject_refs,0,revision}' ~ '^[1-9][0-9]{0,18}$'
             AND (d.response#>>'{subject_refs,0,revision}')::numeric >= $3)))",
    )
    .bind(project).bind(task).bind(version)
    .fetch_one(db).await
}
