//! Disposable old-writer qualification helper, never a production host.
use aq_playbooks_runner::*;
use std::collections::BTreeSet;
#[allow(dead_code)]
#[path = "../tests/support/mod.rs"]
mod support;
fn main() {
    let namespace = std::env::args().nth(1).expect("namespace");
    let engine = Engine::new(support::store(), namespace, "qualified-new-writer").unwrap();
    let artifact = support::artifact("keyed-probe");
    let hash = artifact.digest.clone();
    engine.install(artifact).unwrap();
    let authority = Authority {
        actor: "operator".into(),
        scope: "project".into(),
        capabilities: BTreeSet::new(),
    };
    engine.activate("probe", &hash, authority.clone()).unwrap();
    let receipt = engine
        .admit_once(
            KeyedAdmissionRequest {
                key: "retained-key".into(),
                binding: "probe".into(),
                expected_activation_revision: engine.snapshot().unwrap().bindings["probe"].revision,
                entry: "main".into(),
                input: serde_json::Value::Null,
                authority,
            },
            1,
        )
        .unwrap();
    assert!(!receipt.duplicate);
    engine.release().unwrap();
    println!("{}", serde_json::to_string(&receipt.receipt).unwrap());
}
