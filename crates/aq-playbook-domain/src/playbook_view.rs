//! Playbooks view values (G11 X30-X33): definition name and version, the
//! pinned-versus-active comparison, the ordered public step graph and the
//! pending human decision. Pure; the reads are in aq-server.
use crate::{Error, Namespace, Result};
use aq_playbook_host::model::{CallMode, Expr, IterationFailure, JoinMode, Operation, Playbook};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use uuid::Uuid;

/// X30/X31. `version` is AQ's per-name install ordinal ("1", "2", ...): the
/// engine has no definition version.
#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq)]
pub struct DefinitionView {
    pub name: String,
    pub version: String,
    /// Whether a current activation of this definition name still selects the
    /// run's pinned artifact. Null when the current catalog could not be read
    /// in this observation (never guessed).
    pub pinned_is_active: Option<bool>,
    /// The activation new runs of this definition use: the pinned one when it
    /// is still active, otherwise the highest registered version.
    pub active: Option<ActiveDefinitionView>,
}
#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq)]
pub struct ActiveDefinitionView {
    pub binding: String,
    pub artifact: String,
    pub version: String,
    pub activation_revision: String,
}
/// X33: the run's pending effect is a human `decision.request`.
#[derive(Clone, Debug, Serialize, JsonSchema, PartialEq)]
pub struct WaitingView {
    /// Always `decision` in this version.
    pub kind: String,
    pub intent: String,
    pub decision_id: Uuid,
    /// `pending`, `answered`, `expired` or `cancelled`; null if unrecorded.
    pub decision_state: Option<String>,
    pub related_task_ids: Vec<Uuid>,
}
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
    namespace: &Namespace,
    artifact: &str,
) -> Result<Option<Option<Graph>>> {
    let graph: Option<Option<serde_json::Value>> = sqlx::query_scalar("SELECT graph FROM playbook_definitions WHERE namespace_id=$1 AND generation=$2 AND artifact=$3")
        .bind(namespace.namespace_id)
        .bind(namespace.generation)
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
/// The catalog fields needed to compare a run pin to the active version.
pub struct ActivationPin<'a> {
    pub binding: &'a str,
    pub artifact: &'a str,
    pub revision: &'a str,
}

pub fn definition_view<'a>(
    artifact: &str,
    binding: Option<&str>,
    name: &str,
    version: i64,
    activations: Option<impl Iterator<Item = ActivationPin<'a>>>,
    registry: &BTreeMap<String, (String, i64)>,
) -> DefinitionView {
    let known = activations.is_some();
    let active = activations
        .into_iter()
        .flatten()
        .filter_map(|a| {
            registry
                .get(a.artifact)
                .filter(|(n, _)| n == name)
                .map(|(_, v)| (a, *v))
        })
        .max_by(|(a, av), (b, bv)| {
            let own = |x: &ActivationPin<'_>| binding == Some(x.binding);
            (own(a), a.artifact == artifact, *av, b.binding).cmp(&(
                own(b),
                b.artifact == artifact,
                *bv,
                a.binding,
            ))
        })
        .map(|(a, v)| ActiveDefinitionView {
            binding: a.binding.into(),
            artifact: a.artifact.into(),
            version: v.to_string(),
            activation_revision: a.revision.into(),
        });
    DefinitionView {
        name: name.into(),
        version: version.to_string(),
        pinned_is_active: known.then(|| active.as_ref().is_some_and(|a| a.artifact == artifact)),
        active,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definition_view_compares_the_run_pin_with_current_deployments() {
        let registry = BTreeMap::from([
            ("a1".into(), ("recovery".into(), 1)),
            ("a2".into(), ("recovery".into(), 2)),
            ("x".into(), ("other".into(), 7)),
        ]);
        let active = || ActivationPin {
            binding: "main",
            artifact: "a2",
            revision: "3",
        };
        let other = ActivationPin {
            binding: "other",
            artifact: "x",
            revision: "3",
        };
        let superseded = definition_view(
            "a1",
            None,
            "recovery",
            1,
            Some([active(), other].into_iter()),
            &registry,
        );
        assert_eq!(superseded.version, "1");
        assert_eq!(superseded.pinned_is_active, Some(false));
        let deployed = superseded.active.unwrap();
        assert_eq!(
            (deployed.artifact.as_str(), deployed.version.as_str()),
            ("a2", "2")
        );
        let current = definition_view(
            "a2",
            None,
            "recovery",
            2,
            Some(std::iter::once(active())),
            &registry,
        );
        assert_eq!(current.pinned_is_active, Some(true));
        let retired = definition_view(
            "a1",
            None,
            "recovery",
            1,
            Some(std::iter::empty()),
            &registry,
        );
        assert_eq!(
            (retired.pinned_is_active, retired.active),
            (Some(false), None)
        );
        let unknown = definition_view(
            "a1",
            None,
            "recovery",
            1,
            None::<std::iter::Empty<ActivationPin<'_>>>,
            &registry,
        );
        assert_eq!((unknown.pinned_is_active, unknown.active), (None, None));
    }

    fn node(id: &str, kind: &str, edges: &[(&str, &str)], on_error: Option<&str>) -> StepNode {
        StepNode {
            id: id.into(),
            kind: kind.into(),
            target: None,
            edges: edges
                .iter()
                .map(|(l, t)| Edge {
                    label: (*l).into(),
                    to: (*t).into(),
                })
                .collect(),
            on_error: on_error.map(Into::into),
        }
    }
    #[test]
    fn step_timeline_is_breadth_first_from_the_entry_and_skips_unreachable_steps() {
        let graph = Graph {
            entries: BTreeMap::from([
                ("main".into(), "plan".into()),
                ("other".into(), "orphan".into()),
            ]),
            steps: [
                node(
                    "plan",
                    "invoke",
                    &[("approved", "ship"), ("rejected", "stop")],
                    Some("recover"),
                ),
                node("ship", "invoke", &[("ok", "stop")], None),
                node("stop", "return", &[], None),
                node("recover", "wait", &[("done", "plan")], None),
                node("orphan", "return", &[], None),
            ]
            .into_iter()
            .map(|n| (n.id.clone(), n))
            .collect(),
        };
        let timeline = ordered(&graph, "main").unwrap();
        assert_eq!(timeline.start, "plan");
        assert_eq!(
            timeline
                .steps
                .iter()
                .map(|s| s.id.as_str())
                .collect::<Vec<_>>(),
            ["plan", "ship", "stop", "recover"]
        );
        assert!(ordered(&graph, "missing").is_none());
    }
}
