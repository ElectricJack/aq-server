//! Resume owner cancellation/deletion cleanup after outages and actual stops.
//! Graph task state is terminal immediately; process cleanup never gates it.
use super::*;
use crate::profiled_execution::{cancellation, disposition};

#[derive(Default)]
pub struct Sweep(Option<Uuid>);
impl Sweep {
    pub async fn tick(&mut self, state: &AppState) -> Result<()> {
        let rows = aq_machine_control::owner_retry::due_terminations(&state.store.pool, self.0)
            .await
            .map_err(|_| unavailable())?;
        if rows.is_empty() {
            self.0 = None;
        }
        for (attempt, task, operation, actor, workspace) in rows {
            self.0 = Some(attempt);
            if cleanup(state, attempt, task, operation, actor, workspace)
                .await
                .is_err()
            {
                tracing::warn!(%attempt, "task stop cleanup will retry");
            }
        }
        Ok(())
    }
}
async fn cleanup(
    state: &AppState,
    attempt: Uuid,
    task: Uuid,
    operation: Uuid,
    actor: Uuid,
    workspace: Uuid,
) -> Result<()> {
    let Some(user) = state.store.find_user(actor).await? else {
        return Ok(());
    };
    crate::dashboard_api::actions::existing::owner(state, &user)
        .await
        .map_err(|_| unavailable())?;
    let path = format!("/v1/workspaces/{workspace}/nodes/{task}");
    let raw = runtime(state)?.human_get(&path, &[], 1_000_000).await?;
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
    let request = disposition::Request {
        operation_id: id,
        reason: "Owner canceled or deleted task".into(),
    };
    let _ = disposition::abandon(state, &user, attempt, &request).await;
    Ok(())
}
fn termination_id(operation: Uuid, attempt: Uuid) -> Uuid {
    use sha2::{Digest, Sha256};
    let bytes = Sha256::digest([operation.as_bytes().as_slice(), attempt.as_bytes()].concat());
    Uuid::from_bytes(bytes[..16].try_into().expect("digest prefix"))
}
