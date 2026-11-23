//! Map retained bridge outcomes into their engine dispatch and recovery status.
use crate::{Error, Result, host_values::HostResult, intent_store::MessageResult};
use aq_playbook_host::{
    plugins::command_plugin,
    runner::{DispatchResult, Intent},
};
use serde_json::Value;
#[derive(Debug, Clone)]
pub enum DecisionResult {
    Accepted { handle: String },
    Completed { outcome: String, output: Value },
    Cancelled,
}
pub fn host_error(error: aq_playbook_host::Error) -> Error {
    match error {
        aq_playbook_host::Error::Overloaded => Error::Overloaded,
        _ => Error::Unknown,
    }
}
/// An engine answer to a new event or admission: an unknown outcome or a
/// storage failure is `Unknown` (left pending, delivered again); a refusal
/// is `Invalid` (held).
pub fn delivery_error(error: aq_playbook_host::Error) -> Error {
    match error {
        aq_playbook_host::Error::Overloaded => Error::Overloaded,
        aq_playbook_host::Error::Unknown | aq_playbook_host::Error::Stopped => Error::Unknown,
        _ => Error::Invalid,
    }
}
/// What a quantum deferred. Capacity defers new work without preventing
/// retained work from reaching its drain and tick; any other failure of one
/// stage is kept, one reason per stage, while the rest of the quantum runs.
#[derive(Default)]
pub struct Partial {
    pub overloaded: bool,
    pub reasons: Vec<String>,
}
impl Partial {
    pub fn reason<T>(&mut self, result: &Result<T>) -> Option<String> {
        let failure = result
            .as_ref()
            .err()
            .filter(|error| **error != Error::Overloaded);
        if self.reasons.is_empty()
            && let Some(error) = failure
        {
            self.reasons.push(format!("quantum failed: {error}"));
        }
        let reasons = self.reasons.join("; ");
        (!reasons.is_empty()).then_some(reasons)
    }
    pub fn defer<T>(&mut self, what: &str, result: Result<T>) {
        match result {
            Ok(_) => {}
            Err(Error::Overloaded) => self.overloaded = true,
            Err(error) => self.reasons.push(format!("{what}: {error}")),
        }
    }
}
/// An asynchronous host command's `accepted` outcome is its durable handle.
pub fn host_result(result: HostResult, intent: &Intent) -> DispatchResult {
    let acknowledged =
        command_plugin(&intent.command.name).is_none() && result.outcome == "accepted";
    if let Some(handle) = result.output["handle"].as_str().filter(|_| acknowledged) {
        return DispatchResult::Accepted {
            handle: handle.to_string(),
        };
    }
    DispatchResult::Completed {
        outcome: result.outcome,
        output: result.output,
    }
}
pub fn decision_dispatch(result: Result<DecisionResult>) -> DispatchResult {
    match result {
        Ok(DecisionResult::Accepted { handle }) => DispatchResult::Accepted { handle },
        Ok(DecisionResult::Completed { outcome, output }) => {
            DispatchResult::Completed { outcome, output }
        }
        Ok(DecisionResult::Cancelled) => DispatchResult::Rejected {
            reason: "cancelled before question admission".into(),
        },
        Err(Error::Unknown) => DispatchResult::Unknown {
            reason: "decision owner result unknown; retained lookup required".into(),
        },
        Err(e) => DispatchResult::Rejected {
            reason: e.to_string(),
        },
    }
}

pub fn message_result(result: MessageResult) -> DispatchResult {
    match result {
        MessageResult::Queued(receipt) => DispatchResult::Completed {
            outcome: "queued".into(),
            output: receipt,
        },
        MessageResult::Cancelled => DispatchResult::Rejected {
            reason: "cancelled before mailbox admission".into(),
        },
    }
}
pub fn message_dispatch(result: Result<MessageResult>) -> DispatchResult {
    match result {
        Ok(result) => message_result(result),
        Err(Error::Unknown) => DispatchResult::Unknown {
            reason: "mailbox admission outcome unknown; retained lookup required".into(),
        },
        Err(error) => DispatchResult::Rejected {
            reason: error.to_string(),
        },
    }
}

pub fn retained_dispatch<T>(
    retained: Result<Option<T>>,
    completed: impl FnOnce(T) -> DispatchResult,
    missing: &str,
) -> DispatchResult {
    match retained {
        Ok(Some(result)) => completed(result),
        _ => DispatchResult::Unknown {
            reason: missing.into(),
        },
    }
}
