//! Explanations of the verified, pinned artifact. Only reviewed static
//! contract vocabulary and structural relationships reach this projection.
use crate::{
    playbook_view::{Graph, StepNode},
    review_domain::CatalogProfile,
};
use aq_playbook_host::model::{ArtifactContent, Expr, Operation};
use serde::Serialize;
use std::{collections::BTreeMap, sync::OnceLock};

const MAX_INSPECTION_BYTES: usize = 24 * 1024;
const READ: &str = "work.read";
const ASSESS: &str = "judgment.assess";
const COMMAND_ERROR: &str = "The step could not proceed, for example because its input was invalid or the runtime rejected the command.";

#[derive(Default, Serialize)]
pub(super) struct StepInspection {
    #[serde(skip_serializing_if = "Option::is_none")]
    summary: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    uses: Vec<FieldUse>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    branches: Vec<Branch>,
}

#[derive(Serialize)]
struct Branch {
    label: String,
    to: String,
    error: bool,
    reason: &'static str,
}

fn branches(
    step: &StepNode,
    outcomes: &[(&str, &'static str)],
    on_error: Option<&'static str>,
) -> Vec<Branch> {
    let mut described: Vec<_> = step
        .edges
        .iter()
        .filter_map(|edge| {
            let (_, reason) = outcomes.iter().find(|(label, _)| *label == edge.label)?;
            Some(Branch {
                label: edge.label.clone(),
                to: edge.to.clone(),
                error: false,
                reason,
            })
        })
        .collect();
    if let Some((to, reason)) = step.on_error.as_ref().zip(on_error) {
        described.push(Branch {
            label: "On error".into(),
            to: to.clone(),
            error: true,
            reason,
        });
    }
    described
}

#[derive(Clone, Serialize)]
struct FieldUse {
    producer: String,
    consumer: String,
    paths: Vec<String>,
    purpose: &'static str,
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

mod fields;
use fields::{Field, assessment_fields, assessment_summary, decision_field, sources};

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

impl FieldUse {
    fn supplies_assessment(&self, id: &str) -> bool {
        self.producer == id
            && self.purpose == "Assessment context"
            && ["title", "description", "kind"].into_iter().all(|field| {
                self.paths.iter().any(|path| {
                    path.rsplit_once(".facts.")
                        .is_some_and(|(_, name)| name == field)
                })
            })
    }
}
impl StepInspection {
    fn explain_assessment(&mut self, id: &str) {
        if self.uses.iter().any(|usage| usage.supplies_assessment(id))
            && let Some(summary) = &mut self.summary
        {
            summary.push_str("\n\nIf the workflow requests an assessment, the agent uses those details to understand the work.");
        }
    }
}

fn skips_when_true(artifact: &ArtifactContent, step: &aq_playbook_host::model::Step) -> bool {
    let target = step
        .edges
        .get("true")
        .and_then(|next| artifact.playbook.steps.get(next));
    target.is_some_and(
        |step| matches!(&step.op, Operation::Return { outcome, .. } if outcome == "skipped"),
    )
}

pub(super) fn project(
    artifact: &ArtifactContent,
    graph: &Graph,
) -> BTreeMap<String, StepInspection> {
    let sources = sources(artifact, graph);
    let mut inspection = BTreeMap::<String, StepInspection>::new();
    for (id, step) in &artifact.playbook.steps {
        let node = &graph.steps[id];
        let (summary, described) = match (reviewed_command(artifact, &step.op), &step.op) {
            (Some(READ), Operation::Invoke { .. }) => (
                "Looks up the task for this run and reads its current title, description, declared kind, and who set any existing route.".into(),
                branches(node, &[
                    ("observed", "The task was found and its details are available."),
                    ("refused", "The task is unknown or its project does not use automatic routing."),
                ], Some(COMMAND_ERROR)),
            ),
            (Some(ASSESS), Operation::Invoke { input, .. }) => {
                let usages = assessment_fields(&sources, id, input);
                let summary = assessment_summary(input, &usages);
                record_uses(&mut inspection, id, usages, "Assessment context");
                (summary, branches(node, &[
                    ("accepted", "The host queued the assessment and returned a handle."),
                    ("assessed", "A completed assessment was available."),
                    ("failed", "The host could not get a usable assessment, for example because the provider was unavailable or the deadline passed."),
                    ("refused", "The request did not run, for example because it exceeded the host's budget cap."),
                ], Some(COMMAND_ERROR)))
            }
            (_, Operation::Decide { predicate }) => {
                let skips_on_match = skips_when_true(artifact, step);
                let Some((field, summary, yes, no)) = decision_field(&sources, id, predicate, skips_on_match) else { continue };
                let purpose = if field.name == "route" { "Route check" } else { "Decision input" };
                record_uses(&mut inspection, id, [field], purpose);
                (summary, branches(node, &[
                    ("true", yes), ("false", no), ("unknown", "The condition could not be evaluated."),
                ], None))
            }
            _ => continue,
        };
        let detail = inspection.entry(id.clone()).or_default();
        detail.summary = Some(summary);
        detail.branches = described;
    }
    for (id, detail) in &mut inspection {
        detail.explain_assessment(id);
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
            "../../../vendor/aq-playbooks-8901af1/crates/aq-playbooks-compiler/authoring-examples/escalate-on-judgment/previous.artifact.json"
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
        context["fields"]["value"]
            .as_array_mut()
            .unwrap()
            .push(json!({"id": "kind"}));
        context["question"]["value"] = json!("PRIVATE_PROMPT");

        let result = details(raw);
        let view = &result["step_inspection"];
        assert_eq!(
            view["owned"]["summary"],
            "Checks whether an owner has already chosen the route for this task. It compares task.facts.route, which came from the task lookup, with owner. This keeps a manually chosen route in place."
        );
        assert_eq!(
            view["read"]["summary"],
            "Looks up the task for this run and reads its current title, description, declared kind, and who set any existing route.\n\nIf the workflow requests an assessment, the agent uses those details to understand the work."
        );
        let read_branches = view["read"]["branches"].as_array().unwrap();
        assert!(read_branches.contains(&json!({"label":"refused","to":"skipped","error":false,"reason":"The task is unknown or its project does not use automatic routing."})));
        assert!(read_branches.contains(
            &json!({"label":"On error","to":"failed","error":true,"reason":COMMAND_ERROR})
        ));
        assert!(view["owned"]["branches"].as_array().unwrap().contains(&json!({"label":"false","to":"triage","error":false,"reason":"The route is unset or was chosen by the automatic router."})));
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
            "Asks an agent to classify the kind of work and assess its risk. The agent uses the task's title, description, and declared kind from the earlier task lookup."
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
            "Checks whether the automatic router chose the current route by comparing task.facts.route with router."
        );
        assert!(changed["step_inspection"]["owned"]["branches"].as_array().unwrap().contains(&json!({"label":"true","to":"skipped","error":false,"reason":"The automatic router chose the current route."})));
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
