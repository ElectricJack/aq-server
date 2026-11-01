//! Explicit development fixture: no commands, triggers or automatic admission.
use aq_playbook_host::model::*;
use std::collections::{BTreeMap, BTreeSet};

pub fn artifact() -> Artifact {
    const SOURCE: &str = "Development inspection-only definition. No triggers or commands. If explicitly admitted, main returns ok with null output. Startup must not admit it.";
    let name = "development-inspection-only";
    let artifact = Artifact::seal(ArtifactContent {
        format_version: 1,
        compiler: "agent-q labelled development inert example".into(),
        source: SourceBinding {
            digest: hash_bytes(SOURCE.as_bytes()),
            header: SourceHeader {
                format_version: 1,
                id: name.into(),
                scope: "project".into(),
                interfaces: BTreeSet::from(["main".into()]),
                definitions: BTreeSet::new(),
            },
        },
        playbook: Playbook {
            id: name.into(),
            purpose: SOURCE.into(),
            scope: "project".into(),
            entries: BTreeMap::from([(
                "main".into(),
                EntryPoint {
                    input: Schema::Null,
                    outcomes: BTreeMap::from([("ok".into(), Schema::Null)]),
                    start: "done".into(),
                },
            )]),
            steps: BTreeMap::from([(
                "done".into(),
                Step {
                    op: Operation::Return {
                        outcome: "ok".into(),
                        output: Expr::literal(serde_json::Value::Null),
                    },
                    edges: BTreeMap::new(),
                    on_error: None,
                },
            )]),
            triggers: vec![],
            limits: Limits::default(),
            required_capabilities: BTreeSet::new(),
        },
        registry: Registry::default(),
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::new(),
        source_map: BTreeMap::new(),
    })
    .expect("seal labelled inert development artifact");
    artifact
        .verify()
        .expect("validate inert development artifact");
    artifact
}
