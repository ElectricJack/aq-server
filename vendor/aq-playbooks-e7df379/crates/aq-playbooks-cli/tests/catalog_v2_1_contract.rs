//! Host catalog 2.1.0 conformance: candidate.prepare returns the immutable
//! candidate source OID that check.request binds, beside frozen 1.0.0 and 2.0.0.
use aq_playbooks_cli::catalog;
use aq_playbooks_model::{Registry, digest, hash_bytes};
use serde_json::{Value, json};

fn fixtures() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/aq-host-2.1.0/fixtures.json"
    ))
    .unwrap()
}

#[test]
fn catalog_v2_1_manifest_schemas_and_fixtures_are_pinned() {
    let registry: Registry = serde_json::from_str(catalog::REGISTRY_2_1_JSON).unwrap();
    let manifest: Value = serde_json::from_str(catalog::MANIFEST_2_1_JSON).unwrap();
    assert_eq!(registry, catalog::catalog_v2_1());
    assert_eq!(manifest["version"], catalog::VERSION_2_1);
    assert_eq!(
        manifest["fingerprint"],
        digest(&(catalog::VERSION_2_1, &registry)).unwrap()
    );
    for (name, command) in &registry.commands {
        assert_eq!(name, &command.name);
        command.input.validate_definition().unwrap();
        for output in command.outcomes.values() {
            output.validate_definition().unwrap();
        }
    }
    let prepare = &registry.commands["candidate.prepare"];
    assert_eq!(prepare.version, "2.1.0");
    // The effect is unchanged, so the existing grant still authorizes it.
    assert_eq!(
        prepare.capabilities,
        catalog::strings(&["candidate.prepare"])
    );
    assert!(prepare.public_result_fields.contains("source"));
    // The prepared source has exactly the check.request input source shape.
    let source = &registry.commands["check.request"].input;
    let Value::Object(check_input) = serde_json::to_value(source).unwrap() else {
        panic!("check.request input is an object")
    };
    let prepared = serde_json::to_value(&prepare.outcomes["prepared"]).unwrap();
    assert_eq!(
        prepared["properties"]["source"],
        check_input["properties"]["source"]
    );
    assert!(
        prepared["required"]
            .as_array()
            .unwrap()
            .contains(&json!("source"))
    );
    for outcome in ["conflict", "refused"] {
        let schema = serde_json::to_value(&prepare.outcomes[outcome]).unwrap();
        assert!(schema["properties"].get("source").is_none(), "{outcome}");
    }
    for fixture in fixtures()["cases"].as_array().unwrap() {
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
fn catalog_v2_1_preserves_2_0_0_bytes_and_unchanged_bindings() {
    let compat = &fixtures()["compatibility"];
    let v1 = catalog::catalog();
    let v2 = catalog::catalog_v2();
    let v21 = catalog::catalog_v2_1();
    let v2_manifest: Value = serde_json::from_str(catalog::MANIFEST_2_JSON).unwrap();
    let v21_manifest: Value = serde_json::from_str(catalog::MANIFEST_2_1_JSON).unwrap();
    assert_eq!(catalog::VERSION_2, "2.0.0");
    assert_eq!(
        hash_bytes(catalog::REGISTRY_2_JSON.as_bytes()),
        compat["base"]["registry_sha256"]
    );
    assert_eq!(v2_manifest["fingerprint"], compat["base"]["fingerprint"]);
    assert_eq!(v2_manifest["fingerprint"], digest(&("2.0.0", &v2)).unwrap());
    assert_eq!(v21_manifest["supersedes"]["version"], "2.0.0");
    assert_eq!(
        v21_manifest["supersedes"]["fingerprint"],
        v2_manifest["fingerprint"]
    );
    for key in ["context_views", "work_kinds", "work_change_variants"] {
        assert_eq!(v21_manifest[key], v2_manifest[key]);
    }
    assert_eq!(
        v2.commands.keys().collect::<Vec<_>>(),
        v21.commands.keys().collect::<Vec<_>>()
    );
    for (name, command) in &v21.commands {
        let old = &v2.commands[name];
        match compat["unchanged_commands"].get(name) {
            Some(pinned) => {
                assert_eq!(command, old, "{name}");
                assert_eq!(&json!(command.fingerprint().unwrap()), pinned, "{name}");
            }
            None => {
                let changed = &compat["changed_commands"][name];
                assert_eq!(changed["from"], old.version.as_str(), "{name}");
                assert_eq!(changed["to"], command.version.as_str(), "{name}");
                assert_eq!(json!(command.capabilities), changed["capabilities"]);
                assert_ne!(command.fingerprint().unwrap(), old.fingerprint().unwrap());
                // Only the prepared outcome and its public projection change.
                let mut restored = command.clone();
                restored.version = old.version.clone();
                restored
                    .outcomes
                    .insert("prepared".into(), old.outcomes["prepared"].clone());
                restored.public_result_fields = old.public_result_fields.clone();
                assert_eq!(&restored, old, "{name}");
            }
        }
    }
    assert_eq!(compat["changed_commands"].as_object().unwrap().len(), 1);
    assert_eq!(
        v1.commands["candidate.prepare"],
        v2.commands["candidate.prepare"]
    );
    assert_eq!(v2.events, v21.events);
    for (name, event) in &v21.events {
        assert_eq!(
            &json!(event.fingerprint().unwrap()),
            &compat["unchanged_events"][name],
            "{name}"
        );
    }
    assert_eq!(compat["added_events"], json!([]));
}

#[test]
fn catalog_v2_1_fake_host_returns_the_prepared_source_only_for_2_1_0() {
    use aq_playbooks_cli::fake_host::FakeHost;
    use aq_playbooks_runner::{Authority, DispatchResult, Host, Intent, IntentStatus};
    let input = json!({"base": {"id": "work-1", "revision": 1}, "sources": []});
    let intent = |command: aq_playbooks_model::CommandContract| Intent {
        id: "prepare-1".into(),
        run: "fixture".into(),
        step: "prepare".into(),
        invocation: 1,
        deadline_at_ms: 60_000,
        input_digest: digest(&input).unwrap(),
        input: input.clone(),
        authority: Authority {
            actor: "alice".into(),
            scope: "project".into(),
            capabilities: command.capabilities.clone(),
        },
        command,
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
    let prepare = |registry: Registry| {
        let command = registry.commands["candidate.prepare"].clone();
        let mut host = FakeHost::default();
        host.grant_all("alice", &["project"]);
        host.subjects.insert("work-1".into(), 1);
        let first = host.dispatch(&intent(command.clone()));
        let DispatchResult::Completed { outcome, output } = &first else {
            panic!("{first:?}")
        };
        assert_eq!(outcome, "prepared");
        assert!(
            command.outcomes["prepared"].validate(output).is_ok(),
            "{output}"
        );
        let host = host.restart();
        let mut host = host;
        assert_eq!(
            serde_json::to_value(host.reconcile(&intent(command))).unwrap(),
            serde_json::to_value(&first).unwrap()
        );
        output.clone()
    };
    let v21 = prepare(catalog::catalog_v2_1());
    assert_eq!(v21["source"]["format"], "sha256");
    assert_eq!(v21["source"]["hex"].as_str().unwrap().len(), 64);
    let v2 = prepare(catalog::catalog_v2());
    assert!(v2.get("source").is_none());
}
