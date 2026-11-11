//! Resume owner cancellation/deletion cleanup after outages and actual stops.
//! Graph task state is terminal immediately; process cleanup never gates it.
use super::*;
use crate::profiled_execution::{cancellation, disposition};

#[derive(Default)]
pub struct Sweep(Option<Uuid>);
impl Sweep {
    pub async fn tick(&mut self, state: &AppState) -> Result<()> {
        let rows: Vec<(Uuid, Uuid, Uuid, Uuid, Uuid)> = sqlx::query_as(
            "SELECT DISTINCT ON(a.attempt_id) a.attempt_id,a.task_id,d.operation_id,d.principal_id,i.workspace_id \
             FROM execution_attempts a JOIN dashboard_operations d ON d.response->'subject_refs'->0->>'id'=a.task_id::text \
             JOIN dashboard_graph_intents i USING(operation_id) \
             WHERE a.kind='profiled_v1' AND a.holds_task AND d.kind IN ('task.cancel','task.delete') \
             AND d.disposition IN ('pending','unknown','confirmed') \
             AND ($1::uuid IS NULL OR a.attempt_id>$1) ORDER BY a.attempt_id,d.created_at DESC LIMIT 8",
        ).bind(self.0).fetch_all(&state.store.pool).await.map_err(|_| unavailable())?;
        if rows.is_empty() {
            self.0 = None;
        }
        for (attempt, task, operation, actor, workspace) in rows {
            self.0 = Some(attempt);
            let result = async {
                let Some(user) = state.store.find_user(actor).await? else {
                    return Ok(());
                };
                crate::dashboard_api::actions::existing::owner(state, &user)
                    .await
                    .map_err(|_| unavailable())?;
                let raw = runtime(state)?
                    .human_get(
                        &format!("/v1/workspaces/{workspace}/nodes/{task}"),
                        &[],
                        1_000_000,
                    )
                    .await?;
                if raw["id"] != json!(task)
                    || raw["workspace_id"] != json!(workspace)
                    || raw["kind"] != "task"
                    || (raw["deleted"] != true && raw["task_state"] != "canceled")
                {
                    return Ok(());
                }
                // A deterministic ID per attempt avoids reusing one cancellation
                // identity if admission raced the Graph lifecycle transition.
                let id = termination_id(operation, attempt);
                cancellation::cancel(state, attempt, id, "Owner canceled or deleted task").await?;
                // This owner alone releases the held claim and requires verified
                // stopped proof. Until then the worker receives cancellation on
                // every poll. Source and unresolved effects stay retained.
                let _ = disposition::abandon(
                    state,
                    &user,
                    attempt,
                    &disposition::Request {
                        operation_id: id,
                        reason: "Owner canceled or deleted task".into(),
                    },
                )
                .await;
                Ok::<_, AppError>(())
            }
            .await;
            if result.is_err() {
                tracing::warn!(%attempt, "task stop cleanup will retry");
            }
        }
        Ok(())
    }
}
fn termination_id(operation: Uuid, attempt: Uuid) -> Uuid {
    use sha2::{Digest, Sha256};
    let bytes = Sha256::digest([operation.as_bytes().as_slice(), attempt.as_bytes()].concat());
    Uuid::from_bytes(bytes[..16].try_into().expect("digest prefix"))
}
