#![cfg(unix)]

use aq_playbooks_model::{
    CommandContract, Effect, EventContract, Recovery, Registry, Schema, digest,
};
use serde_json::{Value, json};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::Path,
    process::{Command, Output},
    thread,
    time::Duration,
};

const PROJECT: &str = "10000000-0000-4000-8000-000000000004";

fn object<const N: usize>(fields: [(&str, Schema); N]) -> Schema {
    Schema::Object {
        required: fields.iter().map(|(name, _)| (*name).into()).collect(),
        properties: fields
            .into_iter()
            .map(|(name, field)| (name.into(), field))
            .collect(),
    }
}

fn fixture(root: &Path) {
    let command = CommandContract {
        name: "ext.test.ping".into(),
        version: "1".into(),
        input: object([("action", Schema::string(32))]),
        outcomes: [
            ("ok".into(), object([("text", Schema::string(4))])),
            ("refused".into(), object([("reason", Schema::string(32))])),
        ]
        .into(),
        capabilities: ["ext.test".into()].into(),
        scopes: ["project".into()].into(),
        effect: Effect::Mutation,
        recovery: Recovery::KeyedIdempotent,
        asynchronous: None,
        cancellation: false,
        public_result_fields: Default::default(),
        sensitive_input_fields: Default::default(),
        sensitive_output_fields: Default::default(),
        max_payload_bytes: 1024,
        deadline_ms: 8_000,
    };
    let retry_command = CommandContract {
        name: "ext.test.ping_retry".into(),
        deadline_ms: 4_000,
        ..command.clone()
    };
    let event = EventContract {
        name: "ext.test.pinged".into(),
        version: "1".into(),
        payload: object([
            (
                "subject",
                object([
                    ("id", Schema::string(20)),
                    (
                        "revision",
                        Schema::Integer {
                            minimum: 0,
                            maximum: 10,
                        },
                    ),
                ]),
            ),
            ("operation", Schema::string(20)),
        ]),
        trusted_producers: ["ext.test".into()].into(),
        scopes: ["project".into()].into(),
        correlation_fields: ["operation".into()].into(),
        subject_field: Some("subject.id".into()),
        revision_field: Some("subject.revision".into()),
        max_payload_bytes: 1024,
        retention_ms: 60_000,
        sensitive_payload_fields: Default::default(),
    };
    let package = root.join("test");
    let vault = root.join("vault/plugins/test");
    fs::create_dir_all(package.join("bin")).unwrap();
    fs::create_dir_all(package.join("contracts")).unwrap();
    fs::create_dir_all(&vault).unwrap();
    fs::write(
        package.join("plugin.toml"),
        r#"
name = "test"
version = "0.1.0"
protocol = "aq.plugin/1"
exe = "bin/test"
contracts = "contracts/registry.json"
notes = "contracts/notes.json"
bindings = "plugins/test/bindings.toml"
secrets = ["plugins/test/token"]
"#,
    )
    .unwrap();
    let registry = Registry {
        commands: [
            (command.name.clone(), command),
            (retry_command.name.clone(), retry_command),
        ]
        .into(),
        events: [(event.name.clone(), event)].into(),
    };
    fs::write(
        package.join("contracts/registry.json"),
        serde_json::to_vec(&registry).unwrap(),
    )
    .unwrap();
    fs::write(package.join("contracts/notes.json"), b"{}").unwrap();
    fs::write(package.join("bin/test"), include_bytes!("test-plugin.py")).unwrap();
    fs::set_permissions(package.join("bin/test"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(vault.join("token"), b"fake\n").unwrap();
    fs::write(
        vault.join("bindings.toml"),
        format!("[projects.\"{PROJECT}\"]\nmessage='test fixture'\n"),
    )
    .unwrap();
    for name in ["token", "bindings.toml"] {
        fs::set_permissions(vault.join(name), fs::Permissions::from_mode(0o600)).unwrap();
    }
}

fn run(root: &Path, cases: Vec<Value>, override_binding: bool) -> (Output, Value) {
    fs::write(
        root.join("cases.json"),
        serde_json::to_vec(&json!({"cases":cases})).unwrap(),
    )
    .unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_aq-plugin-conformance"));
    command
        .arg("--plugin-dir")
        .arg(root.join("test"))
        .arg("--vault")
        .arg(root.join("vault"))
        .arg("--state-root")
        .arg(root.join("state"))
        .arg("--project")
        .arg(PROJECT)
        .arg("--fixtures")
        .arg(root.join("cases.json"))
        .env("TEST_MUST_NOT_LEAK", "parent only");
    if override_binding {
        let override_path = root.join("binding.json");
        fs::write(&override_path, br#"{"message":"test fixture"}"#).unwrap();
        command.arg("--binding").arg(override_path);
    }
    let output = command.output().unwrap();
    let report = serde_json::from_slice(&output.stdout)
        .unwrap_or_else(|error| panic!("{error}: {}", String::from_utf8_lossy(&output.stderr)));
    (output, report)
}

fn case(id: &str, action: &str, expect: Value) -> Value {
    let command = if action == "timeout_retry" {
        "ext.test.ping_retry"
    } else {
        "ext.test.ping"
    };
    json!({"id":id,"command":command,"input":{"action":action},"expect":expect})
}

#[test]
fn validates_response_replays_exact_bytes_and_kills_the_timed_out_process_group() {
    let temp = tempfile::tempdir().unwrap();
    fixture(temp.path());
    let (output, report) = run(
        temp.path(),
        vec![
            case(
                "normal",
                "echo",
                json!({"outcome":"ok","event_names":["ext.test.pinged"]}),
            ),
            case(
                "retry",
                "timeout_retry",
                json!({"outcome":"ok","retry_after":"timeout"}),
            ),
        ],
        false,
    );
    assert!(output.status.success(), "{report}");
    assert_eq!(report["status"], "passed");
    assert_eq!(report["external_side_effects_verified"], false);
    assert_eq!(report["cases"][0]["result"]["exact_replay"], true);
    assert_eq!(report["cases"][1]["result"]["attempts"], 3);
    let result = &report["cases"][0]["result"];
    let event = &result["envelopes"][0];
    assert_eq!(result["response"]["output"], json!({"text":"ok"}));
    assert_eq!(event["producer"], "ext.test");
    assert_eq!(event["scope"], "project");
    assert_eq!(event["revision"], 2);
    assert_eq!(event["subject"], "topic");
    assert_eq!(event["correlation"]["operation"], "first");
    let intent = format!("conformance:{PROJECT}:normal");
    assert_eq!(event["event_id"], digest(&(&intent, 0_usize)).unwrap());
    assert_eq!(event["causation"], intent);

    let state = temp.path().join("state/test");
    let budgets: Vec<u64> = fs::read_to_string(state.join("retry-budgets"))
        .unwrap()
        .lines()
        .map(|budget| budget.parse().unwrap())
        .collect();
    assert_eq!(budgets.len(), 3);
    assert_eq!(budgets[0], 2_000);
    assert!((1..2_000).contains(&budgets[1]), "{budgets:?}");
    assert_eq!(budgets[2], 2_000);
    // The fixture ledger, independent of the driver's byte comparison, verifies
    // one side effect per distinct key. A timed-out descendant cannot write.
    assert_eq!(
        fs::read_to_string(state.join("effects"))
            .unwrap()
            .lines()
            .count(),
        2
    );
    thread::sleep(Duration::from_millis(2_250));
    assert!(!state.join("orphan").exists());
}

#[test]
fn reports_transport_and_contract_failures_without_returning_any_events() {
    let temp = tempfile::tempdir().unwrap();
    fixture(temp.path());
    let cases = [
        ("exit", "nonzero_exit"),
        ("malformed", "invalid_json"),
        ("outcome", "invalid_response"),
        ("bytes", "invalid_response"),
        ("forbidden_event", "invalid_response"),
        ("bad_event", "invalid_response"),
    ]
    .into_iter()
    .map(|(action, failure)| case(action, action, json!({"failure":failure})))
    .collect();
    let (output, report) = run(temp.path(), cases, true);
    assert!(output.status.success(), "{report}");
    for case in report["cases"].as_array().unwrap() {
        assert_eq!(case["result"]["status"], "expected_failure");
        assert!(case["result"].get("envelopes").is_none());
    }
}

#[test]
fn fails_on_semantically_equal_but_byte_different_stdout() {
    let temp = tempfile::tempdir().unwrap();
    fixture(temp.path());
    let (output, report) = run(
        temp.path(),
        vec![case("changes", "unstable", json!({}))],
        false,
    );
    assert!(!output.status.success());
    assert_eq!(report["status"], "failed");
    assert_eq!(
        report["cases"][0]["result"]["reason"],
        "replay stdout differs byte-for-byte"
    );
}
