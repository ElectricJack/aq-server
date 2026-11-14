//! Host catalog 2.0.0 conformance: D6 asynchronous judgment and registered check
//! execution, published beside a byte-frozen 1.0.0.
use aq_playbooks_cli::catalog;
use aq_playbooks_model::{Registry, canonical_bytes, digest, hash_bytes};
use serde_json::{Value, json};

fn fixtures() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/aq-host-2.0.0/fixtures.json"
    ))
    .unwrap()
}

#[test]
fn catalog_v2_manifest_schemas_and_fixtures_are_pinned() {
    let registry: Registry = serde_json::from_str(catalog::REGISTRY_2_JSON).unwrap();
    let manifest: Value = serde_json::from_str(catalog::MANIFEST_2_JSON).unwrap();
    assert_eq!(registry, catalog::catalog_v2());
    assert_eq!(manifest["version"], catalog::VERSION_2);
    assert_eq!(
        manifest["fingerprint"],
        digest(&(catalog::VERSION_2, &registry)).unwrap()
    );
    assert_eq!(registry.commands.len(), 12);
    assert!(registry.events.len() <= 16);
    assert!(canonical_bytes(&registry).unwrap().len() <= 262_144);
    for (name, command) in &registry.commands {
        assert_eq!(name, &command.name);
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
            // operation.get reconciles every completion the catalog can emit.
            let completed = &registry.commands["operation.get"].outcomes["completed"];
            assert!(asynchronous.completion.assignable_to(completed), "{name}");
        }
    }
    for event in registry.events.values() {
        event.payload.validate_definition().unwrap();
        assert_eq!(
            event.trusted_producers,
            catalog::strings(&["aq.host.bridge@1"])
        );
        assert_eq!(event.correlation_fields, catalog::strings(&["operation"]));
    }
    for name in ["judgment.request", "check.request"] {
        let command = &registry.commands[name];
        assert_eq!(command.version, "2.0.0");
        assert!(command.asynchronous.is_some());
        assert_eq!(
            command.capabilities,
            catalog::strings(&[&format!("{name}.v2")]),
            "2.0.0 effects need a new grant, never an inherited 1.0.0 capability"
        );
    }
    assert!(registry.commands["check.request"].cancellation);
    assert!(!registry.commands["judgment.request"].cancellation);
    let fixtures = fixtures();
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
}

#[test]
fn catalog_v2_preserves_1_0_0_bytes_and_unchanged_bindings() {
    let compat = &fixtures()["compatibility"];
    let v1 = catalog::catalog();
    let v2 = catalog::catalog_v2();
    let v1_manifest: Value = serde_json::from_str(catalog::MANIFEST_JSON).unwrap();
    let v2_manifest: Value = serde_json::from_str(catalog::MANIFEST_2_JSON).unwrap();
    assert_eq!(catalog::VERSION, "1.0.0");
    assert_eq!(
        hash_bytes(catalog::REGISTRY_JSON.as_bytes()),
        compat["base"]["registry_sha256"]
    );
    assert_eq!(v1_manifest["fingerprint"], compat["base"]["fingerprint"]);
    assert_eq!(v1_manifest["fingerprint"], digest(&("1.0.0", &v1)).unwrap());
    assert_eq!(
        v2_manifest["supersedes"]["fingerprint"],
        v1_manifest["fingerprint"]
    );
    for key in ["context_views", "work_kinds", "work_change_variants"] {
        assert_eq!(v2_manifest[key], v1_manifest[key]);
    }
    assert_eq!(
        v1.commands.keys().collect::<Vec<_>>(),
        v2.commands.keys().collect::<Vec<_>>()
    );
    for (name, command) in &v2.commands {
        let old = &v1.commands[name];
        match compat["unchanged_commands"].get(name) {
            Some(pinned) => {
                assert_eq!(command, old, "{name}");
                assert_eq!(&json!(command.fingerprint().unwrap()), pinned, "{name}");
            }
            None => {
                let changed = &compat["changed_commands"][name];
                assert_eq!(changed["from"], old.version.as_str(), "{name}");
                assert_eq!(changed["to"], command.version.as_str(), "{name}");
                assert_ne!(command.fingerprint().unwrap(), old.fingerprint().unwrap());
            }
        }
    }
    for (name, event) in &v1.events {
        assert_eq!(&v2.events[name], event, "{name}");
        assert_eq!(
            &json!(event.fingerprint().unwrap()),
            &compat["unchanged_events"][name]
        );
    }
    let added: Vec<_> = v2
        .events
        .keys()
        .filter(|n| !v1.events.contains_key(*n))
        .collect();
    assert_eq!(json!(added), compat["added_events"]);
}

#[test]
fn catalog_v2_replay_fixtures_retain_admission_and_reject_conflicting_reuse() {
    use aq_playbooks_cli::fake_host::FakeHost;
    use aq_playbooks_runner::{Authority, DispatchResult, Host, Intent, IntentStatus};
    let registry = catalog::catalog_v2();
    let capabilities: std::collections::BTreeSet<String> = registry
        .commands
        .values()
        .flat_map(|c| c.capabilities.clone())
        .collect();
    for fixture in fixtures()["replay_cases"].as_array().unwrap() {
        let command = registry.commands[fixture["command"].as_str().unwrap()].clone();
        let input = fixture["input"].clone();
        let intent = Intent {
            id: fixture["operation"].as_str().unwrap().into(),
            run: "fixture".into(),
            step: "request".into(),
            invocation: 1,
            deadline_at_ms: 60_000,
            command: command.clone(),
            input_digest: digest(&input).unwrap(),
            input,
            authority: Authority {
                actor: "alice".into(),
                scope: "project".into(),
                capabilities: capabilities.clone(),
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
        let mut host = FakeHost::default();
        host.grant_all("alice", &["project"]);
        host.subjects.insert("work-1".into(), 1);
        // A 1.0.0 grant never authorizes a 2.0.0 effect.
        assert!(matches!(
            host.dispatch(&intent),
            DispatchResult::Rejected { .. }
        ));
        assert!(host.operations.is_empty());
        host.grants.insert("alice".into(), capabilities.clone());
        let first = host.dispatch(&intent);
        assert!(
            matches!(&first, DispatchResult::Accepted { handle } if handle == fixture["expected_handle"].as_str().unwrap()),
            "{first:?}"
        );
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
