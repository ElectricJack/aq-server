//! Bounded, public projections of compiled workflow graphs.
use crate::{Error, Result};
use aq_playbook_host::model::{CallMode, Expr, IterationFailure, JoinMode, Operation, Playbook};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Edge {
    pub label: String,
    pub to: String,
}
#[derive(Clone, Debug, Serialize, Deserialize, JsonSchema, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct StepNode {
    pub id: String,
    /// invoke, decide, wait, for_each, call, join or return.
    pub kind: String,
    /// Command (invoke) or event (wait) contract name, or `dependency/entry`
    /// (call). Never inputs, expressions or values.
    pub target: Option<String>,
    /// Labelled successors in label order.
    pub edges: Vec<Edge>,
    pub on_error: Option<String>,
}
/// Stored public graph of one artifact.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Graph {
    pub entries: BTreeMap<String, String>,
    pub steps: BTreeMap<String, StepNode>,
}

pub fn graph_for_storage(playbook: &Playbook) -> Result<Option<serde_json::Value>> {
    let graph = serde_json::to_value(graph(playbook))?;
    Ok((serde_json::to_vec(&graph)?.len() <= 65536).then_some(graph))
}

pub fn validate_artifact_id(artifact: &str) -> Result<()> {
    if artifact.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    }) {
        Ok(())
    } else {
        Err(Error::Invalid)
    }
}

/// The outer option distinguishes a foreign or unknown artifact from an
/// installed artifact whose graph exceeded the storage budget.
pub async fn registered_graph<'e>(
    db: impl sqlx::PgExecutor<'e>,
    namespace_id: Uuid,
    generation: i64,
    artifact: &str,
) -> Result<Option<Option<Graph>>> {
    let graph: Option<Option<serde_json::Value>> = sqlx::query_scalar("SELECT graph FROM playbook_definitions WHERE namespace_id=$1 AND generation=$2 AND artifact=$3")
        .bind(namespace_id)
        .bind(generation)
        .bind(artifact)
        .fetch_optional(db)
        .await?;
    graph
        .map(|graph| graph.map(serde_json::from_value).transpose())
        .transpose()
        .map_err(|_| Error::Unknown)
}

#[derive(Serialize)]
pub struct StepFact {
    label: &'static str,
    value: String,
}

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
            let mut facts = Vec::new();
            let mut add = |label, value: String| facts.push(StepFact { label, value });
            match &step.op {
                Operation::Invoke {
                    input, bind, retry, ..
                } => {
                    if let Expr::Object { fields } = input {
                        add(
                            "Input fields",
                            fields.keys().cloned().collect::<Vec<_>>().join(", "),
                        );
                    }
                    add("Stores result as", bind.clone());
                    if let Some(retry) = retry {
                        add("Maximum attempts", retry.max_attempts.to_string());
                    }
                }
                Operation::Decide { predicate } => {
                    let operands = match predicate {
                        Expr::Eq { left, right } | Expr::Lt { left, right } => {
                            vec![&**left, &**right]
                        }
                        Expr::Exists { value } | Expr::Not { value } => vec![&**value],
                        expr => vec![expr],
                    };
                    for operand in operands {
                        if let Expr::Ref { root, path } = operand {
                            add(
                                "Checks",
                                std::iter::once(root)
                                    .chain(path)
                                    .cloned()
                                    .collect::<Vec<_>>()
                                    .join("."),
                            );
                        }
                    }
                }
                Operation::Wait {
                    correlation,
                    deadline_ms,
                    bind,
                    ..
                } => {
                    if !correlation.is_empty() {
                        add(
                            "Matches event fields",
                            correlation.keys().cloned().collect::<Vec<_>>().join(", "),
                        );
                    }
                    add("Deadline", duration(*deadline_ms));
                    add("Stores result as", bind.clone());
                }
                Operation::ForEach {
                    max_items,
                    concurrency,
                    on_failure,
                    ..
                } => {
                    add("Maximum items", max_items.to_string());
                    add("Parallel iterations", concurrency.to_string());
                    add(
                        "On iteration failure",
                        match on_failure {
                            IterationFailure::Stop => "Stop",
                            IterationFailure::Continue => "Continue",
                        }
                        .into(),
                    );
                }
                Operation::Call { mode, bind, .. } => {
                    add(
                        "Execution",
                        match mode {
                            CallMode::Wait => "Wait for the child playbook to finish",
                            CallMode::OwnedAsync => "Start a child run owned by this run",
                            CallMode::Independent => "Start a separate run",
                        }
                        .into(),
                    );
                    add("Stores result as", bind.clone());
                }
                Operation::Join {
                    mode,
                    deadline_ms,
                    accepted_outcomes,
                    bind,
                    ..
                } => {
                    add(
                        "Waits for",
                        match mode {
                            JoinMode::All => "All members to finish",
                            JoinMode::AnyCompletion => "Any member to finish",
                            JoinMode::AnyAccepted => "Any member with an accepted outcome",
                        }
                        .into(),
                    );
                    if !accepted_outcomes.is_empty() {
                        add(
                            "Accepted outcomes",
                            accepted_outcomes
                                .iter()
                                .cloned()
                                .collect::<Vec<_>>()
                                .join(", "),
                        );
                    }
                    add("Deadline", duration(*deadline_ms));
                    add("Stores result as", bind.clone());
                }
                Operation::Return { outcome, .. } => add("Outcome", outcome.clone()),
            }
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
/// X32: steps reachable from the run's entry in breadth-first order from
/// `start` (successors by edge label, then `on_error`).
#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq)]
pub struct StepTimeline {
    pub entry: String,
    pub start: String,
    pub steps: Vec<StepNode>,
}
pub fn graph(playbook: &Playbook) -> Graph {
    let steps = playbook
        .steps
        .iter()
        .map(|(id, step)| {
            let mut extra = Vec::new();
            let (kind, target) = match &step.op {
                Operation::Invoke { command, .. } => ("invoke", Some(command.name.clone())),
                Operation::Decide { .. } => ("decide", None),
                Operation::Wait { event, .. } => ("wait", event.as_ref().map(|e| e.name.clone())),
                Operation::ForEach { body, next, .. } => {
                    extra.push(("body", body.clone()));
                    extra.push(("next", next.clone()));
                    ("for_each", None)
                }
                Operation::Call {
                    dependency, entry, ..
                } => ("call", Some(format!("{dependency}/{entry}"))),
                Operation::Join { .. } => ("join", None),
                Operation::Return { .. } => ("return", None),
            };
            let mut edges = step
                .edges
                .iter()
                .map(|(label, to)| Edge {
                    label: label.clone(),
                    to: to.clone(),
                })
                .collect::<Vec<_>>();
            for (label, to) in extra {
                if !step.edges.contains_key(label) {
                    edges.push(Edge {
                        label: label.into(),
                        to,
                    });
                }
            }
            edges.sort_by(|a, b| a.label.cmp(&b.label));
            (
                id.clone(),
                StepNode {
                    id: id.clone(),
                    kind: kind.into(),
                    target,
                    edges,
                    on_error: step.on_error.clone(),
                },
            )
        })
        .collect();
    Graph {
        entries: playbook
            .entries
            .iter()
            .map(|(name, entry)| (name.clone(), entry.start.clone()))
            .collect(),
        steps,
    }
}
pub fn ordered(graph: &Graph, entry: &str) -> Option<StepTimeline> {
    let start = graph.entries.get(entry)?;
    let mut seen = BTreeSet::new();
    let mut queue = VecDeque::from([start.clone()]);
    let mut steps = Vec::new();
    while let Some(id) = queue.pop_front() {
        if !seen.insert(id.clone()) {
            continue;
        }
        let Some(node) = graph.steps.get(&id) else {
            continue;
        };
        queue.extend(
            node.edges
                .iter()
                .map(|e| &e.to)
                .chain(node.on_error.iter())
                .filter(|next| !seen.contains(*next))
                .cloned(),
        );
        steps.push(node.clone());
    }
    Some(StepTimeline {
        entry: entry.into(),
        start: start.clone(),
        steps,
    })
}
