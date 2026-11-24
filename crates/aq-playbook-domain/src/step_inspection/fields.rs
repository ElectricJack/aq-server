use super::{ArtifactContent, BTreeMap, Expr, Graph, Operation, READ, reviewed_command};
use std::collections::BTreeSet;

pub(super) struct Source<'a> {
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

fn result_binding(op: &Operation) -> Option<&str> {
    match op {
        Operation::Invoke { bind, .. }
        | Operation::Wait { bind, .. }
        | Operation::Call { bind, .. }
        | Operation::Join { bind, .. } => Some(bind),
        _ => None,
    }
}

pub(super) fn sources<'a>(
    artifact: &'a ArtifactContent,
    graph: &'a Graph,
) -> BTreeMap<&'a str, Source<'a>> {
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
    for binding in artifact
        .playbook
        .steps
        .values()
        .filter_map(|step| result_binding(&step.op))
    {
        *bindings.entry(binding).or_default() += 1;
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
            Some((bind.as_str(), Source::from_graph(graph, id)?))
        })
        .collect()
}

impl<'a> Source<'a> {
    fn from_graph(graph: &'a Graph, id: &'a str) -> Option<Self> {
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
        Some(Self {
            id,
            observed: reachable(graph, [observed.to.as_str()], id),
            bypassed: reachable(graph, entry_or_alternate, id),
        })
    }
    fn reaches(&self, consumer: &str) -> bool {
        consumer != self.id && self.observed.contains(consumer) && !self.bypassed.contains(consumer)
    }
}

pub(super) struct Field<'a> {
    pub(super) producer: &'a str,
    pub(super) name: &'static str,
    pub(super) path: String,
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
    source.reaches(consumer).then(|| Field {
        producer: source.id,
        name,
        path: format!("{root}.facts.{name}"),
    })
}

pub(super) fn decision_field<'a>(
    sources: &BTreeMap<&str, Source<'a>>,
    id: &str,
    predicate: &Expr,
    skips_on_match: bool,
) -> Option<(Field<'a>, String, &'static str, &'static str)> {
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
        ("route", Some("owner")) => {
            let mut description = format!(
                "Checks whether an owner has already chosen the route for this task. It compares {path}, which came from the task lookup, with owner."
            );
            if skips_on_match {
                description.push_str(" This keeps a manually chosen route in place.");
            }
            (
                description,
                "An owner already chose a route for this task.",
                "The route is unset or was chosen by the automatic router.",
            )
        }
        ("route", Some("router")) => (
            format!(
                "Checks whether the automatic router chose the current route by comparing {path} with router."
            ),
            "The automatic router chose the current route.",
            "The route is unset or was chosen by an owner.",
        ),
        ("route", Some("none")) => (
            format!("Checks whether the task has no route by comparing {path} with none."),
            "The task has no route.",
            "An owner or the automatic router already chose a route.",
        ),
        _ => (
            format!("Checks whether {path} matches the configured value."),
            "The values match.",
            "The values differ.",
        ),
    };
    Some((field, summary, yes, no))
}

fn field<'a>(expr: &'a Expr, key: &str) -> Option<&'a Expr> {
    let Expr::Object { fields } = expr else {
        return None;
    };
    fields.get(key)
}

pub(super) fn assessment_fields<'a>(
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

pub(super) fn assessment_summary(input: &Expr, uses: &[Field<'_>]) -> String {
    let task_details = uses.first().is_some_and(|first| {
        uses.iter().all(|field| field.producer == first.producer)
            && uses.iter().map(|field| field.name).collect::<BTreeSet<_>>()
                == BTreeSet::from(["title", "description", "kind"])
    });
    let question = assessment_question(input);
    if task_details {
        format!(
            "{question} The agent uses the task's title, description, and declared kind from the earlier task lookup."
        )
    } else {
        question.into()
    }
}

fn assessment_question(input: &Expr) -> &'static str {
    match field(input, "fields") {
        Some(Expr::Literal { value }) => {
            let labels: BTreeSet<_> = value
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|item| item["id"].as_str())
                .collect();
            if labels.contains("kind") && labels.contains("risk") {
                "Asks an agent to classify the kind of work and assess its risk."
            } else {
                "Asks an agent to make the assessment configured for this step."
            }
        }
        _ => "Asks an agent to evaluate the supplied context.",
    }
}
