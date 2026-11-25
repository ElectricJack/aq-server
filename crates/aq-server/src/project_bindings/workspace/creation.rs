use super::*;
use aq_human_work::task_graph::single::Create;

pub(crate) async fn prepare(
    s: &AppState,
    home: &ProjectRecord,
    request: &Create,
) -> Result<Vec<Selected>> {
    select_all(s, home, request.operation_id, &request.workspace).await
}
pub(crate) async fn stage(
    tx: &mut Transaction<'_, Postgres>,
    home: Uuid,
    task: Uuid,
    selections: &[Selected],
) -> Result<()> {
    if selections.is_empty() {
        return Ok(());
    }
    for selected in selections {
        selected.fence(tx).await?;
    }
    sqlx::query("INSERT INTO task_workspace_creations(home_project_id,task_id,requirements) VALUES($1,$2,$3)")
        .bind(home).bind(task).bind(serde_json::to_value(selections).map_err(|_|AppError::Internal)?)
        .execute(&mut **tx).await.map_err(db)?;
    Ok(())
}
pub(crate) async fn finish(
    tx: &mut Transaction<'_, Postgres>,
    home: Uuid,
    task: Uuid,
    confirmed: bool,
) -> Result<()> {
    store::lock_task(tx, home, task).await?;
    let saved:Value=sqlx::query_scalar("DELETE FROM task_workspace_creations WHERE home_project_id=$1 AND task_id=$2 RETURNING requirements")
        .bind(home).bind(task).fetch_one(&mut **tx).await.map_err(db)?;
    if confirmed {
        let selected: Vec<Selected> =
            serde_json::from_value(saved).map_err(|_| AppError::Internal)?;
        for requirement in selected {
            requirement.save(tx, home, task, None).await?;
        }
    }
    Ok(())
}
