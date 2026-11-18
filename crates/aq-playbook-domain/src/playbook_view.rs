//! Playbooks view values (G11 X30-X33): definition name and version, the
//! pinned-versus-active comparison, the ordered public step graph and the
//! pending human decision. Pure; the reads are in aq-server.
use aq_playbook_host::model::{Operation, Playbook};
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
#[cfg(test)]
mod tests {
    use super::*;
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
