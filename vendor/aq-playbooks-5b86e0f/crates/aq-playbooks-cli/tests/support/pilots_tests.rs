//! The pilots' own tests. They were its module's unit tests while the pilots
//! were part of the CLI's library; `tests/host_pilots.rs` runs them, once.
use crate::pilots::schemas::*;
use crate::pilots::*;
use aq_playbooks_cli::catalog::*;
use aq_playbooks_model::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
#[test]
fn catalog_v2_async_judgment_and_check_compile_and_join() {
    let r = catalog_v2();
    let fixtures: Value = serde_json::from_str(include_str!(
        "../../../../contracts/aq-host-2.0.0/fixtures.json"
    ))
    .unwrap();
    let input = |name: &str| {
        fixtures["replay_cases"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["command"] == name)
            .unwrap()["input"]
            .clone()
    };
    let steps = BTreeMap::from([
        (
            "judgment".into(),
            invoke(
                &r,
                "judgment.request",
                lit(input("judgment.request")),
                "judgment",
                &[("accepted", "check")],
            ),
        ),
        (
            "check".into(),
            invoke(
                &r,
                "check.request",
                lit(input("check.request")),
                "check",
                &[("accepted", "wait")],
            ),
        ),
        (
            "wait".into(),
            edge(
                Operation::Join {
                    member_keys: vec![],
                    // A join names each handle by its binding: the runner
                    // resolves `{"handle": h}`, not the bare handle string.
                    members: vec![reference("judgment", &[]), reference("check", &[])],
                    mode: JoinMode::All,
                    accepted_outcomes: strings(&["assessed", "passed"]),
                    deadline_ms: 60_000,
                    remaining: RemainingPolicy::Track,
                    bind: "results".into(),
                },
                &[
                    ("completed", "done"),
                    ("timeout", "failure"),
                    ("no_match", "failure"),
                ],
            ),
        ),
        ("done".into(), ret("success", lit(Value::Null))),
    ]);
    let artifact = build(
        "v2-adapters",
        r,
        Schema::Null,
        Schema::Null,
        "judgment",
        steps,
        &[],
    )
    .unwrap();
    let retained = &artifact.content.registry;
    assert_eq!(retained.commands["judgment.request"].version, "2.0.0");
    assert!(retained.events.contains_key("aq.judgment.completed"));
    assert!(retained.events.contains_key("aq.check.receipted"));
}
fn prepared_source_check(r: Registry) -> anyhow::Result<Artifact> {
    let check = json!({
        "id": "unit",
        "validator": {"id": "validator-1", "version": "1", "digest": format!("sha256:{}", "b".repeat(64))},
    });
    let steps = BTreeMap::from([
        (
            "prepare".into(),
            invoke(
                &r,
                "candidate.prepare",
                lit(json!({"base": subject_value(), "sources": [subject_value()]})),
                "candidate",
                &[("prepared", "check")],
            ),
        ),
        (
            "check".into(),
            invoke(
                &r,
                "check.request",
                fields(&[
                    ("subject", reference("candidate", &["subject"])),
                    ("check", lit(check)),
                    ("source", reference("candidate", &["source"])),
                ]),
                "check",
                &[("accepted", "done")],
            ),
        ),
        ("done".into(), ret("success", lit(Value::Null))),
    ]);
    build(
        "prepared-source-check",
        r,
        Schema::Null,
        Schema::Null,
        "prepare",
        steps,
        &[],
    )
}
#[test]
fn catalog_v2_1_check_binds_the_prepared_candidate_source() {
    let artifact = prepared_source_check(catalog_v2_1()).unwrap();
    let retained = &artifact.content.registry;
    assert_eq!(retained.commands["candidate.prepare"].version, "2.1.0");
    assert_eq!(retained.commands["check.request"].version, "2.0.0");
    // 2.0.0 has no prepared source, so the same binding cannot compile.
    let err = prepared_source_check(catalog_v2()).unwrap_err().to_string();
    assert!(err.contains("unknown field source"), "{err}");
}
#[test]
fn every_pilot_compiles_and_executes() {
    for name in NAMES {
        let s = scenario(name).unwrap();
        let report = aq_playbooks_cli::simulation::simulate(&s).unwrap();
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
    let mut s = scenario("watcher").unwrap();
    s.mocks.remove("findings.reconcile");
    let report = aq_playbooks_cli::simulation::simulate(&s).unwrap();
    assert!(!report.complete);
    assert!(report.coverage.contains("unresolved"));
}
#[test]
fn coding_conflict_repairs_and_starts_independent_followup() {
    let s = scenario("coding").unwrap();
    let report = aq_playbooks_cli::simulation::simulate(&s).unwrap();
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
/// The draft filler, given a pilot's Markdown, names and catalog,
/// reproduces the pilot's artifact byte for byte; for a pilot with
/// callable children it derives the same registry and capabilities.
#[test]
fn the_draft_filler_reproduces_every_pilot_artifact() {
    use aq_playbooks_compiler::draft::{SourceRecord, Workspace, retain_registry, seal, unfill};
    let mut artifacts = vec![(
        prepared_source_check(catalog_v2_1()).unwrap(),
        catalog_v2_1(),
    )];
    for name in NAMES {
        for a in scenario(name).unwrap().artifacts {
            artifacts.push((a, catalog()));
        }
    }
    let (mut filled, mut derived) = (0, 0);
    for (artifact, catalog) in artifacts {
        let c = &artifact.content;
        let id = &c.playbook.id;
        let (mut commands, mut events) = (BTreeSet::new(), BTreeSet::new());
        for step in c.playbook.steps.values() {
            match &step.op {
                Operation::Invoke { command, .. } => {
                    commands.insert(command.name.clone());
                }
                Operation::Wait { event: Some(e), .. } => {
                    events.insert(e.name.clone());
                }
                _ => {}
            }
        }
        if c.dependencies.is_empty() {
            let draft = unfill(&artifact);
            assert!(!draft.to_string().contains("fingerprint"), "{id}");
            let ws = Workspace {
                markdown: format!(
                    "\n# {id}\n\nA synthetic bounded pilot. Domain services retain authority.\n"
                ),
                record: SourceRecord {
                    id: id.clone(),
                    binding: id.clone(),
                    scope: c.playbook.scope.clone(),
                    purpose: c.playbook.purpose.clone(),
                    commands,
                    events,
                    capabilities: c.playbook.required_capabilities.clone(),
                    facts_version: String::new(),
                },
                catalog,
                facts: BTreeMap::new(),
                draft,
            };
            let sealed = seal(&ws).unwrap_or_else(|d| panic!("{id}: {d:#?}"));
            assert_eq!(
                canonical_bytes(&sealed).unwrap(),
                canonical_bytes(&artifact).unwrap(),
                "{id}"
            );
            filled += 1;
        } else {
            let (registry, capabilities) = retain_registry(&catalog, &commands, &events).unwrap();
            assert_eq!(registry, c.registry, "{id}");
            let children = c.dependencies.values().flat_map(|d| d.capabilities.clone());
            let all: BTreeSet<String> = capabilities.into_iter().chain(children).collect();
            assert_eq!(all, c.playbook.required_capabilities, "{id}");
            // Every operation, calls included, unfills to the model's own
            // JSON with names in place of contract references.
            let mut steps = serde_json::to_value(&c.playbook.steps).unwrap();
            for step in steps.as_object_mut().unwrap().values_mut() {
                let op = &mut step["op"];
                for field in ["command", "event"] {
                    if let Some(name) = op.get(field).and_then(|r| r.get("name")).cloned() {
                        op[field] = name;
                    }
                }
            }
            assert_eq!(unfill(&artifact)["steps"], steps, "{id}");
            assert!(steps.to_string().contains("\"kind\":\"call\""), "{id}");
            derived += 1;
        }
    }
    assert_eq!((filled, derived), (8, 2));
}
#[test]
fn report_fallback_and_late_author_publish_once() {
    let s = scenario("report").unwrap();
    let report = aq_playbooks_cli::simulation::simulate(&s).unwrap();
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
