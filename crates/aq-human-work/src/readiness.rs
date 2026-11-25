use super::Hold;
use serde_json::{Value, json};

/// Graph relationships and completeness observed for the task.
pub struct TaskDependencies {
    pub blockers: Vec<Value>,
    pub hierarchy: Vec<Value>,
    pub complete: bool,
}
impl TaskDependencies {
    pub fn new(blockers: Vec<Value>, complete: bool) -> Self {
        Self {
            blockers,
            complete,
            hierarchy: Vec::new(),
        }
    }
}
/// Work-view readiness. The state is the first match of: the task's own
/// `draft`, `completed` or `canceled` state; `running` while the holding
/// attempt executes; `held` when it has stopped but keeps the task claim, or
/// an active task has a server-side hold ([`crate::task_hold`]); `waiting` for an
/// unsatisfied blocker or a hierarchy relationship ([`crate::hierarchy`]) that keeps
/// the task out of the runnable queue; `unknown` for a truncated Context;
/// otherwise `ready`. `reasons` explains a stopped attempt's hold in any
/// state, then the task's hold.
pub fn readiness(
    task_state: Option<&str>,
    hold: Option<&Hold>,
    held: impl IntoIterator<Item = Value>,
    dependencies: TaskDependencies,
) -> Value {
    let mut dependencies = dependencies;
    let mut held: Vec<_> = held.into_iter().collect();
    if task_state != Some("active") {
        held.clear();
        dependencies.hierarchy.clear();
    }
    // The original attempt's own retry hold replaces the generic budget hold.
    if hold.is_some_and(|h| {
        held.iter().any(|reason| {
            reason["code"] == "owner_retry_pending" && reason["attempt_id"] == json!(h.attempt_id)
        })
    }) {
        held.retain(|reason| reason["wake_condition"] != "owner_retry_authorized");
    }
    if held
        .iter()
        .any(|reason| reason["wake_condition"] == "owner_retry_authorized")
    {
        held.retain(|reason| reason["code"] != "replacement_facts_ready");
    }
    let closed_launch = held.iter().any(|reason| {
        matches!(
            reason["code"].as_str(),
            Some("closure_does_not_prove_nonstart" | "closed_launch_settlement_pending")
        )
    });
    let running = !closed_launch && hold.is_some_and(Hold::running);
    let state = dependencies.state(task_state, running, hold.is_some() || !held.is_empty());
    let TaskDependencies {
        blockers,
        hierarchy,
        complete,
    } = dependencies;
    let reasons: Vec<Value> = hold
        .and_then(Hold::reason)
        .into_iter()
        .chain(held)
        .collect();
    let mut out = json!({"state":state,"blockers":blockers,"complete":complete,"reasons":reasons});
    if !hierarchy.is_empty() {
        out["hierarchy"] = Value::Array(hierarchy);
    }
    out
}

impl TaskDependencies {
    fn state<'a>(&self, task_state: Option<&'a str>, running: bool, held: bool) -> &'a str {
        let waiting =
            !self.hierarchy.is_empty() || self.blockers.iter().any(|b| b["satisfied"] != true);
        match task_state {
            Some(s @ ("completed" | "canceled" | "draft")) => s,
            _ if running => "running",
            _ if held => "held",
            _ if waiting => "waiting",
            _ if !self.complete => "unknown",
            _ => "ready",
        }
    }
}
