//! Central H3 sweep. Each tick steps a bounded page of live profiled sessions
//! through the persisted ladder; the episode row, not this cursor, is
//! authoritative, so shutdown mid-effect resumes from its pending intent.
use super::{
    engine::{self, Target},
    policy::ExecutionPolicy,
    ports::DbPorts,
};
use crate::{AppState, error::AppError};
use serde_json::Value;
use std::time::Duration;
use tokio::sync::watch;
use uuid::Uuid;

const PAGE: i64 = 32;

#[derive(Default)]
pub struct Sweep {
    after: Option<Uuid>,
}

fn target(attempt_id: Uuid, b: &Value) -> Option<Target> {
    let id = |k: &str| b[k].as_str()?.parse().ok();
    Some(Target {
        attempt_id,
        session_id: id("session_id")?,
        task_id: id("task_id")?,
        project_id: id("project_id")?,
        workspace_id: id("workspace_id")?,
        generation: b["generation"].as_u64()?,
    })
}

impl Sweep {
    /// Returns the number of sessions stepped. Sessions that hold their task,
    /// plus any episode still mid-ladder after its task was released.
    pub async fn tick(
        &mut self,
        state: &AppState,
        policy: &ExecutionPolicy,
        now: u64,
    ) -> Result<usize, AppError> {
        let rows: Vec<(Uuid, Value)> = sqlx::query_as(
            "SELECT a.attempt_id,b.binding FROM execution_attempts a \
             JOIN agent_launch_bindings b USING(attempt_id) \
             WHERE a.kind='profiled_v1' \
             AND ((a.holds_task AND NOT b.revoked) OR EXISTS(SELECT 1 FROM stall_episodes e \
                  WHERE e.attempt_id=a.attempt_id AND e.state->>'rung' NOT IN ('observing','replaced','quarantined'))) \
             AND ($1::UUID IS NULL OR a.attempt_id>$1) ORDER BY a.attempt_id LIMIT $2",
        )
        .bind(self.after)
        .bind(PAGE)
        .fetch_all(&state.store.pool)
        .await
        .map_err(|_| AppError::Internal)?;
        if rows.is_empty() {
            self.after = None;
            return Ok(0);
        }
        let ports = DbPorts {
            state: state.clone(),
        };
        let mut stepped = 0;
        for (attempt, binding) in rows {
            self.after = Some(attempt);
            let Some(t) = target(attempt, &binding) else {
                continue;
            };
            match engine::tick(&state.store.pool, &ports, &t, policy, now).await {
                Ok(_) => stepped += 1,
                Err(error) => {
                    tracing::warn!(%attempt, ?error, "stall ladder tick failed; intent stays pending")
                }
            }
        }
        Ok(stepped)
    }
}

pub async fn run(state: AppState, mut stop: watch::Receiver<bool>) {
    if state.config.profiled_execution.is_none() {
        return;
    }
    let policy = ExecutionPolicy::v1();
    let mut sweep = Sweep::default();
    let mut ticks = tokio::time::interval(Duration::from_secs(15));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = stop.wait_for(|value| *value) => return,
            _ = ticks.tick() => {}
        }
        let now = (state.clock)().timestamp_millis().max(0) as u64;
        tokio::select! {
            biased;
            _ = stop.wait_for(|value| *value) => return,
            outcome = tokio::time::timeout(Duration::from_secs(30), sweep.tick(&state, &policy, now)) => {
                if !matches!(outcome, Ok(Ok(_))) {
                    tracing::warn!("stall ladder sweep incomplete");
                }
            }
        }
    }
}
