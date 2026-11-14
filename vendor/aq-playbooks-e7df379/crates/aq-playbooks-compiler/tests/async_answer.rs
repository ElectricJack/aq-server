//! Compiling "ask, then read the answer": the FORMAT.md example compiles; a
//! draft that joins a handle through `["handle"]` or reads a field not every
//! accepted outcome has is refused, while the same content, already stored,
//! still verifies.
use aq_playbooks_compiler::{COMPILER_VERSION, CompileInput, Proposal, compile};
use aq_playbooks_model::*;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

const REGISTRY_2_1: &str = include_str!("../../../contracts/aq-host-2.1.0/registry.json");
const FORMAT: &str = include_str!("../../aq-playbooks-model/FORMAT.md");
const OUTCOMES: [&str; 5] = ["approved", "declined", "unanswered", "late", "error"];

fn registry() -> Registry {
    let all: Registry = serde_json::from_str(REGISTRY_2_1).unwrap();
    Registry {
        commands: all
            .commands
            .into_iter()
            .filter(|(name, _)| ["judgment.request", "operation.get"].contains(&name.as_str()))
            .collect(),
        events: all
            .events
            .into_iter()
            .filter(|(name, _)| name == "aq.judgment.completed")
            .collect(),
    }
}
/// The FORMAT.md example's steps with the real fingerprint and its returns.
fn example_steps(registry: &Registry) -> BTreeMap<String, Step> {
    let start = FORMAT.find("    \"ask\":").expect("example in FORMAT.md");
    let end = start + FORMAT[start..].find("\n\n").unwrap();
    let fingerprint = registry.commands["judgment.request"].fingerprint().unwrap();
    let block = FORMAT[start..end].replace("sha256:…", &fingerprint);
    let mut steps: BTreeMap<String, Step> =
        serde_json::from_str(&format!("{{{block}}}")).expect("example steps parse");
    for outcome in OUTCOMES {
        steps.insert(
            outcome.into(),
            Step {
                op: Operation::Return {
                    outcome: outcome.into(),
                    output: Expr::literal(Value::Null),
                },
                edges: BTreeMap::new(),
                on_error: None,
            },
        );
    }
    steps
}
fn input(steps: BTreeMap<String, Step>) -> CompileInput {
    let header = SourceHeader {
        format_version: FORMAT_VERSION,
        id: "ask-then-read".into(),
        scope: "project".into(),
        interfaces: BTreeSet::from(["main".into()]),
        definitions: BTreeSet::new(),
    };
    CompileInput {
        source: format!(
            "---aq-playbook\n{}\n---\n# Ask, then read the answer\n",
            serde_json::to_string(&header).unwrap()
        ),
        proposal: Proposal {
            playbook: Playbook {
                id: "ask-then-read".into(),
                purpose: "ask, then read the answer".into(),
                scope: "project".into(),
                entries: BTreeMap::from([(
                    "main".into(),
                    EntryPoint {
                        input: Schema::Null,
                        outcomes: OUTCOMES
                            .into_iter()
                            .map(|o| (o.to_string(), Schema::Null))
                            .collect(),
                        start: "ask".into(),
                    },
                )]),
                steps,
                triggers: vec![],
                limits: Limits::default(),
                required_capabilities: BTreeSet::from(["judgment.request.v2".into()]),
            },
            source_map: BTreeMap::new(),
        },
        registry: registry(),
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::new(),
        compiler: COMPILER_VERSION.into(),
    }
}
fn join(steps: &mut BTreeMap<String, Step>) -> (&mut Vec<Expr>, &mut BTreeSet<String>) {
    let Operation::Join {
        members,
        accepted_outcomes,
        ..
    } = &mut steps.get_mut("read").unwrap().op
    else {
        panic!("read is a join")
    };
    (members, accepted_outcomes)
}

#[test]
fn the_format_example_compiles() {
    let artifact = compile(&input(example_steps(&registry()))).unwrap();
    assert!(artifact.verify().is_ok());
}

#[test]
fn a_draft_joining_the_handle_path_is_refused_but_a_stored_one_loads() {
    let good = compile(&input(example_steps(&registry()))).unwrap();
    let mut steps = example_steps(&registry());
    join(&mut steps).0[0] = Expr::Ref {
        root: "j".into(),
        path: vec!["handle".into()],
    };
    let diagnostics = compile(&input(steps.clone())).unwrap_err();
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    let d = &diagnostics[0];
    assert_eq!(
        (d.code.as_str(), d.path.as_str()),
        ("join_member_type", "steps.read")
    );
    for text in [
        "members[0] (key c)",
        "`j.handle`",
        r#"write {"op":"ref","root":"j","path":[]}"#,
    ] {
        assert!(d.message.contains(text), "{text}: {}", d.message);
    }
    let mut stored = good.content.clone();
    stored.playbook.steps = steps;
    assert!(Artifact::seal(stored).unwrap().verify().is_ok());
}

#[test]
fn a_draft_reading_a_field_some_accepted_outcome_lacks_is_refused_but_a_stored_one_loads() {
    let good = compile(&input(example_steps(&registry()))).unwrap();
    for accepted in [vec![], vec!["assessed", "refused"]] {
        let mut steps = example_steps(&registry());
        *join(&mut steps).1 = accepted.iter().map(|o| o.to_string()).collect();
        let diagnostics = compile(&input(steps.clone())).unwrap_err();
        assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
        let d = &diagnostics[0];
        assert_eq!(
            (d.code.as_str(), d.path.as_str()),
            ("expression_type", "steps.decide")
        );
        for text in [
            "steps.decide.predicate.eq.left: `answer.c.output.choice`",
            "outcome `refused` of member `c` on edge `completed` of join `read` has no `choice`",
        ] {
            assert!(d.message.contains(text), "{text}: {}", d.message);
        }
        let mut stored = good.content.clone();
        stored.playbook.steps = steps;
        assert!(Artifact::seal(stored).unwrap().verify().is_ok());
    }
}
