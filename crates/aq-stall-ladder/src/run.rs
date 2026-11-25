//! The session, policy time and durable action observed during a ladder tick.
use crate::{ladder::Intent, policy::ExecutionPolicy};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
/// The profiled session the ladder watches. Its lineage is the task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub attempt_id: Uuid,
    pub session_id: Uuid,
    pub task_id: Uuid,
    pub project_id: Uuid,
    pub workspace_id: Uuid,
    pub generation: u64,
}

#[derive(Clone, Copy)]
pub struct Tick<'a> {
    pub policy: &'a ExecutionPolicy,
    pub now: u64,
}
impl<'a> Tick<'a> {
    pub fn new(policy: &'a ExecutionPolicy, now: u64) -> Self {
        Self { policy, now }
    }
}
#[derive(Clone, Copy)]
pub struct Action<'a> {
    pub id: Uuid,
    pub intent: &'a Intent,
}
impl<'a> Action<'a> {
    pub fn new(id: Uuid, intent: &'a Intent) -> Self {
        Self { id, intent }
    }
}
pub struct Nudge<'a> {
    pub operation_id: Uuid,
    pub body: &'a str,
    pub defer: bool,
    pub now: u64,
}
