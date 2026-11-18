//! Explanations of the verified, pinned artifact. Only reviewed static
//! contract vocabulary and structural relationships reach this projection.
use crate::{playbook_view::Graph, review_domain::CatalogProfile};
use aq_playbook_host::model::{ArtifactContent, Expr, Operation};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::OnceLock,
};

const MAX_INSPECTION_BYTES: usize = 24 * 1024;
const READ: &str = "work.read";
const ASSESS: &str = "judgment.assess";

#[derive(Default, Serialize)]
pub(super) struct StepInspection {
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    uses: Vec<FieldUse>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    branches: Vec<Branch>,
}

#[derive(Clone, Serialize)]
struct FieldUse {
    producer: String,
    consumer: String,
    paths: Vec<String>,
    purpose: &'static str,
}

#[derive(Serialize)]
struct Branch {
    label: String,
    to: String,
    error: bool,
    reason: &'static str,
}

impl StepInspection {
    fn outcome(&mut self, graph: &Graph, id: &str, label: &str, reason: &'static str) {
        if let Some(edge) = graph.steps[id]
            .edges
            .iter()
            .find(|edge| edge.label == label)
        {
            self.branches.push(Branch {
                label: edge.label.clone(),
                to: edge.to.clone(),
                error: false,
                reason,
            });
        }
    }
}

/// Only use copy reviewed for a command whose full pinned contract matches
/// the reviewed catalog. An equal command name alone does not establish it.
fn reviewed_command<'a>(artifact: &ArtifactContent, op: &'a Operation) -> Option<&'a str> {
    let Operation::Invoke { command, .. } = op else {
        return None;
    };
    if !matches!(command.name.as_str(), READ | ASSESS) {
        return None;
    }
    static FINGERPRINTS: OnceLock<BTreeMap<&str, String>> = OnceLock::new();
    let known = FINGERPRINTS.get_or_init(|| {
        let registry = CatalogProfile::Host9.registry();
        [READ, ASSESS]
            .into_iter()
            .map(|name| {
                let fingerprint = registry.commands[name]
                    .fingerprint()
                    .expect("reviewed contract fingerprints are serializable");
                (name, fingerprint)
            })
            .collect()
    });
    let name = command.name.as_str();
    let pinned = artifact.registry.commands.get(name)?.fingerprint().ok()?;
    (known.get(name) == Some(&command.fingerprint) && pinned == command.fingerprint).then_some(name)
}

struct Source<'a> {
    id: &'a str,
    observed: BTreeSet<&'a str>,
    bypassed: BTreeSet<&'a str>,
}

fn reachable<'a>(
    graph: &'a Graph,
    starts: impl IntoIterator<Item = &'a str>,
    without_observed: &'a str,
) -> BTreeSet<&'a str> {
    let mut seen = BTreeSet::new();
    let mut pending: Vec<_> = starts.into_iter().collect();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        let Some(step) = graph.steps.get(id) else {
            continue;
        };
        pending.extend(step.edges.iter().filter_map(|edge| {
            (id != without_observed || edge.label != "observed").then_some(edge.to.as_str())
        }));
        pending.extend(step.on_error.as_deref());
    }
    seen
}

fn sources<'a>(artifact: &'a ArtifactContent, graph: &'a Graph) -> BTreeMap<&'a str, Source<'a>> {
    // Iteration control has implicit edges and can restore a binding. Leave it
    // to the execution model before attributing reads in those graphs.
    if artifact
        .playbook
        .steps
        .values()
        .any(|step| matches!(step.op, Operation::ForEach { .. }))
    {
        return BTreeMap::new();
    }
    let mut bindings = BTreeMap::<&str, usize>::new();
    for step in artifact.playbook.steps.values() {
        match &step.op {
            Operation::Invoke { bind, .. }
            | Operation::Wait { bind, .. }
            | Operation::Call { bind, .. }
            | Operation::Join { bind, .. } => *bindings.entry(bind).or_default() += 1,
            _ => {}
        }
    }
    artifact
        .playbook
        .steps
        .iter()
        .filter_map(|(id, step)| {
            let Operation::Invoke { bind, .. } = &step.op else {
                return None;
            };
            if bindings[bind.as_str()] != 1 || reviewed_command(artifact, &step.op) != Some(READ) {
                return None;
            }
            let node = &graph.steps[id];
            let observed = node.edges.iter().find(|edge| edge.label == "observed")?;
            // A Refused result can replace a previous output on a repeated
            // visit. Count alternate successors as bypasses too.
            let entry_or_alternate = graph.entries.values().map(String::as_str).chain(
                node.edges
                    .iter()
                    .filter(|edge| edge.label != "observed")
                    .map(|edge| edge.to.as_str())
                    .chain(node.on_error.as_deref()),
            );
            Some((
                bind.as_str(),
                Source {
                    id,
                    observed: reachable(graph, [observed.to.as_str()], id),
                    bypassed: reachable(graph, entry_or_alternate, id),
                },
            ))
        })
        .collect()
}

struct Field<'a> {
    producer: &'a str,
    name: &'static str,
    path: String,
}

fn public_field<'a>(
    sources: &BTreeMap<&str, Source<'a>>,
    consumer: &str,
    expr: &Expr,
) -> Option<Field<'a>> {
    let Expr::Ref { root, path } = expr else {
        return None;
    };
    let [facts, name] = path.as_slice() else {
        return None;
    };
    if facts != "facts" {
        return None;
    }
    // These are static names from the pinned work.read contract. Runtime
    // task facts and other properties are never read or included here.
    let name = ["route", "title", "description", "kind"]
        .into_iter()
        .find(|public| name == public)?;
    let source = sources.get(root.as_str())?;
    (consumer != source.id
        && source.observed.contains(consumer)
        && !source.bypassed.contains(consumer))
    .then(|| Field {
        producer: source.id,
        name,
        path: format!("{root}.facts.{name}"),
    })
}

fn decision_field<'a>(
    sources: &BTreeMap<&str, Source<'a>>,
    id: &str,
    predicate: &Expr,
    graph: &Graph,
    inspection: &mut StepInspection,
) -> Option<Field<'a>> {
    let Expr::Eq { left, right } = predicate else {
        return None;
    };
    let (field, value) = match (&**left, &**right) {
        (Expr::Literal { value }, reference) | (reference, Expr::Literal { value }) => {
            (public_field(sources, id, reference)?, value)
        }
        _ => return None,
    };
    let path = &field.path;
    let (summary, yes, no) = match (field.name, value.as_str()) {
        ("route", Some("owner")) => (
            format!(
                "Checks whether an owner already set the task's route by comparing {path} with owner."
            ),
            "An owner already set the task's route.",
            "The task has no route or its route was set by the router.",
        ),
        ("route", Some("router")) => (
            format!(
                "Checks whether the router already set the task's route by comparing {path} with router."
            ),
            "The router already set the task's route.",
            "The task has no route or its route was set by an owner.",
        ),
        ("route", Some("none")) => (
            format!("Checks whether the task has no route by comparing {path} with none."),
            "The task has no route.",
            "An owner or the router already set the task's route.",
        ),
        _ => (
            format!("Checks whether {path} matches the configured value."),
            "The values match.",
            "The values differ.",
        ),
    };
    inspection.summary = Some(summary);
    inspection.outcome(graph, id, "true", yes);
    inspection.outcome(graph, id, "false", no);
    inspection.outcome(
        graph,
        id,
        "unknown",
        "The condition could not be evaluated.",
    );
    Some(field)
}

fn field<'a>(expr: &'a Expr, key: &str) -> Option<&'a Expr> {
    let Expr::Object { fields } = expr else {
        return None;
    };
    fields.get(key)
}

fn assessment_fields<'a>(
    sources: &BTreeMap<&str, Source<'a>>,
    id: &str,
    input: &Expr,
) -> Vec<Field<'a>> {
    let Some(Expr::Array { items }) =
        field(input, "context").and_then(|context| field(context, "items"))
    else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| public_field(sources, id, field(item, "text")?))
        .collect()
}

fn record_uses<'a>(
    inspection: &mut BTreeMap<String, StepInspection>,
    consumer: &str,
    fields: impl IntoIterator<Item = Field<'a>>,
    purpose: &'static str,
) {
    let mut groups = BTreeMap::<&str, Vec<String>>::new();
    for field in fields {
        let paths = groups.entry(field.producer).or_default();
        if !paths.contains(&field.path) {
            paths.push(field.path);
        }
    }
    for (producer, paths) in groups {
        let usage = FieldUse {
            producer: producer.into(),
            consumer: consumer.into(),
            paths,
            purpose,
        };
        inspection
            .entry(producer.into())
            .or_default()
            .uses
            .push(usage.clone());
        inspection
            .entry(consumer.into())
            .or_default()
            .uses
            .push(usage);
    }
}

pub(super) fn project(
    artifact: &ArtifactContent,
    graph: &Graph,
) -> BTreeMap<String, StepInspection> {
    let sources = sources(artifact, graph);
    let mut inspection = BTreeMap::<String, StepInspection>::new();
    for (id, step) in &artifact.playbook.steps {
        let mut detail = StepInspection::default();
        let mut usages = Vec::new();
        let mut purpose = "Decision input";
        match (reviewed_command(artifact, &step.op), &step.op) {
            (Some(READ), Operation::Invoke { .. }) => {
                detail.summary = Some(
                    "Uses the task ID in subject to read the task's current details and revision."
                        .into(),
                );
                detail.outcome(
                    graph,
                    id,
                    "observed",
                    "The host found the task and returned its details.",
                );
                detail.outcome(
                    graph,
                    id,
                    "refused",
                    "The task is unknown or its project does not use automatic routing.",
                );
                if let Some(to) = &graph.steps[id].on_error {
                    detail.branches.push(Branch {
                        label: "On error".into(),
                        to: to.clone(),
                        error: true,
                        reason: "The step could not proceed, for example because its input was invalid or the runtime rejected the command.",
                    });
                }
            }
            (Some(ASSESS), Operation::Invoke { input, .. }) => {
                usages = assessment_fields(&sources, id, input);
                purpose = "Assessment context";
                let names: BTreeSet<_> = usages.iter().map(|field| field.name).collect();
                detail.summary = Some(
                    if names == BTreeSet::from(["title", "description", "kind"]) {
                        "Requests a model assessment using the task's title, description, and kind."
                    } else {
                        "Requests a model assessment using the context supplied to this step."
                    }
                    .into(),
                );
                detail.outcome(
                    graph,
                    id,
                    "accepted",
                    "The host queued the assessment and returned a handle.",
                );
                detail.outcome(
                    graph,
                    id,
                    "assessed",
                    "A completed assessment was available.",
                );
                detail.outcome(graph, id, "failed", "The host could not get a usable assessment, for example because the provider was unavailable or the deadline passed.");
                detail.outcome(graph, id, "refused", "The request did not run, for example because it exceeded the host's budget cap.");
            }
            (_, Operation::Decide { predicate }) => {
                if let Some(field) = decision_field(&sources, id, predicate, graph, &mut detail) {
                    if field.name == "route" {
                        purpose = "Route check";
                    }
                    usages.push(field);
                }
            }
            _ => {}
        }
        if detail.summary.is_some() || !detail.branches.is_empty() {
            let stored = inspection.entry(id.clone()).or_default();
            stored.summary = detail.summary;
            stored.branches = detail.branches;
        }
        record_uses(&mut inspection, id, usages, purpose);
    }
    if serde_json::to_vec(&inspection).is_ok_and(|encoded| encoded.len() <= MAX_INSPECTION_BYTES) {
        inspection
    } else {
        BTreeMap::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playbook_view::{DefinitionDetails, graph};
    use aq_playbook_host::model::Artifact;
    use serde_json::{Value, json};

    fn authored_example() -> Value {
        serde_json::from_str(include_str!(
            "../../../vendor/aq-playbooks-1196671/crates/aq-playbooks-compiler/authoring-examples/escalate-on-judgment/previous.artifact.json"
        )).unwrap()
    }

    fn details(raw: Value) -> Value {
        let content = serde_json::from_value(raw["content"].clone()).unwrap();
        let artifact = Artifact::seal(content).unwrap();
        let graph = graph(&artifact.content.playbook);
        serde_json::to_value(DefinitionDetails::new(Some(&graph), Some(&artifact), None)).unwrap()
    }

    #[test]
    fn an_exact_routing_artifact_explains_the_read_condition_and_context() {
        let mut raw = authored_example();
        let context = &mut raw["content"]["playbook"]["steps"]["ask"]["op"]["input"]["fields"];
        let items = context["context"]["fields"]["items"]["items"]
            .as_array_mut()
            .unwrap();
        items.push(json!({"op": "object", "fields": {
            "label": {"op": "literal", "value": "PRIVATE_CONTEXT_LABEL"},
            "text": {"op": "ref", "root": "task", "path": ["facts", "kind"]}
        }}));
        context["question"]["value"] = json!("PRIVATE_PROMPT");

        let result = details(raw);
        let view = &result["step_inspection"];
        assert_eq!(
            view["owned"]["summary"],
            "Checks whether an owner already set the task's route by comparing task.facts.route with owner."
        );
        assert!(view["owned"]["branches"].as_array().unwrap().iter().any(|edge| edge == &json!({"label":"false","to":"triage","error":false,"reason":"The task has no route or its route was set by the router."})));
        assert!(
            view["read"]["branches"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| edge["label"] == "refused"
                    && edge["to"] == "skipped"
                    && edge["reason"]
                        .as_str()
                        .unwrap()
                        .contains("automatic routing"))
        );
        assert!(
            view["read"]["branches"]
                .as_array()
                .unwrap()
                .iter()
                .any(|edge| edge["label"] == "On error"
                    && edge["to"] == "failed"
                    && edge["error"] == true)
        );
        let use_in_ask = json!({"producer":"read","consumer":"ask","paths":["task.facts.title","task.facts.description","task.facts.kind"],"purpose":"Assessment context"});
        assert_eq!(view["ask"]["uses"][0], use_in_ask);
        assert!(
            view["read"]["uses"]
                .as_array()
                .unwrap()
                .contains(&use_in_ask)
        );
        assert_eq!(
            view["ask"]["summary"],
            "Requests a model assessment using the task's title, description, and kind."
        );
        assert!(!result.to_string().contains("PRIVATE_"));
    }

    #[test]
    fn a_changed_predicate_never_reuses_old_copy_or_exposes_an_arbitrary_literal() {
        let mut raw = authored_example();
        raw["content"]["playbook"]["steps"]["owned"]["op"]["predicate"]["right"]["value"] =
            json!("router");
        let changed = details(raw.clone());
        assert_eq!(
            changed["step_inspection"]["owned"]["summary"],
            "Checks whether the router already set the task's route by comparing task.facts.route with router."
        );
        raw["content"]["playbook"]["steps"]["owned"]["op"]["predicate"]["right"]["value"] =
            json!("PRIVATE_COMPARISON");
        let changed = details(raw);
        assert_eq!(
            changed["step_inspection"]["owned"]["summary"],
            "Checks whether task.facts.route matches the configured value."
        );
        assert!(!changed.to_string().contains("PRIVATE_COMPARISON"));
    }

    #[test]
    fn ambiguous_producers_and_old_pins_fall_back_without_inventing_a_link() {
        let mut raw = authored_example();
        let mut alternate = raw["content"]["playbook"]["entries"]["main"].clone();
        alternate["start"] = json!("ask");
        raw["content"]["playbook"]["entries"]["alternate"] = alternate;
        let result = details(raw.clone());
        assert!(result["step_inspection"]["ask"].get("uses").is_none());
        assert!(result["step_inspection"]["owned"]["uses"].is_array());

        let content: ArtifactContent = serde_json::from_value(raw["content"].clone()).unwrap();
        let artifact = Artifact::seal(content).unwrap();
        let mut graph = graph(&artifact.content.playbook);
        let result =
            serde_json::to_value(DefinitionDetails::new(Some(&graph), None, None)).unwrap();
        assert_eq!(result, json!({}));

        graph.steps.get_mut("read").unwrap().on_error = None;
        let result =
            serde_json::to_value(DefinitionDetails::new(Some(&graph), Some(&artifact), None))
                .unwrap();
        assert!(result.get("step_facts").is_some());
        assert!(result.get("step_inspection").is_none());
    }
}
