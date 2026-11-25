use super::StepFact;
use aq_playbook_host::model::{
    CallMode, Expr, IterationFailure, JoinMode, Operation, Playbook, RetryPolicy,
};
use std::collections::{BTreeMap, BTreeSet};

fn duration(ms: u64) -> String {
    let (scale, unit) = [(3_600_000, "h"), (60_000, "min"), (1_000, "s")]
        .into_iter()
        .find(|(scale, _)| ms >= *scale && ms.is_multiple_of(*scale))
        .unwrap_or((1, "ms"));
    format!("{} {unit}", ms / scale)
}

/// Static facts for the inspector. Project names, modes and bounds only.
/// Expressions, literal values, captured definitions and run data stay private.
pub fn step_facts(playbook: &Playbook) -> BTreeMap<String, Vec<StepFact>> {
    let facts = playbook
        .steps
        .iter()
        .map(|(id, step)| {
            let facts = facts_for(&step.op);
            (id.clone(), facts)
        })
        .filter(|(_, facts)| !facts.is_empty())
        .collect();
    if serde_json::to_vec(&facts).is_ok_and(|encoded| encoded.len() <= 32768) {
        facts
    } else {
        BTreeMap::new()
    }
}

fn fact(label: &'static str, value: impl ToString) -> StepFact {
    StepFact {
        label,
        value: value.to_string(),
    }
}
fn bindings(label: &'static str, values: impl Iterator<Item = String>) -> StepFact {
    fact(label, values.collect::<Vec<_>>().join(", "))
}
fn decision(predicate: &Expr) -> Vec<StepFact> {
    let operands = match predicate {
        Expr::Eq { left, right } | Expr::Lt { left, right } => vec![&**left, &**right],
        Expr::Exists { value } | Expr::Not { value } => vec![&**value],
        expr => vec![expr],
    };
    operands
        .into_iter()
        .filter_map(|operand| {
            let Expr::Ref { root, path } = operand else {
                return None;
            };
            Some(fact(
                "Checks",
                std::iter::once(root)
                    .chain(path)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("."),
            ))
        })
        .collect()
}
fn invoke(input: &Expr, bind: &str, retry: Option<&RetryPolicy>) -> Vec<StepFact> {
    let mut facts = Vec::new();
    if let Expr::Object { fields } = input {
        facts.push(bindings("Input fields", fields.keys().cloned()));
    }
    facts.push(fact("Stores result as", bind));
    if let Some(retry) = retry {
        facts.push(fact("Maximum attempts", retry.max_attempts));
    }
    facts
}
fn wait(correlation: &BTreeMap<String, Expr>, deadline: u64, bind: &str) -> Vec<StepFact> {
    let mut facts = Vec::new();
    if !correlation.is_empty() {
        facts.push(bindings(
            "Matches event fields",
            correlation.keys().cloned(),
        ));
    }
    facts.extend([
        fact("Deadline", duration(deadline)),
        fact("Stores result as", bind),
    ]);
    facts
}
fn join(mode: &JoinMode, accepted: &BTreeSet<String>, deadline: u64, bind: &str) -> Vec<StepFact> {
    let waits_for = match mode {
        JoinMode::All => "All members to finish",
        JoinMode::AnyCompletion => "Any member to finish",
        JoinMode::AnyAccepted => "Any member with an accepted outcome",
    };
    let mut facts = vec![fact("Waits for", waits_for)];
    if !accepted.is_empty() {
        facts.push(bindings("Accepted outcomes", accepted.iter().cloned()));
    }
    facts.extend([
        fact("Deadline", duration(deadline)),
        fact("Stores result as", bind),
    ]);
    facts
}
fn facts_for(op: &Operation) -> Vec<StepFact> {
    match op {
        Operation::Invoke {
            input, bind, retry, ..
        } => invoke(input, bind, retry.as_ref()),
        Operation::Decide { predicate } => decision(predicate),
        Operation::Wait {
            correlation,
            deadline_ms,
            bind,
            ..
        } => wait(correlation, *deadline_ms, bind),
        Operation::ForEach {
            max_items,
            concurrency,
            on_failure,
            ..
        } => {
            let failure = match on_failure {
                IterationFailure::Stop => "Stop",
                IterationFailure::Continue => "Continue",
            };
            vec![
                fact("Maximum items", max_items),
                fact("Parallel iterations", concurrency),
                fact("On iteration failure", failure),
            ]
        }
        Operation::Call { mode, bind, .. } => {
            let execution = match mode {
                CallMode::Wait => "Wait for the child playbook to finish",
                CallMode::OwnedAsync => "Start a child run owned by this run",
                CallMode::Independent => "Start a separate run",
            };
            vec![fact("Execution", execution), fact("Stores result as", bind)]
        }
        Operation::Join {
            mode,
            accepted_outcomes,
            deadline_ms,
            bind,
            ..
        } => join(mode, accepted_outcomes, *deadline_ms, bind),
        Operation::Return { outcome, .. } => vec![fact("Outcome", outcome)],
    }
}
