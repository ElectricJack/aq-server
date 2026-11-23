//! Host catalog 2.3.0 conformance (Epic 0, delivery by playbook): 2.2.0 plus
//! the delivery event `aq.work.result_ready`, the delivery commands, their
//! completion event `aq.delivery.completed`, and `work.read` extended with
//! the facts of a task's result.
#[allow(dead_code)]
#[path = "support/schemas.rs"]
mod schemas;
use aq_playbooks_cli::catalog;
use aq_playbooks_compiler::docs;
use aq_playbooks_compiler::draft::{SourceRecord, Workspace, check, seal};
use aq_playbooks_model::*;
use schemas::{enumeration, object, strings, text};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

const ADDED_COMMANDS: [&str; 10] = [
    "branch.publish",
    "main.sync",
    "pull_request.await_checks",
    "pull_request.close",
    "pull_request.merge",
    "pull_request.open",
    "supervisor.notify",
    "work.complete",
    "work.hold",
    "work.return",
];
const ADDED_EVENTS: [&str; 2] = ["aq.delivery.completed", "aq.work.result_ready"];
const CHANGED_COMMANDS: [&str; 1] = ["work.read"];
/// The codes a task is sent back or held for. A playbook cannot build text, so
/// the reason is a code and the sentence a fact value.
const DELIVERY_REASONS: [&str; 8] = [
    "checks_failed",
    "checks_timed_out",
    "merge_conflict",
    "not_mergeable",
    "policy",
    "publish_conflict",
    "pull_request_closed",
    "returns_exhausted",
];
const MERGE_METHODS: [&str; 3] = ["merge", "rebase", "squash"];
const EXAMPLE: &str = "deliver-by-playbook";

fn fixtures() -> Value {
    serde_json::from_str(include_str!(
        "../../../contracts/aq-host-2.3.0/fixtures.json"
    ))
    .unwrap()
}
fn contracts_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../contracts/aq-host-2.3.0")
}

// ---- The generator: 2.2.0 plus the delivery contracts. ----

fn string(max_length: usize) -> Schema {
    Schema::string(max_length)
}
fn code(values: &[&str]) -> Schema {
    Schema::String {
        max_length: 64,
        values: strings(values),
    }
}
fn array(items: Schema, max_items: usize) -> Schema {
    Schema::Array {
        items: Box::new(items),
        max_items,
    }
}
fn range(minimum: i64, maximum: i64) -> Schema {
    Schema::Integer { minimum, maximum }
}
fn without_required(schema: Schema, optional: &[&str]) -> Schema {
    let Schema::Object {
        properties,
        mut required,
    } = schema
    else {
        unreachable!("an object")
    };
    for name in optional {
        required.remove(*name);
    }
    Schema::Object {
        properties,
        required,
    }
}
/// An outcome that says only that it happened.
fn bare() -> Schema {
    object(&[])
}
fn refused(reasons: &[&str]) -> Schema {
    object(&[("reason", enumeration(reasons))])
}
fn pull_request_fact() -> Schema {
    Schema::Nullable {
        inner: Box::new(object(&[
            ("number", range(1, 1_000_000_000)),
            ("url", string(2048)),
            ("state", enumeration(&["closed", "merged", "open"])),
            (
                "mergeable",
                enumeration(&["conflicting", "mergeable", "unknown"]),
            ),
            (
                "checks",
                object(&[
                    (
                        "state",
                        enumeration(&["failed", "none", "passed", "pending"]),
                    ),
                    ("failed", array(string(256), 64)),
                ]),
            ),
        ])),
    }
}
/// The facts 2.3.0 adds to `work.read`: those of the task's latest result.
fn result_facts() -> Vec<(&'static str, Schema)> {
    vec![
        ("outcome", string(64)),
        ("changed", Schema::Boolean),
        ("branch", string(256)),
        ("head", string(128)),
        ("base", string(128)),
        ("main_head", string(128)),
        ("returns", range(0, 1000)),
        ("pull_request", pull_request_fact()),
    ]
}
fn work_read(v22: &Registry) -> CommandContract {
    let mut read = v22.commands["work.read"].clone();
    read.version = "2.3.0".into();
    let Some(Schema::Object { properties, .. }) = read.outcomes.get_mut("observed") else {
        unreachable!("observed is an object")
    };
    let Some(Schema::Object {
        properties: facts,
        required,
    }) = properties.get_mut("facts")
    else {
        unreachable!("facts is an object")
    };
    for (name, schema) in result_facts() {
        facts.insert(name.into(), schema);
        required.insert(name.into());
    }
    read
}

struct Shape {
    deadline_ms: u64,
}
fn command(
    name: &str,
    input: Schema,
    outcomes: &[(&str, Schema)],
    shape: Shape,
    asynchronous: Option<AsyncContract>,
) -> CommandContract {
    CommandContract {
        name: name.into(),
        version: "1.0.0".into(),
        input,
        outcomes: outcomes
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect(),
        capabilities: strings(&[name]),
        scopes: strings(&["project"]),
        effect: Effect::Mutation,
        recovery: Recovery::KeyedIdempotent,
        asynchronous,
        cancellation: false,
        public_result_fields: BTreeSet::new(),
        sensitive_input_fields: BTreeSet::new(),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 65_536,
        deadline_ms: shape.deadline_ms,
    }
}
fn event(
    name: &str,
    payload: Schema,
    correlation: &[&str],
    subject: Option<&str>,
) -> EventContract {
    EventContract {
        sensitive_payload_fields: BTreeSet::new(),
        name: name.into(),
        version: "1.0.0".into(),
        payload,
        trusted_producers: strings(&["aq.host.bridge@1"]),
        scopes: strings(&["project"]),
        correlation_fields: strings(correlation),
        subject_field: subject.map(str::to_string),
        revision_field: subject.map(|_| "version".to_string()),
        max_payload_bytes: 65_536,
        retention_ms: 86_400_000,
    }
}

const SYNC: Shape = Shape {
    deadline_ms: 10_000,
};
const ASYNC: Shape = Shape {
    deadline_ms: 60_000,
};

fn task() -> (&'static str, Schema) {
    ("task", text())
}
fn stale_reasons() -> [&'static str; 3] {
    ["stale_result", "unknown_result", "unknown_task"]
}
fn work_complete() -> CommandContract {
    command(
        "work.complete",
        object(&[task(), ("result_id", text())]),
        &[
            ("completed", bare()),
            ("refused", refused(&stale_reasons())),
        ],
        SYNC,
        None,
    )
}
fn work_return() -> CommandContract {
    command(
        "work.return",
        object(&[
            task(),
            ("result_id", text()),
            ("reason", code(&DELIVERY_REASONS)),
            ("detail", string(1024)),
            ("update_from_main", Schema::Boolean),
        ]),
        &[("returned", bare()), ("refused", refused(&stale_reasons()))],
        SYNC,
        None,
    )
    .with_optional(&["detail"])
}
fn work_hold() -> CommandContract {
    command(
        "work.hold",
        object(&[
            task(),
            ("result_id", text()),
            ("reason", code(&DELIVERY_REASONS)),
        ]),
        &[("held", bare())],
        SYNC,
        None,
    )
}
fn supervisor_notify() -> CommandContract {
    command(
        "supervisor.notify",
        object(&[
            ("project", text()),
            ("identity", text()),
            ("text", string(2048)),
        ]),
        &[("sent", bare()), ("duplicate", bare())],
        SYNC,
        None,
    )
}
trait WithOptional {
    fn with_optional(self, optional: &[&str]) -> Self;
}
impl WithOptional for CommandContract {
    fn with_optional(mut self, optional: &[&str]) -> Self {
        self.input = without_required(self.input, optional);
        self
    }
}

/// An asynchronous delivery command: `accepted {handle}` and its business
/// outcomes. The answer arrives on `aq.delivery.completed`.
fn async_command(
    v22: &Registry,
    name: &str,
    input: Schema,
    outcomes: &[(&str, Schema)],
) -> CommandContract {
    let accepted = v22.commands["judgment.assess"].outcomes["accepted"].clone();
    let mut all = vec![("accepted", accepted)];
    all.extend(outcomes.iter().cloned());
    let completion = object(&[
        (
            "outcome",
            enumeration(&outcomes.iter().map(|(n, _)| *n).collect::<Vec<_>>()),
        ),
        (
            "output",
            Schema::Union {
                variants: outcomes.iter().map(|(_, s)| s.clone()).collect(),
            },
        ),
    ]);
    command(
        name,
        input,
        &all,
        ASYNC,
        Some(AsyncContract {
            completion_event: "aq.delivery.completed".into(),
            completion,
            correlation_field: "operation".into(),
            reconciliation_command: "operation.get".into(),
        }),
    )
}
fn delivery_commands(v22: &Registry) -> Vec<CommandContract> {
    let mut commands = vec![
        work_complete(),
        work_return(),
        work_hold(),
        supervisor_notify(),
    ];
    commands.push(async_command(
        v22,
        "main.sync",
        object(&[("project", text())]),
        &[
            ("synced", object(&[("head", string(128))])),
            ("refused", refused(&["unknown_project", "unreachable"])),
        ],
    ));
    commands.push(async_command(
        v22,
        "branch.publish",
        object(&[task()]),
        &[
            ("published", object(&[("head", string(128))])),
            ("conflict", bare()),
            (
                "refused",
                refused(&["no_branch", "unknown_task", "unreachable"]),
            ),
        ],
    ));
    commands.push(async_command(
        v22,
        "pull_request.open",
        object(&[task()]),
        &[
            (
                "opened",
                object(&[("number", range(1, 1_000_000_000)), ("url", string(2048))]),
            ),
            (
                "refused",
                refused(&["not_published", "unknown_task", "unreachable"]),
            ),
        ],
    ));
    commands.push(
        async_command(
            v22,
            "pull_request.await_checks",
            object(&[
                task(),
                ("required", array(string(256), 64)),
                ("deadline_ms", range(1000, 604_800_000)),
            ]),
            &[
                ("passed", bare()),
                ("failed", object(&[("names", array(string(256), 64))])),
                ("conflict", bare()),
                ("closed", bare()),
                ("timeout", bare()),
            ],
        )
        .with_optional(&["required"]),
    );
    commands.push(async_command(
        v22,
        "pull_request.merge",
        object(&[
            task(),
            ("method", code(&MERGE_METHODS)),
            ("expected_head_sha", string(128)),
        ]),
        &[
            ("merged", bare()),
            ("conflict", bare()),
            ("not_mergeable", bare()),
            (
                "refused",
                refused(&[
                    "head_changed",
                    "no_pull_request",
                    "unknown_task",
                    "unreachable",
                ]),
            ),
        ],
    ));
    commands.push(async_command(
        v22,
        "pull_request.close",
        object(&[task()]),
        &[("closed", bare())],
    ));
    commands
}
/// The one completion event of every asynchronous delivery command: its
/// payload accepts the completion of each of them.
fn delivery_completed(commands: &[CommandContract]) -> EventContract {
    let mut outcomes = BTreeSet::new();
    let mut variants: Vec<Schema> = vec![];
    for c in commands.iter().filter(|c| c.asynchronous.is_some()) {
        for (name, schema) in &c.outcomes {
            if name != "accepted" {
                outcomes.insert(name.clone());
                if !variants.contains(schema) {
                    variants.push(schema.clone());
                }
            }
        }
    }
    event(
        "aq.delivery.completed",
        object(&[
            (
                "outcome",
                enumeration(&outcomes.iter().map(String::as_str).collect::<Vec<_>>()),
            ),
            ("output", Schema::Union { variants }),
        ]),
        &["operation"],
        None,
    )
}
fn result_ready() -> EventContract {
    event(
        "aq.work.result_ready",
        object(&[
            ("task_id", text()),
            ("project_id", text()),
            ("attempt_id", text()),
            ("result_id", text()),
            ("version", range(0, i64::MAX)),
        ]),
        &[],
        Some("task_id"),
    )
}
/// 2.2.0, unchanged, except `work.read`, plus the delivery contracts.
fn build_v2_3() -> Registry {
    let v22 = catalog::catalog_v2_2();
    let mut registry = v22.clone();
    let read = work_read(&v22);
    registry.commands.insert(read.name.clone(), read);
    let commands = delivery_commands(&v22);
    let completed = delivery_completed(&commands);
    for c in commands {
        registry.commands.insert(c.name.clone(), c);
    }
    for e in [completed, result_ready()] {
        registry.events.insert(e.name.clone(), e);
    }
    registry
}
fn pretty(value: &impl serde::Serialize) -> String {
    // A Value orders keys, as every published bundle does.
    let value = serde_json::to_value(value).unwrap();
    serde_json::to_string_pretty(&value).unwrap() + "\n"
}
fn fingerprints<'a, T: 'a>(
    items: impl Iterator<Item = (&'a String, &'a T)>,
    keep: impl Fn(&str) -> bool,
    fingerprint: impl Fn(&T) -> Hash,
) -> Value {
    items
        .filter(|(n, _)| keep(n))
        .map(|(n, c)| (n.clone(), json!(fingerprint(c))))
        .collect::<serde_json::Map<_, _>>()
        .into()
}

#[test]
#[ignore = "writes contracts/aq-host-2.3.0; run by hand after changing the generator"]
fn write_catalog_v2_3() {
    let registry = build_v2_3();
    let dir = contracts_dir();
    let v22 = catalog::catalog_v2_2();
    let v22_manifest: Value = serde_json::from_str(catalog::MANIFEST_2_2_JSON).unwrap();
    let manifest = json!({
        "version": "2.3.0",
        "fingerprint": digest(&("2.3.0", &registry)).unwrap(),
        "supersedes": {"version": "2.2.0", "fingerprint": v22_manifest["fingerprint"]},
        "commands_added": ADDED_COMMANDS,
        "commands_changed": CHANGED_COMMANDS,
        "events_added": ADDED_EVENTS,
        "context_views": v22_manifest["context_views"],
        "work_kinds": v22_manifest["work_kinds"],
        "work_change_variants": v22_manifest["work_change_variants"],
    });
    let added = |n: &str| ADDED_COMMANDS.contains(&n) || ADDED_EVENTS.contains(&n);
    let unchanged = |n: &str| !added(n) && !CHANGED_COMMANDS.contains(&n);
    let command_fp = |c: &CommandContract| c.fingerprint().unwrap();
    let event_fp = |e: &EventContract| e.fingerprint().unwrap();
    let mut fixtures = fixtures();
    fixtures["compatibility"] = json!({
        "base": {
            "version": "2.2.0",
            "fingerprint": v22_manifest["fingerprint"],
            "registry_sha256": hash_bytes(catalog::REGISTRY_2_2_JSON.as_bytes()),
        },
        "unchanged_commands": fingerprints(registry.commands.iter(), unchanged, command_fp),
        "unchanged_events": fingerprints(registry.events.iter(), unchanged, event_fp),
        "changed_commands": CHANGED_COMMANDS.iter().map(|n| {
            (n.to_string(), json!({
                "capabilities": v22.commands[*n].capabilities,
                "from": v22.commands[*n].version,
                "to": registry.commands[*n].version,
            }))
        }).collect::<serde_json::Map<_, _>>(),
        "added_commands": fingerprints(registry.commands.iter(), added, command_fp),
        "added_events": fingerprints(registry.events.iter(), added, event_fp),
    });
    std::fs::write(dir.join("registry.json"), pretty(&registry)).unwrap();
    std::fs::write(dir.join("manifest.json"), pretty(&manifest)).unwrap();
    std::fs::write(dir.join("fixtures.json"), pretty(&fixtures)).unwrap();
}

// ---- Conformance. ----

#[test]
fn catalog_v2_3_manifest_and_fixtures_are_pinned() {
    let registry: Registry = serde_json::from_str(catalog::REGISTRY_2_3_JSON).unwrap();
    let manifest: Value = serde_json::from_str(catalog::MANIFEST_2_3_JSON).unwrap();
    let v22_manifest: Value = serde_json::from_str(catalog::MANIFEST_2_2_JSON).unwrap();
    assert_eq!(registry, catalog::catalog_v2_3());
    // The committed bytes are exactly what the generator writes.
    assert_eq!(registry, build_v2_3());
    assert_eq!(catalog::REGISTRY_2_3_JSON, pretty(&registry));
    assert_eq!(catalog::VERSION_2_3, "2.3.0");
    assert_eq!(manifest["version"], catalog::VERSION_2_3);
    assert_eq!(
        manifest["fingerprint"],
        digest(&(catalog::VERSION_2_3, &registry)).unwrap()
    );
    assert_ne!(manifest["fingerprint"], v22_manifest["fingerprint"]);
    assert_eq!(manifest["supersedes"]["version"], "2.2.0");
    assert_eq!(
        manifest["supersedes"]["fingerprint"],
        v22_manifest["fingerprint"]
    );
    assert_eq!(manifest["commands_added"], json!(ADDED_COMMANDS));
    assert_eq!(manifest["commands_changed"], json!(CHANGED_COMMANDS));
    assert_eq!(manifest["events_added"], json!(ADDED_EVENTS));
    for key in ["context_views", "work_kinds", "work_change_variants"] {
        assert_eq!(manifest[key], v22_manifest[key], "{key}");
    }
    assert_eq!(registry.commands.len(), 16 + ADDED_COMMANDS.len());
    assert_eq!(registry.events.len(), 11 + ADDED_EVENTS.len());
    for (name, command) in &registry.commands {
        assert_eq!(name, &command.name);
        command.input.validate_definition().unwrap();
        for output in command.outcomes.values() {
            output.validate_definition().unwrap();
        }
    }
    for (name, event) in &registry.events {
        assert_eq!(name, &event.name);
        event.payload.validate_definition().unwrap();
    }
    let compat = &fixtures()["compatibility"];
    for name in ADDED_COMMANDS {
        let c = &registry.commands[name];
        assert_eq!(c.version, "1.0.0");
        assert_eq!(c.capabilities, strings(&[name]), "{name}");
        assert_eq!(c.scopes, strings(&["project"]), "{name}");
        assert_eq!(c.max_payload_bytes, 65_536, "{name}");
        assert!(!c.cancellation, "{name}");
        assert_eq!(
            (&c.effect, &c.recovery),
            (&Effect::Mutation, &Recovery::KeyedIdempotent),
            "{name}"
        );
        assert_eq!(
            compat["added_commands"][name],
            json!(c.fingerprint().unwrap())
        );
    }
    for name in ADDED_EVENTS {
        let e = &registry.events[name];
        assert_eq!(e.version, "1.0.0");
        assert_eq!(e.trusted_producers, strings(&["aq.host.bridge@1"]));
        assert_eq!(e.retention_ms, 86_400_000);
        assert_eq!(e.max_payload_bytes, 65_536);
        assert_eq!(
            compat["added_events"][name],
            json!(e.fingerprint().unwrap())
        );
    }
    // Synchronous commands answer within 10 s; asynchronous ones admit within 60 s.
    let asynchronous = [
        "main.sync",
        "branch.publish",
        "pull_request.open",
        "pull_request.await_checks",
        "pull_request.merge",
        "pull_request.close",
    ];
    for name in ADDED_COMMANDS {
        let c = &registry.commands[name];
        assert_eq!(
            c.asynchronous.is_some(),
            asynchronous.contains(&name),
            "{name}"
        );
        assert_eq!(
            c.deadline_ms,
            if asynchronous.contains(&name) {
                60_000
            } else {
                10_000
            },
            "{name}"
        );
    }
    // The event is level-triggered like `aq.work.route_needed`: no correlation.
    let ready = &registry.events["aq.work.result_ready"];
    assert!(ready.correlation_fields.is_empty());
    assert_eq!(ready.subject_field.as_deref(), Some("task_id"));
    assert_eq!(ready.revision_field.as_deref(), Some("version"));
    assert_eq!(
        ready.payload,
        object(&[
            ("task_id", text()),
            ("project_id", text()),
            ("attempt_id", text()),
            ("result_id", text()),
            ("version", range(0, i64::MAX)),
        ])
    );
}

#[test]
fn catalog_v2_3_keeps_every_2_2_0_contract_but_work_read_byte_identical() {
    let compat = &fixtures()["compatibility"];
    let v22 = catalog::catalog_v2_2();
    let v23 = catalog::catalog_v2_3();
    let v22_manifest: Value = serde_json::from_str(catalog::MANIFEST_2_2_JSON).unwrap();
    assert_eq!(compat["base"]["version"], "2.2.0");
    assert_eq!(compat["base"]["fingerprint"], v22_manifest["fingerprint"]);
    assert_eq!(
        hash_bytes(catalog::REGISTRY_2_2_JSON.as_bytes()),
        compat["base"]["registry_sha256"]
    );
    assert_eq!(v22.commands.len(), 16);
    assert_eq!(v22.events.len(), 11);
    assert_eq!(compat["unchanged_commands"].as_object().unwrap().len(), 15);
    assert_eq!(compat["unchanged_events"].as_object().unwrap().len(), 11);
    assert_eq!(compat["changed_commands"].as_object().unwrap().len(), 1);
    for (name, old) in &v22.commands {
        let new = &v23.commands[name];
        match compat["unchanged_commands"].get(name) {
            Some(pinned) => {
                assert_eq!(new, old, "{name}");
                assert_eq!(canonical_bytes(new).unwrap(), canonical_bytes(old).unwrap());
                assert_eq!(&json!(old.fingerprint().unwrap()), pinned, "{name}");
            }
            None => {
                assert_eq!(name, "work.read");
                let changed = &compat["changed_commands"][name];
                assert_eq!(changed["from"], old.version.as_str());
                assert_eq!(changed["to"], new.version.as_str());
                assert_eq!(json!(new.capabilities), changed["capabilities"]);
                assert_eq!(new.capabilities, old.capabilities);
                assert_ne!(new.fingerprint().unwrap(), old.fingerprint().unwrap());
                // Only the version and the facts of `observed` change, by
                // adding fields: nothing 2.2.0 said is taken away.
                let mut restored = new.clone();
                restored.version = old.version.clone();
                let (
                    Schema::Object {
                        properties: p,
                        required: r,
                    },
                    Schema::Object {
                        properties: op,
                        required: or,
                    },
                ) = (&new.outcomes["observed"], &old.outcomes["observed"])
                else {
                    panic!("observed is an object")
                };
                let (
                    Schema::Object {
                        properties: f,
                        required: fr,
                    },
                    Schema::Object {
                        properties: of,
                        required: ofr,
                    },
                ) = (&p["facts"], &op["facts"])
                else {
                    panic!("facts is an object")
                };
                assert_eq!(r, or);
                assert_eq!(p["reference"], op["reference"]);
                assert_eq!(p["subject"], op["subject"]);
                for (field, schema) in of {
                    assert_eq!(&f[field], schema, "facts.{field}");
                }
                assert!(ofr.is_subset(fr));
                let added: BTreeSet<&str> = f
                    .keys()
                    .filter(|k| !of.contains_key(*k))
                    .map(String::as_str)
                    .collect();
                assert_eq!(
                    added,
                    result_facts()
                        .iter()
                        .map(|(n, _)| *n)
                        .collect::<BTreeSet<_>>()
                );
                assert_eq!(
                    &fr.difference(ofr)
                        .map(String::as_str)
                        .collect::<BTreeSet<_>>(),
                    &added
                );
                assert_eq!(new.input, old.input);
                assert_eq!(new.outcomes["refused"], old.outcomes["refused"]);
                assert_eq!(new.deadline_ms, old.deadline_ms);
                assert_eq!(
                    (&new.effect, &new.recovery),
                    (&Effect::Observation, &Recovery::Observation)
                );
            }
        }
    }
    for (name, old) in &v22.events {
        let new = &v23.events[name];
        assert_eq!(new, old, "{name}");
        assert_eq!(
            compat["unchanged_events"][name],
            json!(old.fingerprint().unwrap()),
            "{name}"
        );
    }
    let added: BTreeSet<&str> = v23
        .commands
        .keys()
        .filter(|n| !v22.commands.contains_key(*n))
        .map(String::as_str)
        .collect();
    assert_eq!(added, BTreeSet::from(ADDED_COMMANDS));
    let added: BTreeSet<&str> = v23
        .events
        .keys()
        .filter(|n| !v22.events.contains_key(*n))
        .map(String::as_str)
        .collect();
    assert_eq!(added, BTreeSet::from(ADDED_EVENTS));
    // Each old entry other than work.read appears unchanged in the 2.3.0 file.
    for name in v22.commands.keys().filter(|n| *n != "work.read") {
        let entry =
            serde_json::to_string_pretty(&serde_json::to_value(&v22.commands[name]).unwrap())
                .unwrap()
                .replace('\n', "\n    ");
        assert!(
            catalog::REGISTRY_2_3_JSON.contains(&format!("\"{name}\": {entry}")),
            "{name}"
        );
    }
}

#[test]
fn every_async_delivery_completion_is_assignable_to_its_event() {
    let v23 = catalog::catalog_v2_3();
    let mut seen = 0;
    for name in ADDED_COMMANDS {
        let c = &v23.commands[name];
        let Some(a) = &c.asynchronous else { continue };
        seen += 1;
        assert_eq!(a.completion_event, "aq.delivery.completed", "{name}");
        assert_eq!(a.correlation_field, "operation");
        assert_eq!(a.reconciliation_command, "operation.get");
        let event = &v23.events[&a.completion_event];
        assert!(a.completion.assignable_to(&event.payload), "{name}");
        assert!(event.correlation_fields.contains(&a.correlation_field));
        // Each completion variant is exactly the business outcome of that name.
        let Schema::Object { properties, .. } = &a.completion else {
            panic!("completion is an object")
        };
        let Schema::Union { variants } = &properties["output"] else {
            panic!("output is a union")
        };
        let Schema::String { values, .. } = &properties["outcome"] else {
            panic!("outcome is an enumeration")
        };
        let business: Vec<&String> = c.outcomes.keys().filter(|n| *n != "accepted").collect();
        assert_eq!(values.iter().collect::<Vec<_>>(), business, "{name}");
        assert_eq!(variants.len(), business.len(), "{name}");
        for variant in variants {
            assert!(c.outcomes.values().any(|o| o == variant), "{name}");
        }
    }
    assert_eq!(seen, 6);
    // The structure check of an artifact finds no async_contract issue.
    let sealed = seal(&delivery_workspace()).unwrap_or_else(|d| panic!("{d:#?}"));
    assert_eq!(sealed.content.validate_structure(), vec![]);
}

#[test]
fn valid_and_invalid_fixture_cases_validate_as_declared() {
    let registry = catalog::catalog_v2_3();
    let fixtures = fixtures();
    let mut covered = BTreeSet::new();
    for fixture in fixtures["cases"].as_array().unwrap() {
        let name = fixture["name"].as_str().unwrap();
        let contract = fixture["contract"].as_str().unwrap();
        let kind = fixture["kind"].as_str().unwrap();
        let schema = match kind {
            "input" => &registry.commands[contract].input,
            "output" => &registry.commands[contract].outcomes[fixture["outcome"].as_str().unwrap()],
            "event" => &registry.events[contract].payload,
            other => panic!("unknown fixture kind {other}"),
        };
        let valid = fixture["valid"].as_bool().unwrap();
        assert_eq!(schema.validate(&fixture["value"]).is_ok(), valid, "{name}");
        if valid {
            let place = match kind {
                "output" => format!("{contract}.{}", fixture["outcome"].as_str().unwrap()),
                "input" => format!("{contract}.input"),
                _ => contract.to_string(),
            };
            covered.insert(place);
        }
    }
    // A valid case for every input and outcome of an added command, the
    // observed outcome of work.read, and every added event.
    let mut expected = BTreeSet::new();
    for name in ADDED_COMMANDS {
        expected.insert(format!("{name}.input"));
        for outcome in registry.commands[name].outcomes.keys() {
            expected.insert(format!("{name}.{outcome}"));
        }
    }
    for name in ADDED_EVENTS {
        expected.insert(name.to_string());
    }
    let covered_new: BTreeSet<String> = covered
        .iter()
        .filter(|c| !c.starts_with("work.route") && !c.starts_with("judgment.assess"))
        .filter(|c| !c.starts_with("work.unroutable") && !c.starts_with("aq.judgment"))
        .filter(|c| !c.starts_with("aq.work.route_needed") && !c.starts_with("work.read"))
        .cloned()
        .collect();
    assert_eq!(covered_new, expected);
    for place in ["work.read.input", "work.read.observed", "work.read.refused"] {
        assert!(covered.contains(place), "{place}");
    }
}

// ---- Notes: what the authoring agent reads. ----

#[test]
fn the_notes_cover_every_added_or_changed_contract_and_render() {
    let registry = catalog::catalog_v2_3();
    let all: Value = serde_json::from_str(docs::CATALOG_2_3_NOTES).unwrap();
    let mut commands: BTreeSet<String> = ADDED_COMMANDS.iter().map(|s| s.to_string()).collect();
    commands.extend(CHANGED_COMMANDS.iter().map(|s| s.to_string()));
    let events: BTreeSet<String> = ADDED_EVENTS.iter().map(|s| s.to_string()).collect();
    // The notes cover every contract of the catalog, exactly.
    assert_eq!(
        all["commands"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>(),
        registry.commands.keys().cloned().collect::<BTreeSet<_>>()
    );
    assert_eq!(
        all["events"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>(),
        registry.events.keys().cloned().collect::<BTreeSet<_>>()
    );
    // Every added or changed contract is precisely described: a purpose, every
    // input field, every outcome and every payload field say something.
    for name in &commands {
        let notes = &all["commands"][name];
        assert!(
            !notes["purpose"].as_str().unwrap().trim().is_empty(),
            "{name}"
        );
        for (field, text) in notes["input"].as_object().unwrap() {
            assert!(!text.as_str().unwrap().trim().is_empty(), "{name}.{field}");
        }
        for (outcome, text) in notes["outcomes"].as_object().unwrap() {
            assert!(
                !text.as_str().unwrap().trim().is_empty(),
                "{name}.{outcome}"
            );
        }
    }
    for name in &events {
        let notes = &all["events"][name];
        assert!(
            !notes["purpose"].as_str().unwrap().trim().is_empty(),
            "{name}"
        );
        assert!(!notes["sent"].as_str().unwrap().trim().is_empty(), "{name}");
        for (field, text) in notes["payload"].as_object().unwrap() {
            assert!(!text.as_str().unwrap().trim().is_empty(), "{name}.{field}");
        }
    }
    // The renderer accepts them, and the rendered file names every contract.
    let rendered = docs::catalog_2_3(&commands, &events).unwrap_or_else(|e| panic!("{e}"));
    for name in commands.iter().chain(&events) {
        assert!(rendered.contains(&format!("### `{name}`")), "{name}");
    }
    // The notes on `work.read` describe the result facts.
    for fact in result_facts().iter().map(|(n, _)| *n) {
        assert!(rendered.contains(&format!("`{fact}`")), "{fact}");
    }
}

// ---- Feasibility: the example delivery playbook. ----

fn example_file(name: &str) -> String {
    std::fs::read_to_string(contracts_dir().join("examples").join(EXAMPLE).join(name))
        .unwrap_or_else(|e| panic!("{EXAMPLE}/{name}: {e}"))
}
fn example_json(name: &str) -> Value {
    serde_json::from_str(&example_file(name)).unwrap_or_else(|e| panic!("{EXAMPLE}/{name}: {e}"))
}
fn delivery_workspace() -> Workspace {
    let record: SourceRecord = serde_json::from_value(example_json("record.json")).unwrap();
    let (catalog, _) = aq_playbooks_compiler::draft::retain_registry(
        &catalog::catalog_v2_3(),
        &record.commands,
        &record.events,
    )
    .unwrap();
    let facts: BTreeMap<String, Value> =
        serde_json::from_value(example_json("facts.json")).unwrap();
    Workspace {
        markdown: example_file("source.md"),
        record,
        catalog,
        facts,
        draft: example_json("draft.json"),
    }
}

#[test]
fn the_example_delivery_playbook_checks_and_seals_against_catalog_2_3_0() {
    let ws = delivery_workspace();
    let diagnostics = check(&ws);
    assert!(diagnostics.is_empty(), "{diagnostics:#?}");
    let sealed = seal(&ws).unwrap_or_else(|d| panic!("{d:#?}"));
    assert!(sealed.verify().is_ok());
    // The catalog file the example ships is the one the host places.
    assert_eq!(
        example_json("catalog.json"),
        serde_json::to_value(&ws.catalog).unwrap()
    );
    // It uses the whole delivery path.
    let used: BTreeSet<&str> = ws.draft["steps"]
        .as_object()
        .unwrap()
        .values()
        .filter_map(|s| s["op"]["command"].as_str())
        .collect();
    for name in [
        "work.read",
        "branch.publish",
        "pull_request.open",
        "pull_request.await_checks",
        "pull_request.merge",
        "work.complete",
        "work.return",
        "work.hold",
        "supervisor.notify",
    ] {
        assert!(used.contains(name), "{name}");
    }
    assert_eq!(ws.draft["triggers"][0]["event"], "aq.work.result_ready");
    // Sending a task back is decided by `lt` on the `returns` fact.
    let decide = &ws.draft["steps"]["may_return"]["op"]["predicate"];
    assert_eq!(decide["op"], "lt");
    assert_eq!(decide["left"]["path"], json!(["facts", "returns"]));
    // The strict rules still bite: a fact the catalog does not have is refused.
    let mut wrong = ws.clone();
    wrong.draft["steps"]["may_return"]["op"]["predicate"]["left"]["path"] =
        json!(["facts", "returned"]);
    assert!(!check(&wrong).is_empty());
    // So is a return reason that is not a code.
    let mut wrong = ws;
    wrong.draft["steps"]["send_back"]["op"]["input"]["fields"]["reason"] =
        json!({"op": "literal", "value": "because_i_said_so"});
    let diagnostics = check(&wrong);
    assert!(
        diagnostics.iter().any(|d| d.code == "schema_compatibility"),
        "{diagnostics:#?}"
    );
}

#[test]
fn work_read_observed_with_result_facts_is_valid_and_a_2_2_0_answer_is_not() {
    let v23 = catalog::catalog_v2_3();
    let v22 = catalog::catalog_v2_2();
    let facts = json!({"kind": "feature", "title": "A task", "description": "", "origin": "owner",
        "level_hint": "", "charged_failures": 0, "ended_attempts": 0, "route": "router",
        "outcome": "completed", "changed": true, "branch": "aq/task-1", "head": "a1", "base": "b2",
        "main_head": "c3", "returns": 1,
        "pull_request": {"number": 7, "url": "https://example.test/pull/7", "state": "open",
            "mergeable": "mergeable", "checks": {"state": "failed", "failed": ["build"]}}});
    let read = json!({"reference": "task-1@3", "subject": {"id": "task-1", "revision": 3}, "facts": facts});
    v23.commands["work.read"].outcomes["observed"]
        .validate(&read)
        .unwrap();
    let mut no_pr = read.clone();
    no_pr["facts"]["pull_request"] = Value::Null;
    v23.commands["work.read"].outcomes["observed"]
        .validate(&no_pr)
        .unwrap();
    let mut old = read;
    for (name, _) in result_facts() {
        old["facts"].as_object_mut().unwrap().remove(name);
    }
    assert!(
        v23.commands["work.read"].outcomes["observed"]
            .validate(&old)
            .is_err()
    );
    v22.commands["work.read"].outcomes["observed"]
        .validate(&old)
        .unwrap();
}
