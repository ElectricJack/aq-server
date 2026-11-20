//! Host 2.4.0 conformance and generator; the supported 2.2.0 lineage adds
//! decision notes and draft-subtree activation.
use aq_playbooks_cli::catalog;
use aq_playbooks_compiler::{
    docs,
    draft::{SourceRecord, Workspace, seal},
};
use aq_playbooks_model::{
    CommandContract, Effect, Recovery, Registry, Schema, canonical_bytes, digest, hash_bytes,
};
use aq_playbooks_simulation::{Case, run_case};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

const ADDED_COMMAND: &str = "work.activate";
const CHANGED_COMMANDS: [&str; 2] = ["decision.request", "operation.get"];
const CHANGED_EVENT: &str = "aq.decision.completed";
const NOTE_LIMIT: usize = 2048;
const FIXTURES: &str = include_str!("../../../contracts/aq-host-2.4.0/fixtures.json");

fn strings(values: &[&str]) -> BTreeSet<String> {
    values.iter().map(|value| (*value).into()).collect()
}

fn object(fields: &[(&str, Schema)]) -> Schema {
    Schema::object(
        fields
            .iter()
            .map(|(name, schema)| ((*name).into(), schema.clone()))
            .collect(),
    )
}

fn properties(schema: &mut Schema) -> &mut BTreeMap<String, Schema> {
    let Schema::Object { properties, .. } = schema else {
        panic!("expected an object schema, found {schema:?}");
    };
    properties
}

fn add_note(schema: &mut Schema, required: bool) {
    properties(schema).insert("note".into(), Schema::string(NOTE_LIMIT));
    if required {
        let Schema::Object { required, .. } = schema else {
            unreachable!()
        };
        required.insert("note".into());
    }
}

fn build_catalog() -> Registry {
    let mut registry = catalog::catalog_v2_2();
    let decision = registry.commands.get_mut("decision.request").unwrap();
    decision.version = "2.0.0".into();
    for outcome in ["approved", "rejected"] {
        add_note(decision.outcomes.get_mut(outcome).unwrap(), true);
    }
    let completion = &mut decision.asynchronous.as_mut().unwrap().completion;
    let legacy_completion = completion.clone();
    // The completion protocol wraps the possible outputs. The runner checks the
    // exact output schema after it reads the outcome from the event.
    add_note(properties(completion).get_mut("output").unwrap(), false);
    let completion = completion.clone();
    let event = registry.events.get_mut(CHANGED_EVENT).unwrap();
    event.version = "2.0.0".into();
    event.payload = completion.clone();

    let lookup = registry.commands.get_mut("operation.get").unwrap();
    lookup.version = "3.0.0".into();
    let Schema::Union { variants } = lookup.outcomes.get_mut("completed").unwrap() else {
        panic!("operation.get must reconcile multiple asynchronous contracts");
    };
    let decision_variant = variants
        .iter_mut()
        .find(|variant| **variant == legacy_completion)
        .unwrap();
    *decision_variant = completion;

    let work_change = &registry.commands["work.change"];
    let Schema::Object { properties, .. } = &work_change.input else {
        panic!("work.change must take a subject");
    };
    let subject = properties["subject"].clone();
    let count = Schema::Integer {
        minimum: 0,
        maximum: i64::MAX,
    };
    registry.commands.insert(
        ADDED_COMMAND.into(),
        CommandContract {
            name: ADDED_COMMAND.into(),
            version: "1.0.0".into(),
            input: object(&[("subject", subject)]),
            outcomes: BTreeMap::from([
                (
                    "activated".into(),
                    object(&[("activated", count.clone()), ("skipped", count)]),
                ),
                ("refused".into(), object(&[("reason", Schema::string(128))])),
            ]),
            scopes: strings(&["project"]),
            capabilities: strings(&[ADDED_COMMAND]),
            effect: Effect::Mutation,
            recovery: Recovery::KeyedIdempotent,
            asynchronous: None,
            cancellation: false,
            public_result_fields: strings(&["activated", "skipped"]),
            sensitive_input_fields: BTreeSet::new(),
            sensitive_output_fields: BTreeSet::new(),
            max_payload_bytes: work_change.max_payload_bytes,
            deadline_ms: work_change.deadline_ms,
        },
    );
    registry
}

fn notes() -> Value {
    let mut notes: Value = serde_json::from_str(docs::CATALOG_NOTES).unwrap();
    let decision = &mut notes["commands"]["decision.request"];
    decision["outcomes"]["approved"] =
        json!("It was approved. `note` is the responder's note and may be empty.");
    decision["outcomes"]["rejected"] =
        json!("It was rejected. `note` is the responder's note and may be empty.");
    notes["events"][CHANGED_EVENT]["payload"]["output"] = json!(
        "The decision output. Approved and rejected decisions include the responder's `note` of up to 2048 UTF-8 bytes."
    );
    notes["commands"][ADDED_COMMAND] = json!({
        "purpose": "Activates draft descendants of a task the namespace ensured.",
        "input": {"subject": "The namespace's ensured task at an exact revision."},
        "outcomes": {
            "activated": "The draft descendants were activated atomically. `activated` counts the tasks changed and `skipped` counts the descendants already outside draft state.",
            "refused": "The host declined the activation and changed no descendants. `reason` describes the refusal."
        }
    });
    notes
}

fn manifest(registry: &Registry) -> Value {
    let base: Value = serde_json::from_str(catalog::MANIFEST_2_2_JSON).unwrap();
    json!({
        "version": catalog::VERSION_2_4,
        "fingerprint": digest(&(catalog::VERSION_2_4, registry)).unwrap(),
        "supersedes": {"version": base["version"], "fingerprint": base["fingerprint"]},
        "commands_added": [ADDED_COMMAND],
        "commands_changed": CHANGED_COMMANDS,
        "events_changed": [CHANGED_EVENT],
        "context_views": base["context_views"],
        "work_kinds": base["work_kinds"],
        "work_change_variants": base["work_change_variants"]
    })
}

fn fixtures(registry: &Registry) -> Value {
    let base: Value = serde_json::from_str(catalog::MANIFEST_2_2_JSON).unwrap();
    let unchanged_commands: BTreeMap<_, _> = registry
        .commands
        .iter()
        .filter(|(name, _)| {
            name.as_str() != ADDED_COMMAND && !CHANGED_COMMANDS.contains(&name.as_str())
        })
        .map(|(name, command)| (name, command.fingerprint().unwrap()))
        .collect();
    let unchanged_events: BTreeMap<_, _> = registry
        .events
        .iter()
        .filter(|(name, _)| name.as_str() != CHANGED_EVENT)
        .map(|(name, event)| (name, event.fingerprint().unwrap()))
        .collect();
    let subject = json!({"id": "task-1", "revision": 3});
    json!({
        "compatibility": {
            "base": {
                "version": base["version"], "fingerprint": base["fingerprint"],
                "registry_sha256": hash_bytes(catalog::REGISTRY_2_2_JSON.as_bytes())
            },
            "unchanged_commands": unchanged_commands,
            "unchanged_events": unchanged_events,
            "added_commands": {ADDED_COMMAND: registry.commands[ADDED_COMMAND].fingerprint().unwrap()},
            "changed_commands": {
                "decision.request": registry.commands["decision.request"].fingerprint().unwrap(),
                "operation.get": registry.commands["operation.get"].fingerprint().unwrap()
            },
            "changed_events": {CHANGED_EVENT: registry.events[CHANGED_EVENT].fingerprint().unwrap()}
        },
        "cases": [
            {"contract": ADDED_COMMAND, "kind": "input", "valid": true,
                "value": {"subject": subject}},
            {"contract": ADDED_COMMAND, "kind": "input", "valid": false,
                "value": {"subject": {"id": "task-1"}}},
            {"contract": ADDED_COMMAND, "kind": "output", "outcome": "activated", "valid": true,
                "value": {"activated": 4, "skipped": 1}},
            {"contract": ADDED_COMMAND, "kind": "output", "outcome": "activated", "valid": false,
                "value": {"activated": -1, "skipped": 1}},
            {"contract": ADDED_COMMAND, "kind": "output", "outcome": "refused", "valid": true,
                "value": {"reason": "stale_subject"}},
            {"contract": "decision.request", "kind": "output", "outcome": "approved", "valid": true,
                "value": {"reference": "decision:1", "subject": subject, "note": ""}},
            {"contract": "decision.request", "kind": "output", "outcome": "rejected", "valid": true,
                "value": {"reference": "decision:1", "subject": subject, "note": "Include café support."}},
            {"contract": "decision.request", "kind": "output", "outcome": "rejected", "valid": false,
                "value": {"reference": "decision:1", "subject": subject}},
            {"contract": "decision.request", "kind": "output", "outcome": "expired", "valid": true,
                "value": {"reference": "decision:1", "subject": subject}},
            {"contract": CHANGED_EVENT, "kind": "event", "valid": true,
                "value": {"outcome": "rejected", "output": {"reference": "decision:1", "subject": subject, "note": "Include café support."}}},
            {"contract": "operation.get", "kind": "output", "outcome": "completed", "valid": true,
                "value": {"outcome": "rejected", "output": {"reference": "decision:1", "subject": subject, "note": "Include café support."}}}
        ]
    })
}

fn pretty(value: &impl Serialize) -> String {
    let value = serde_json::to_value(value).unwrap();
    serde_json::to_string_pretty(&value).unwrap() + "\n"
}

#[test]
#[ignore = "writes contracts/aq-host-2.4.0; run after changing the generator"]
fn write_catalog_v2_4() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contracts/aq-host-2.4.0");
    let registry = build_catalog();
    std::fs::write(dir.join("registry.json"), pretty(&registry)).unwrap();
    std::fs::write(dir.join("manifest.json"), pretty(&manifest(&registry))).unwrap();
    std::fs::write(dir.join("fixtures.json"), pretty(&fixtures(&registry))).unwrap();
    std::fs::write(dir.join("notes.json"), pretty(&notes())).unwrap();
}

#[test]
fn generated_catalog_files_are_pinned() {
    let registry = catalog::catalog_v2_4();
    assert_eq!(registry, build_catalog());
    assert_eq!(catalog::REGISTRY_2_4_JSON, pretty(&registry));
    assert_eq!(catalog::MANIFEST_2_4_JSON, pretty(&manifest(&registry)));
    assert_eq!(FIXTURES, pretty(&fixtures(&registry)));
    assert_eq!(docs::CATALOG_2_4_NOTES, pretty(&notes()));

    let base = catalog::catalog_v2_2();
    for (name, old) in base.commands {
        assert_eq!(
            old == registry.commands[&name],
            !CHANGED_COMMANDS.contains(&name.as_str()),
            "{name}"
        );
    }
    for (name, old) in base.events {
        assert_eq!(
            old == registry.events[&name],
            name != CHANGED_EVENT,
            "{name}"
        );
    }
    let rendered = docs::catalog_with_extensions(
        &registry,
        docs::CATALOG_2_4_NOTES,
        &strings(&[ADDED_COMMAND, "decision.request"]),
        &BTreeSet::new(),
        &[],
    )
    .unwrap();
    assert!(rendered.contains("Activates draft descendants"));
    assert!(rendered.contains("responder's note"));
    let activation = serde_json::to_value(&registry.commands[ADDED_COMMAND]).unwrap();
    assert!(docs::published_notes("commands", ADDED_COMMAND, &activation).is_some());
}

#[test]
fn decision_note_and_activation_fixture_values_validate() {
    let registry = catalog::catalog_v2_4();
    let fixtures: Value = serde_json::from_str(FIXTURES).unwrap();
    for fixture in fixtures["cases"].as_array().unwrap() {
        let contract = fixture["contract"].as_str().unwrap();
        let schema = match fixture["kind"].as_str().unwrap() {
            "input" => &registry.commands[contract].input,
            "output" => &registry.commands[contract].outcomes[fixture["outcome"].as_str().unwrap()],
            "event" => &registry.events[contract].payload,
            other => panic!("unknown fixture kind {other}"),
        };
        assert_eq!(
            schema.validate(&fixture["value"]).is_ok(),
            fixture["valid"].as_bool().unwrap(),
            "{fixture}"
        );
    }
    let subject = json!({"id": "planner-1", "revision": 3});
    let decision = &registry.commands["decision.request"];
    for outcome in ["approved", "rejected"] {
        let schema = &decision.outcomes[outcome];
        let mut body =
            json!({"reference": "decision:1", "subject": subject, "note": "é".repeat(1024)});
        assert!(schema.validate(&body).is_ok());
        body["note"] = json!("é".repeat(1025));
        assert!(schema.validate(&body).is_err());
    }
}

#[test]
fn a_review_can_read_the_rejection_note_and_activate_only_after_approval() {
    let artifact = seal(&Workspace {
        markdown: "# Plan approval\n\nActivate the planner's draft descendants after approval.\n"
            .into(),
        record: SourceRecord {
            id: "plan-approval".into(),
            binding: "aq.plan-approval".into(),
            scope: "project".into(),
            purpose: "Review a plan and activate its draft descendants".into(),
            commands: strings(&["work.ensure", "decision.request", ADDED_COMMAND]),
            events: BTreeSet::new(),
            capabilities: strings(&[
                "work.ensure",
                "decision.request",
                "operation.get",
                ADDED_COMMAND,
            ]),
            facts_version: "test-1".into(),
        },
        catalog: catalog::catalog_v2_4(),
        facts: BTreeMap::new(),
        draft: serde_json::from_str(include_str!("data/decision-activation-draft.json")).unwrap(),
    })
    .unwrap_or_else(|faults| panic!("{faults:#?}"));
    let bytes = canonical_bytes(&artifact).unwrap();
    assert!(aq_playbooks_model::Artifact::from_canonical_bytes(&bytes).is_ok());

    let subject = json!({"id": "planner-1", "revision": 3});
    for (answer, note, outcome) in [
        ("approved", "", "activated"),
        ("rejected", "Add error handling.", "changes_requested"),
    ] {
        let mut example = json!({
            "name": format!("Plan {answer}"), "situation": "A planner prepared draft work for the owner's review.",
            "start": {"kind": "entry", "entry": "main", "input": null},
            "returns": {
                "work.ensure": [{"at_once": true, "outcome": "ensured", "output": {"reference": "planner-1", "subject": subject}}],
                "decision.request": [{"outcome": answer, "output": {"reference": "decision:1", "subject": subject, "note": note}}]
            },
            "expect": {"outcome": outcome, "calls": [
                {"command": "work.ensure", "input": {"logical_key": "test-plan", "kind": "spec_ingestion", "context": "Plan this work.", "lifetime": "independent", "visibility": "project", "subject": {"id": "parent-1", "revision": 1}}},
                {"command": "decision.request", "input": {"subject": subject, "question": "Approve the draft plan?", "choices": ["approved", "rejected"], "responders": ["user:1"], "expires_at_ms": 86400000}}
            ]}
        });
        if answer == "approved" {
            example["returns"][ADDED_COMMAND] =
                json!([{"outcome": "activated", "output": {"activated": 4, "skipped": 1}}]);
            example["expect"]["calls"]
                .as_array_mut()
                .unwrap()
                .push(json!({"command": ADDED_COMMAND, "input": {"subject": subject}}));
        }
        let case: Case = serde_json::from_value(example).unwrap();
        let report = run_case(&artifact, &case).unwrap();
        assert!(report.passed, "{report:#?}");
    }
}
