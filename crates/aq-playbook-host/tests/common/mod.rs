#![allow(dead_code)]
use aq_playbook_host::{Owner, Request};
use aq_playbooks_model::*;
use aq_playbooks_runner::Authority;
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};
const DEADLINE: Duration = Duration::from_secs(5);
pub fn authority() -> Authority {
    Authority {
        actor: "owner".into(),
        scope: "project".into(),
        capabilities: BTreeSet::new(),
    }
}
pub fn artifact() -> Artifact {
    Artifact::seal(ArtifactContent {
        format_version: 1,
        compiler: "AQ12 synthetic pure workflow fixture".into(),
        source: SourceBinding {
            digest: hash_bytes(b"synthetic"),
            header: SourceHeader {
                format_version: 1,
                id: "test".into(),
                scope: "project".into(),
                interfaces: BTreeSet::from(["main".into()]),
                definitions: BTreeSet::new(),
            },
        },
        playbook: Playbook {
            id: "test".into(),
            purpose: "pure embedding acceptance".into(),
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
                        output: Expr::literal(Value::Null),
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
    .unwrap()
}
pub async fn install(owner: &Owner) -> String {
    let a = artifact();
    let digest = a.digest.clone();
    owner
        .call(
            Request::Install {
                artifact: Box::new(a),
            },
            DEADLINE,
        )
        .await
        .unwrap();
    owner
        .call(
            Request::Activate {
                binding: "test".into(),
                artifact: digest,
                authority: authority(),
            },
            DEADLINE,
        )
        .await
        .unwrap();
    owner
        .call(
            Request::Admit {
                binding: "test".into(),
                entry: "main".into(),
                input: Value::Null,
                authority: authority(),
                now_ms: 1,
            },
            DEADLINE,
        )
        .await
        .unwrap()
        .as_str()
        .unwrap()
        .into()
}
