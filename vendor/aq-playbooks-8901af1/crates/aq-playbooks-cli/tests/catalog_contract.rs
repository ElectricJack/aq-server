/// The stateful synthetic host, test support only (`tests/support`).
#[allow(dead_code)]
#[path = "support/fake_host.rs"]
mod fake_host;
#[allow(dead_code)]
#[path = "support/schemas.rs"]
mod schemas;
use aq_playbooks_cli::catalog;
use aq_playbooks_model::{Effect, Recovery, Registry, canonical_bytes, digest};
use serde_json::{Value, json};

#[test]
fn published_catalog_manifest_schemas_and_fixtures_are_pinned() {
    let registry: Registry =
        serde_json::from_str(include_str!("../../../contracts/aq-host/registry.json")).unwrap();
    let manifest: Value =
        serde_json::from_str(include_str!("../../../contracts/aq-host/manifest.json")).unwrap();
    assert_eq!(registry, catalog::catalog());
    assert_eq!(manifest["version"], "1.0.0");
    assert_eq!(
        manifest["fingerprint"],
        digest(&(manifest["version"].as_str().unwrap(), &registry)).unwrap()
    );
    assert_eq!(manifest["context_views"], json!(schemas::CONTEXT_VIEWS));
    assert_eq!(manifest["work_kinds"], json!(schemas::WORK_KINDS));
    assert_eq!(
        manifest["work_change_variants"],
        json!(schemas::CHANGE_VARIANTS)
    );
    assert_eq!(registry.commands.len(), 12);
    assert!(registry.events.len() <= 16);
    assert!(canonical_bytes(&registry).unwrap().len() <= 262_144);
    for (name, command) in &registry.commands {
        assert_eq!(name, &command.name);
        assert_eq!(command.version, "1.0.0");
        let expected = match name.as_str() {
            "context.read" | "operation.get" => Recovery::Observation,
            "candidate.publish" => Recovery::Reconcilable,
            _ => Recovery::KeyedIdempotent,
        };
        assert_eq!(command.recovery, expected);
        assert_eq!(
            command.effect == Effect::Observation,
            expected == Recovery::Observation
        );
        command.input.validate_definition().unwrap();
        for output in command.outcomes.values() {
            output.validate_definition().unwrap();
        }
        if let Some(asynchronous) = &command.asynchronous {
            let event = &registry.events[&asynchronous.completion_event];
            assert_eq!(asynchronous.completion, event.payload);
            assert!(
                event
                    .correlation_fields
                    .contains(&asynchronous.correlation_field)
            );
            assert_eq!(asynchronous.reconciliation_command, "operation.get");
        }
    }
    for event in registry.events.values() {
        event.payload.validate_definition().unwrap();
        assert_eq!(
            event.trusted_producers,
            schemas::strings(&["aq.host.bridge@1"])
        );
        assert_eq!(event.correlation_fields, schemas::strings(&["operation"]));
    }
    let fixtures: Value =
        serde_json::from_str(include_str!("../../../contracts/aq-host/fixtures.json")).unwrap();
    for fixture in fixtures["cases"].as_array().unwrap() {
        let name = fixture["name"].as_str().unwrap();
        let contract = fixture["contract"].as_str().unwrap();
        let schema = match fixture["kind"].as_str().unwrap() {
            "input" => &registry.commands[contract].input,
            "output" => &registry.commands[contract].outcomes[fixture["outcome"].as_str().unwrap()],
            "event" => &registry.events[contract].payload,
            other => panic!("unknown fixture type {other}"),
        };
        assert_eq!(
            schema.validate(&fixture["value"]).is_ok(),
            fixture["valid"].as_bool().unwrap(),
            "{name}"
        );
    }
    assert!(!registry.events.contains_key("operation.changed"));
    assert!(
        !registry.commands["message.send"]
            .outcomes
            .contains_key("accepted")
    );
    assert!(
        !registry.commands["operation.get"]
            .outcomes
            .contains_key("succeeded")
    );
    let published: Value = serde_json::from_str(catalog::MANIFEST_JSON).unwrap();
    assert_eq!(published["fingerprint"], manifest["fingerprint"]);
}

#[test]
fn published_replay_fixtures_retain_admission_and_reject_conflicting_reuse() {
    use crate::fake_host::FakeHost;
    use aq_playbooks_runner::{Authority, DispatchResult, Host, Intent, IntentStatus};
    use std::collections::BTreeSet;
    let fixtures: Value =
        serde_json::from_str(include_str!("../../../contracts/aq-host/fixtures.json")).unwrap();
    for fixture in fixtures["replay_cases"].as_array().unwrap() {
        let mut host = FakeHost::default();
        host.grant_all("alice", &["project"]);
        host.visibility
            .insert("supervisor".into(), BTreeSet::from(["project".into()]));
        host.subjects.insert("work-1".into(), 1);
        let input = fixture["input"].clone();
        let intent = Intent {
            id: fixture["operation"].as_str().unwrap().into(),
            run: "fixture".into(),
            step: "request".into(),
            invocation: 1,
            deadline_at_ms: 60_000,
            command: catalog::catalog().commands[fixture["command"].as_str().unwrap()].clone(),
            input_digest: digest(&input).unwrap(),
            input,
            authority: Authority {
                actor: "alice".into(),
                scope: "project".into(),
                capabilities: catalog::catalog().commands.keys().cloned().collect(),
            },
            status: IntentStatus::Prepared,
            cancel_requested: false,
            cancel_dispatched: false,
            late_results: vec![],
            completion_event: None,
            dispatch_sequence: None,
            handle: None,
            result: None,
            reason: None,
        };
        let first = host.dispatch(&intent);
        match &first {
            DispatchResult::Accepted { handle } => {
                assert_eq!(handle, fixture["expected_handle"].as_str().unwrap())
            }
            DispatchResult::Completed { outcome, .. } => {
                assert_eq!(outcome, fixture["expected_outcome"].as_str().unwrap())
            }
            other => panic!("unexpected fixture result {other:?}"),
        }
        host = host.restart();
        assert_eq!(
            serde_json::to_value(host.reconcile(&intent)).unwrap(),
            serde_json::to_value(&first).unwrap()
        );
        assert_eq!(host.operations.len(), 1);
        let mut conflicting = intent;
        conflicting.input = fixture["conflicting_input"].clone();
        conflicting.input_digest = digest(&conflicting.input).unwrap();
        assert!(matches!(
            host.dispatch(&conflicting),
            DispatchResult::Rejected { .. }
        ));
        assert_eq!(host.operations.len(), 1);
    }
}
