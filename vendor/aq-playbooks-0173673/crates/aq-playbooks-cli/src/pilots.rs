//! Reusable, compiled pilot workflows with deterministic fake-host scenarios.
use crate::{
    catalog::*,
    simulation::{Action, Scenario, Start},
};
use aq_playbooks_compiler::{COMPILER_VERSION, CompileInput, Proposal, compile};
use aq_playbooks_model::*;
use aq_playbooks_runner::{Authority, DispatchResult};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
pub const NAMES: &[&str] = &["coding", "routing", "watcher", "report"];
fn lit(v: Value) -> Expr {
    Expr::literal(v)
}
fn reference(root: &str, path: &[&str]) -> Expr {
    Expr::Ref {
        root: root.into(),
        path: path.iter().map(|s| (*s).into()).collect(),
    }
}
fn fields(items: &[(&str, Expr)]) -> Expr {
    Expr::Object {
        fields: items.iter().cloned().map(|(k, v)| (k.into(), v)).collect(),
    }
}
fn edge(op: Operation, edges: &[(&str, &str)]) -> Step {
    Step {
        op,
        edges: edges
            .iter()
            .map(|(a, b)| ((*a).into(), (*b).into()))
            .collect(),
        on_error: Some("failure".into()),
    }
}
fn ret(outcome: &str, output: Expr) -> Step {
    Step {
        op: Operation::Return {
            outcome: outcome.into(),
            output,
        },
        edges: BTreeMap::new(),
        on_error: None,
    }
}
fn invoke(
    registry: &Registry,
    name: &str,
    input: Expr,
    bind: &str,
    success: &[(&str, &str)],
) -> Step {
    let c = &registry.commands[name];
    let mut s = edge(
        Operation::Invoke {
            command: ContractRef {
                name: name.into(),
                fingerprint: c.fingerprint().unwrap(),
            },
            input,
            bind: bind.into(),
            retry: None,
        },
        &[],
    );
    for outcome in c.outcomes.keys() {
        s.edges.insert(outcome.clone(), "failure".into());
    }
    for (a, b) in success {
        s.edges.insert((*a).into(), (*b).into());
    }
    s
}
fn call(name: &str, input: Expr, mode: CallMode, bind: &str, edges: &[(&str, &str)]) -> Step {
    edge(
        Operation::Call {
            dependency: name.into(),
            entry: "main".into(),
            input,
            mode,
            bind: bind.into(),
        },
        edges,
    )
}
fn build(
    id: &str,
    registry: Registry,
    input: Schema,
    output: Schema,
    start: &str,
    mut steps: BTreeMap<String, Step>,
    children: &[(&str, &Artifact)],
) -> anyhow::Result<Artifact> {
    let mut registry = registry;
    let mut commands = BTreeSet::new();
    let mut events = BTreeSet::new();
    for step in steps.values() {
        match &step.op {
            Operation::Invoke { command, .. } => {
                commands.insert(command.name.clone());
            }
            Operation::Wait {
                event: Some(event), ..
            } => {
                events.insert(event.name.clone());
            }
            _ => {}
        }
    }
    for name in commands.clone() {
        if let Some(asynchronous) = &registry.commands[&name].asynchronous {
            commands.insert(asynchronous.reconciliation_command.clone());
            events.insert(asynchronous.completion_event.clone());
        }
    }
    registry.commands.retain(|name, _| commands.contains(name));
    registry.events.retain(|name, _| events.contains(name));
    steps
        .entry("failure".into())
        .or_insert_with(|| ret("escalated", lit(Value::Null)));
    let caps = registry
        .commands
        .values()
        .flat_map(|c| c.capabilities.clone())
        .chain(
            children
                .iter()
                .flat_map(|(_, c)| c.content.playbook.required_capabilities.clone()),
        )
        .collect();
    let playbook = Playbook {
        id: id.into(),
        purpose: format!("Synthetic {id} pilot"),
        scope: "project".into(),
        entries: BTreeMap::from([(
            "main".into(),
            EntryPoint {
                input,
                outcomes: BTreeMap::from([
                    ("success".into(), output),
                    ("escalated".into(), Schema::Null),
                ]),
                start: start.into(),
            },
        )]),
        steps,
        triggers: vec![],
        limits: Limits::default(),
        required_capabilities: caps,
    };
    let header = SourceHeader {
        format_version: 1,
        id: id.into(),
        scope: "project".into(),
        interfaces: strings(&["main"]),
        definitions: BTreeSet::new(),
    };
    let dependencies = children
        .iter()
        .map(|(name, a)| {
            (
                (*name).into(),
                CallableDependency {
                    selection: DependencySelection::Exact,
                    binding: (*name).into(),
                    artifact: a.digest.clone(),
                    interfaces: a.content.playbook.entries.clone(),
                    capabilities: a.content.playbook.required_capabilities.clone(),
                    scope: "project".into(),
                    dependencies: a
                        .content
                        .dependencies
                        .values()
                        .flat_map(|d| {
                            std::iter::once(d.artifact.clone()).chain(d.dependencies.clone())
                        })
                        .collect(),
                },
            )
        })
        .collect();
    let input = CompileInput {
        source: format!(
            "---aq-playbook\n{}\n---\n\n# {id}\n\nA synthetic bounded pilot. Domain services retain authority.\n",
            serde_json::to_string(&header)?
        ),
        proposal: Proposal {
            playbook,
            source_map: BTreeMap::new(),
            questions: vec![],
            unsupported_features: vec![],
        },
        registry,
        dependencies,
        definitions: BTreeMap::new(),
        compiler: COMPILER_VERSION.into(),
    };
    compile(&input)
        .map_err(|d| anyhow::anyhow!("{id}: {}", serde_json::to_string_pretty(&d).unwrap()))
}
fn subject_value() -> Value {
    json!({"id":"candidate-1","revision":1})
}
fn output() -> Value {
    json!({"reference":"evidence-1","subject":subject_value()})
}
fn done(outcome: &str) -> DispatchResult {
    let mut value = output();
    if outcome == "completed" {
        value["disposition"] = json!("accepted");
    } else if outcome == "assessed" {
        value["choice"] = json!("permitted-default");
    }
    DispatchResult::Completed {
        outcome: outcome.into(),
        output: value,
    }
}

fn work(kind: &str) -> Expr {
    lit(
        json!({"kind":kind,"subject":subject_value(),"logical_key":format!("pilot-{kind}"),"lifetime":"owned","context":"snapshot-1","visibility":"project"}),
    )
}
fn context(view: &str) -> Expr {
    lit(json!({"view":view,"subject":subject_value(),"snapshot":"snapshot-1","page":0}))
}
fn observed() -> DispatchResult {
    DispatchResult::Completed {
        outcome: "observed".into(),
        output: json!({"reference":"observation-1","subject":subject_value(),"snapshot":"snapshot-1","complete":true,"next_page":null,"visibility":"project","observed_at_ms":0,"expires_at_ms":60000}),
    }
}
fn base_observation(revision: u64) -> DispatchResult {
    let mut value = match observed() {
        DispatchResult::Completed { output, .. } => output,
        _ => unreachable!(),
    };
    value["subject"] = json!({"id":"main","revision":revision});
    value["snapshot"] = json!(format!("base-{revision}"));
    DispatchResult::Completed {
        outcome: "observed".into(),
        output: value,
    }
}
fn base(
    artifacts: Vec<Artifact>,
    name: &str,
    mocks: BTreeMap<String, Vec<DispatchResult>>,
    input: Value,
    actions: Vec<Action>,
) -> Scenario {
    let a = artifacts.last().unwrap();
    let authority = Authority {
        actor: "pilot-user".into(),
        scope: "project".into(),
        capabilities: a.content.playbook.required_capabilities.clone(),
    };
    Scenario {
        bindings: BTreeMap::from([(name.into(), a.digest.clone())]),
        authority: authority.clone(),
        artifacts,
        starts: vec![Start {
            binding: name.into(),
            entry: "main".into(),
            input,
            authority,
        }],
        mocks,
        actions,
        max_transitions: 1000,
    }
}
fn projected_result(root: &str, prefix: &[&str]) -> Expr {
    let mut reference_path = prefix.to_vec();
    reference_path.push("reference");
    let mut subject_path = prefix.to_vec();
    subject_path.push("subject");
    fields(&[
        ("reference", reference(root, &reference_path)),
        ("subject", reference(root, &subject_path)),
    ])
}
fn accepted_work(root: &str, prefix: &[&str]) -> Expr {
    let mut path = prefix.to_vec();
    path.push("disposition");
    Expr::Eq {
        left: Box::new(reference(root, &path)),
        right: Box::new(lit(json!("accepted"))),
    }
}
fn work_child(id: &str, kind: &str) -> anyhow::Result<Artifact> {
    let r = catalog();
    let event = ContractRef {
        name: "operation.changed".into(),
        fingerprint: r.events["operation.changed"].fingerprint()?,
    };
    let input_schema = if kind == "repair" {
        Schema::string(128)
    } else {
        Schema::Null
    };
    let request = if kind == "repair" {
        fields(&[
            ("kind", lit(json!(kind))),
            ("subject", lit(subject_value())),
            ("logical_key", reference("input", &[])),
            ("lifetime", lit(json!("owned"))),
            ("context", lit(json!("snapshot-1"))),
            ("visibility", lit(json!("project"))),
        ])
    } else {
        work(kind)
    };
    build(
        id,
        r.clone(),
        input_schema,
        result(),
        "request",
        BTreeMap::from([
            (
                "request".into(),
                invoke(
                    &r,
                    "work.ensure",
                    request,
                    "work",
                    &[("completed", "assess"), ("accepted", "await_result")],
                ),
            ),
            (
                "assess".into(),
                edge(
                    Operation::Decide {
                        predicate: accepted_work("work", &[]),
                    },
                    &[
                        ("true", "done"),
                        ("false", "failure"),
                        ("unknown", "failure"),
                    ],
                ),
            ),
            ("done".into(), ret("success", projected_result("work", &[]))),
            (
                "await_result".into(),
                edge(
                    Operation::Wait {
                        event: Some(event),
                        correlation: BTreeMap::from([(
                            "operation".into(),
                            reference("work", &["handle"]),
                        )]),
                        minimum_revision: Some(lit(json!(1))),
                        deadline_ms: 10000,
                        bind: "completion".into(),
                    },
                    &[("received", "assess_event"), ("timeout", "failure")],
                ),
            ),
            (
                "assess_event".into(),
                edge(
                    Operation::Decide {
                        predicate: Expr::And {
                            values: vec![
                                Expr::Eq {
                                    left: Box::new(reference("completion", &["outcome"])),
                                    right: Box::new(lit(json!("completed"))),
                                },
                                accepted_work("completion", &["output"]),
                            ],
                        },
                    },
                    &[
                        ("true", "done_event"),
                        ("false", "failure"),
                        ("unknown", "failure"),
                    ],
                ),
            ),
            (
                "done_event".into(),
                ret("success", projected_result("completion", &["output"])),
            ),
        ]),
        &[],
    )
}

pub fn scenario(name: &str) -> anyhow::Result<Scenario> {
    match name {
        "coding" => coding(),
        "routing" => routing(),
        "watcher" => watcher(),
        "report" => report(),
        _ => anyhow::bail!("unknown pilot {name}; choose {}", NAMES.join(", ")),
    }
}
fn coding() -> anyhow::Result<Scenario> {
    let r = catalog();
    let implementation = work_child("implementation", "implementation")?;
    let review = work_child("review", "review")?;
    let repair = work_child("repair", "repair")?;
    let validation = build(
        "validation",
        r.clone(),
        subject(),
        result(),
        "check",
        BTreeMap::from([
            (
                "check".into(),
                invoke(
                    &r,
                    "check.request",
                    fields(&[
                        ("subject", reference("input", &[])),
                        ("suite", lit(json!("integration"))),
                        ("execution_context", lit(json!("fake-ci"))),
                    ]),
                    "check",
                    &[("passed", "done")],
                ),
            ),
            ("done".into(), ret("success", reference("check", &[]))),
        ]),
        &[],
    )?;
    let followup = build(
        "post-delivery-report",
        r.clone(),
        Schema::Null,
        Schema::Null,
        "send",
        BTreeMap::from([
            (
                "send".into(),
                invoke(
                    &r,
                    "message.send",
                    lit(
                        json!({"identity":"coding-delivered","recipient":"supervisor","visibility":"project","body":"Candidate delivered"}),
                    ),
                    "message",
                    &[("accepted", "done")],
                ),
            ),
            ("done".into(), ret("success", lit(Value::Null))),
        ]),
        &[],
    )?;
    let mut steps = BTreeMap::new();
    steps.insert(
        "implementation".into(),
        call(
            "implementation",
            lit(Value::Null),
            CallMode::Wait,
            "implementation_result",
            &[("success", "review"), ("escalated", "failure")],
        ),
    );
    steps.insert(
        "review".into(),
        call(
            "review",
            lit(Value::Null),
            CallMode::Wait,
            "review_result",
            &[("success", "attempts"), ("escalated", "failure")],
        ),
    );
    steps.insert(
        "attempts".into(),
        edge(
            Operation::ForEach {
                collection: lit(json!(["repair-1", "repair-2", "exhausted"])),
                item: "attempt".into(),
                body: "observe_base".into(),
                next: "failure".into(),
                max_items: 3,
                concurrency: 1,
                on_failure: IterationFailure::Stop,
            },
            &[],
        ),
    );
    steps.insert(
        "can_repair".into(),
        edge(
            Operation::Decide {
                predicate: Expr::Eq {
                    left: Box::new(reference("attempt", &[])),
                    right: Box::new(lit(json!("exhausted"))),
                },
            },
            &[
                ("true", "failure"),
                ("false", "repair"),
                ("unknown", "failure"),
            ],
        ),
    );
    steps.insert("observe_base".into(),invoke(&r,"context.read",lit(json!({"view":"project","subject":{"id":"main","revision":1},"snapshot":"","page":0})),"base",&[("observed","prepare")]));
    steps.insert(
        "prepare".into(),
        invoke(
            &r,
            "candidate.prepare",
            fields(&[
                ("sources", lit(json!([subject_value()]))),
                ("base", reference("base", &["subject"])),
            ]),
            "candidate",
            &[("prepared", "validate"), ("conflict", "can_repair")],
        ),
    );
    steps.insert(
        "validate".into(),
        call(
            "validation",
            reference("candidate", &["subject"]),
            CallMode::Wait,
            "checks",
            &[("success", "publish"), ("escalated", "can_repair")],
        ),
    );
    steps.insert(
        "publish".into(),
        invoke(
            &r,
            "candidate.publish",
            fields(&[
                ("candidate", reference("candidate", &["subject"])),
                ("destination", reference("base", &["subject"])),
                (
                    "evidence",
                    Expr::Array {
                        items: vec![reference("checks", &["subject"])],
                    },
                ),
            ]),
            "published",
            &[("published", "followup"), ("conflict", "can_repair")],
        ),
    );
    steps.insert(
        "repair".into(),
        call(
            "repair",
            reference("attempt", &[]),
            CallMode::Wait,
            "repair_result",
            &[("success", "attempts"), ("escalated", "failure")],
        ),
    );
    steps.insert(
        "followup".into(),
        call(
            "report",
            lit(Value::Null),
            CallMode::Independent,
            "report_handle",
            &[("started", "done")],
        ),
    );
    steps.insert("done".into(), ret("success", lit(Value::Null)));
    let root = build(
        "coding",
        r,
        Schema::Null,
        Schema::Null,
        "implementation",
        steps,
        &[
            ("implementation", &implementation),
            ("review", &review),
            ("repair", &repair),
            ("validation", &validation),
            ("report", &followup),
        ],
    )?;
    Ok(base(
        vec![implementation, review, repair, validation, followup, root],
        "coding",
        BTreeMap::from([
            (
                "context.read".into(),
                vec![base_observation(1), base_observation(2)],
            ),
            (
                "work.ensure".into(),
                vec![done("completed"), done("completed"), done("completed")],
            ),
            (
                "candidate.prepare".into(),
                vec![done("prepared"), done("prepared")],
            ),
            ("check.request".into(), vec![done("passed"), done("passed")]),
            (
                "candidate.publish".into(),
                vec![done("conflict"), done("published")],
            ),
            ("message.send".into(), vec![done("accepted")]),
        ]),
        Value::Null,
        vec![],
    ))
}
fn routing() -> anyhow::Result<Scenario> {
    let r = catalog();
    let steps = BTreeMap::from([
        (
            "context".into(),
            invoke(
                &r,
                "context.read",
                context("execution_options"),
                "context",
                &[("observed", "assess")],
            ),
        ),
        (
            "assess".into(),
            edge(
                Operation::Decide {
                    predicate: reference("input", &["needs_judgment"]),
                },
                &[
                    ("true", "judgment"),
                    ("false", "default_change"),
                    ("unknown", "failure"),
                ],
            ),
        ),
        (
            "judgment".into(),
            invoke(
                &r,
                "judgment.request",
                lit(
                    json!({"subject":subject_value(),"context":"snapshot-1","question":"Choose an eligible execution preference","choices":["permitted-default"]}),
                ),
                "assessment",
                &[("assessed", "change")],
            ),
        ),
        (
            "default_change".into(),
            invoke(
                &r,
                "work.change",
                lit(
                    json!({"variant":"execution_constraints","subject":subject_value(),"value":"permitted-default","hold_id":null}),
                ),
                "default_change_result",
                &[("changed", "done")],
            ),
        ),
        (
            "change".into(),
            invoke(
                &r,
                "work.change",
                fields(&[
                    ("variant", lit(json!("execution_constraints"))),
                    ("subject", lit(subject_value())),
                    ("value", reference("assessment", &["choice"])),
                    ("hold_id", lit(Value::Null)),
                ]),
                "change",
                &[("changed", "done")],
            ),
        ),
        ("done".into(), ret("success", lit(Value::Null))),
    ]);
    let a = build(
        "routing",
        r,
        object(&[("needs_judgment", Schema::Boolean)]),
        Schema::Null,
        "context",
        steps,
        &[],
    )?;
    Ok(base(
        vec![a],
        "routing",
        BTreeMap::from([
            ("context.read".into(), vec![observed()]),
            ("judgment.request".into(), vec![done("assessed")]),
            ("work.change".into(), vec![done("changed")]),
        ]),
        json!({"needs_judgment":true}),
        vec![],
    ))
}
fn watcher() -> anyhow::Result<Scenario> {
    let r = catalog();
    let steps = BTreeMap::from([
        (
            "observe".into(),
            invoke(
                &r,
                "context.read",
                context("health_observations"),
                "observation",
                &[("observed", "reconcile"), ("incomplete", "reconcile")],
            ),
        ),
        (
            "reconcile".into(),
            invoke(
                &r,
                "findings.reconcile",
                fields(&[
                    ("partition", lit(json!("project-health"))),
                    ("observation", reference("observation", &["snapshot"])),
                    ("evaluation", lit(json!("evaluation-1"))),
                    (
                        "findings",
                        lit(
                            json!([{"key":"stuck-work","evidence":"work-1","expected_revision":0}]),
                        ),
                    ),
                    ("reminder_ms", lit(json!(3600000))),
                ]),
                "findings",
                &[("reconciled", "done")],
            ),
        ),
        ("done".into(), ret("success", lit(Value::Null))),
    ]);
    let a = build(
        "watcher",
        r,
        Schema::Null,
        Schema::Null,
        "observe",
        steps,
        &[],
    )?;
    Ok(base(
        vec![a],
        "watcher",
        BTreeMap::from([
            ("context.read".into(), vec![observed()]),
            ("findings.reconcile".into(), vec![done("reconciled")]),
        ]),
        Value::Null,
        vec![],
    ))
}
fn report() -> anyhow::Result<Scenario> {
    let r = catalog();
    let author = work_child("report-author", "supervisor_authoring")?;
    let mut steps = BTreeMap::from([
        (
            "context".into(),
            invoke(
                &r,
                "context.read",
                context("project"),
                "context",
                &[("observed", "author")],
            ),
        ),
        (
            "author".into(),
            call(
                "author",
                lit(Value::Null),
                CallMode::OwnedAsync,
                "author_handle",
                &[("started", "wait")],
            ),
        ),
        (
            "wait".into(),
            edge(
                Operation::Join {
                    member_keys: vec![],
                    members: vec![reference("author_handle", &[])],
                    mode: JoinMode::All,
                    accepted_outcomes: strings(&["success"]),
                    deadline_ms: 1000,
                    remaining: RemainingPolicy::Cancel,
                    bind: "author_result".into(),
                },
                &[
                    ("completed", "authored"),
                    ("timeout", "fallback"),
                    ("no_match", "fallback"),
                ],
            ),
        ),
        ("done".into(), ret("success", lit(Value::Null))),
    ]);
    for (name, body) in [
        (
            "authored",
            "Supervisor report is available as retained author work",
        ),
        (
            "fallback",
            "Supervisor report deadline elapsed; frozen context snapshot-1 remains available",
        ),
    ] {
        steps.insert(name.into(),invoke(&r,"message.send",lit(json!({"identity":"report-window-1","recipient":"supervisor","visibility":"project","body":body})),&format!("{name}_message"),&[("accepted","done")]));
    }
    let root = build(
        "report",
        r.clone(),
        Schema::Null,
        Schema::Null,
        "context",
        steps,
        &[("author", &author)],
    )?;
    let event = EventEnvelope {
        producer: "aq.operations".into(),
        event_id: "author-complete".into(),
        contract: ContractRef {
            name: "operation.changed".into(),
            fingerprint: r.events["operation.changed"].fingerprint()?,
        },
        scope: "project".into(),
        correlation: BTreeMap::from([("operation".into(), "author-operation".into())]),
        subject: Some("candidate-1".into()),
        revision: Some(1),
        causation: None,
        occurred_at_ms: Some(2000),
        observed_at_ms: Some(2000),
        payload: json!({"outcome":"completed","output":{"reference":"evidence-1","subject":subject_value(),"disposition":"accepted"}}),
    };
    Ok(base(
        vec![author, root],
        "report",
        BTreeMap::from([
            ("context.read".into(), vec![observed()]),
            (
                "work.ensure".into(),
                vec![DispatchResult::Accepted {
                    handle: "author-operation".into(),
                }],
            ),
            ("message.send".into(), vec![done("accepted")]),
        ]),
        Value::Null,
        vec![
            Action::Tick { at_ms: 1001 },
            Action::Event {
                at_ms: 2000,
                producer: "aq.operations".into(),
                event: Box::new(event),
            },
        ],
    ))
}
/// Reconstruct the exact generated source and explicit proposal for authoring examples.
pub fn compile_input(artifact: &Artifact) -> CompileInput {
    let c = &artifact.content;
    let id = &c.playbook.id;
    CompileInput {
        source: format!(
            "---aq-playbook\n{}\n---\n\n# {id}\n\nA synthetic bounded pilot. Domain services retain authority.\n",
            serde_json::to_string(&c.source.header).unwrap()
        ),
        proposal: Proposal {
            playbook: c.playbook.clone(),
            source_map: c.source_map.clone(),
            questions: vec![],
            unsupported_features: vec![],
        },
        registry: c.registry.clone(),
        dependencies: c.dependencies.clone(),
        definitions: c.definitions.clone(),
        compiler: c.compiler.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_four_pilots_compile_and_execute() {
        for name in NAMES {
            let s = scenario(name).unwrap();
            let report = crate::simulation::simulate(&s).unwrap();
            assert!(
                report.complete,
                "{name}: {}",
                serde_json::to_string_pretty(&report).unwrap()
            );
            let roots = report.inspection["runs"].as_array().unwrap();
            assert!(
                roots
                    .iter()
                    .any(|r| r["id"] == report.roots[0] && r["outcome"] == "success"),
                "{name}: {}",
                serde_json::to_string_pretty(&report).unwrap()
            );
        }
    }
    #[test]
    fn missing_mock_is_not_success() {
        let mut s = scenario("routing").unwrap();
        s.mocks.remove("judgment.request");
        let report = crate::simulation::simulate(&s).unwrap();
        assert!(!report.complete);
        assert!(report.coverage.contains("unresolved"));
    }
    #[test]
    fn coding_conflict_repairs_and_starts_independent_followup() {
        let s = scenario("coding").unwrap();
        let report = crate::simulation::simulate(&s).unwrap();
        let effects = report.inspection["effects"].as_array().unwrap();
        assert_eq!(
            effects
                .iter()
                .filter(|e| e["command"] == "candidate.publish")
                .count(),
            2
        );
        assert!(
            report.inspection["runs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r["origin"].is_string() && r["parent"].is_null())
        );
    }
    #[test]
    fn report_fallback_and_late_author_publish_once() {
        let s = scenario("report").unwrap();
        let report = crate::simulation::simulate(&s).unwrap();
        assert_eq!(
            report.inspection["effects"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|e| e["command"] == "message.send")
                .count(),
            1
        );
    }
}
