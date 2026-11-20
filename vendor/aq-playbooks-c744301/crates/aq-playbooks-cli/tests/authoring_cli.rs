//! The agent's authoring tools, run as the binary in a temporary workspace
//! holding the routing draft and catalog 2.2.0.
#[allow(dead_code)]
#[path = "support/schemas.rs"]
mod schemas;
use aq_playbooks_cli::catalog::catalog_v2_2;
use aq_playbooks_compiler::{
    docs,
    draft::{SourceRecord, retain_registry},
};
use aq_playbooks_model::{
    Artifact, CatalogExtension, EvalContext, EvalLimits, EvalValue, Expr, Registry, eval,
    hash_bytes,
};
use aq_playbooks_simulation::{MAX_CASES, MAX_OUTCOME_BYTES};
use schemas::strings;
use serde_json::{Value, json};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

const ROUTING_DRAFT: &str = include_str!("data/routing_draft.json");
const TEST_EXTENSION: &str = include_str!("data/test-extension.json");
const LINE_21: &str = "Research is Standard, high effort, unless it is clearly harder.";
/// Line 22 continues the sentence of line 21, as a person wraps it.
const LINE_22: &str = "  in which case it is Complex, high effort.";
const LINE_51: &str = "Very high risk work is at least Complex, unless it is narrow and covered by a test, in which case Standard, high effort is enough.";

struct Dir(PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn temp(name: &str) -> Dir {
    let dir = std::env::temp_dir().join(format!(
        "aq-playbooks-authoring-{name}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    Dir(dir)
}
fn run(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_aq-playbooks"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}
/// Runs the command and parses what it prints; `ok` is the exit status.
fn json_of(dir: &Path, args: &[&str]) -> (bool, Value) {
    let out = run(dir, args);
    let value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "{args:?} printed no JSON ({e}): {}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    });
    (out.status.success(), value)
}
fn write(dir: &Path, name: &str, value: &Value) {
    let path = dir.join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

/// A source of 60 lines whose lines 21 (with 22) and 51 are the two
/// sentences the routing document leaves open.
fn markdown() -> String {
    (1..=60)
        .map(|n| match n {
            1 => "# Routing".to_string(),
            21 => LINE_21.to_string(),
            22 => LINE_22.to_string(),
            51 => LINE_51.to_string(),
            n if n % 2 == 0 => String::new(),
            n => format!("Sentence {n} of the document."),
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}
fn subject() -> Value {
    json!({"id": "task-1", "revision": 3})
}
fn observed() -> Value {
    json!({"outcome": "observed", "output": {"reference": "task-1@3", "subject": subject(),
        "facts": {"kind": "feature", "title": "A task", "description": "", "origin": "owner",
            "level_hint": "", "charged_failures": 0, "ended_attempts": 0, "route": "none"}}})
}
fn assessed(kind: &str, risk: &str) -> Value {
    json!({"outcome": "assessed", "output": {"reference": "judgment:op-1", "subject": subject(),
        "labels": [kind, risk], "flags": [], "rationale": "Because.",
        "provenance": {"context_digest": "sha256:a", "credential_profile": "profile/a",
            "model": "model-a", "output_digest": null, "prompt_digest": "sha256:c",
            "provider": "provider-a",
            "usage": {"cost_microunits": 1, "input_tokens": 1, "output_tokens": 1}}}})
}
fn routed() -> Value {
    json!({"outcome": "routed", "output": {"reference": "route-1", "subject": subject(),
        "harnesses": [], "providers": [], "prefer": []}})
}
fn facts() -> Value {
    let everyone = json!({"harnesses": ["claude", "codex", "gemini", "opencode"],
                          "providers": ["anthropic", "google", "ollama", "openai"]});
    json!({
        "levels": ["simple-high", "standard-high", "complex-high", "deep-high"],
        "offers": {"simple-high": everyone, "standard-high": everyone,
                   "complex-high": everyone, "deep-high": everyone},
        "harnesses": ["claude", "codex", "gemini", "opencode"],
        "providers": ["anthropic", "google", "ollama", "openai"],
    })
}
/// A step's input, evaluated as the draft writes it.
fn step_input(step: &str, roots: &[(&str, Value)]) -> Value {
    let draft: Value = serde_json::from_str(ROUTING_DRAFT).unwrap();
    let expr: Expr = serde_json::from_value(draft["steps"][step]["op"]["input"].clone()).unwrap();
    let mut context = EvalContext::from([("definitions".to_string(), EvalValue::Known(facts()))]);
    for (root, value) in roots {
        context.insert(root.to_string(), EvalValue::Known(value.clone()));
    }
    match eval(&expr, &context, &EvalLimits::default()).unwrap() {
        EvalValue::Known(value) => value,
        other => panic!("{step}: {other:?}"),
    }
}
/// A case that lists every call the run makes, each with its whole input.
fn routing_case(name: &str, kind: &str, risk: &str, level: &str) -> Value {
    let task = observed()["output"].clone();
    let answer = assessed(kind, risk)["output"].clone();
    let route = step_input(
        "route",
        &[
            ("task", task.clone()),
            (
                "answer",
                json!({"c": {"participant": "op", "outcome": "assessed", "output": answer}}),
            ),
        ],
    );
    assert_eq!(route["level"], level, "{name}");
    json!({
        "name": name,
        "situation": format!("A {risk} risk {kind} task needs a route"),
        "start": {"kind": "event", "event": "aq.work.route_needed",
                  "payload": {"subject": subject()}},
        "returns": {"work.read": [observed()], "judgment.assess": [assessed(kind, risk)],
                    "work.route": [routed()]},
        "expect": {"outcome": "routed", "calls": [
            {"command": "work.read", "input": {"subject": subject()}},
            {"command": "judgment.assess", "input": step_input("ask", &[("task", task)])},
            {"command": "work.route", "input": route},
        ]},
    })
}

/// The routing workspace the host would write: the document, the record,
/// the catalog kept to what the record allows, the facts, the draft and
/// five cases.
fn workspace(name: &str) -> Dir {
    let dir = temp(name);
    let d = &dir.0;
    fs::write(d.join("source.md"), markdown()).unwrap();
    let record = SourceRecord {
        id: "routing".into(),
        binding: "aq.routing".into(),
        scope: "project".into(),
        purpose: "Route each task".into(),
        commands: strings(&["work.read", "work.route", "judgment.assess"]),
        events: strings(&["aq.work.route_needed"]),
        capabilities: strings(&[
            "work.read",
            "work.route",
            "judgment.assess",
            "operation.get",
        ]),
        facts_version: "settings-1".into(),
    };
    let (catalog, _) = retain_registry(&catalog_v2_2(), &record.commands, &record.events).unwrap();
    write(d, "record.json", &serde_json::to_value(&record).unwrap());
    write(d, "catalog.json", &serde_json::to_value(&catalog).unwrap());
    write(d, "facts.json", &facts());
    fs::write(d.join("draft.json"), ROUTING_DRAFT).unwrap();
    for (file, kind, risk, level) in [
        ("chore", "chore", "low", "simple-high"),
        ("feature", "feature", "low", "standard-high"),
        ("design", "design", "low", "complex-high"),
        ("bugfix", "bugfix", "medium", "standard-high"),
        ("docs", "docs", "low", "standard-high"),
    ] {
        write(
            d,
            &format!("cases/{file}.json"),
            &routing_case(file, kind, risk, level),
        );
    }
    dir
}

#[test]
fn check_try_compare_save_on_a_workspace() {
    let ws = workspace("save");
    let d = &ws.0;
    let (ok, check) = json_of(d, &["check"]);
    assert!(ok, "{check:#}");
    assert_eq!(check["diagnostics"], json!([]));
    assert!(
        check["order"].as_str().unwrap().contains("First"),
        "{check:#}"
    );

    let (ok, report) = json_of(d, &["try", "cases/design.json"]);
    assert!(ok, "{report:#}");
    assert_eq!(report["passed"], true);
    assert_eq!(report["outcome"], "routed");
    let commands: Vec<&str> = report["calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["command"].as_str().unwrap())
        .collect();
    assert_eq!(commands, ["work.read", "judgment.assess", "work.route"]);
    assert_eq!(report["calls"][2]["input"]["level"], "complex-high");

    // No previous playbook yet.
    let (ok, compared) = json_of(d, &["compare"]);
    assert!(ok, "{compared:#}");
    assert_eq!(compared, json!({"previous": null}));
    // seal never overwrites what the host gave the workspace.
    for input in [
        "previous.artifact.json",
        "draft.json",
        "cases/x.json",
        "out/sealed.json",
    ] {
        let (ok, refused) = json_of(d, &["seal", "--output", input]);
        assert!(!ok, "{input}");
        assert!(
            refused["error"].as_str().unwrap().contains(input),
            "{refused:#}"
        );
    }
    let (ok, sealed) = json_of(d, &["seal", "--output", "sealed.artifact.json"]);
    assert!(ok, "{sealed:#}");
    let digest = sealed["artifact"].as_str().unwrap().to_string();
    assert!(digest.starts_with("sha256:"));
    fs::copy(
        d.join("sealed.artifact.json"),
        d.join("previous.artifact.json"),
    )
    .unwrap();
    let (ok, compared) = json_of(d, &["compare"]);
    assert!(ok, "{compared:#}");
    assert_eq!(compared["change"], "identical");
    assert_eq!(compared["abilities_changed"], false);

    let (ok, facts) = json_of(d, &["facts"]);
    assert!(ok, "{facts:#}");
    assert_eq!(facts["facts"]["levels"][0], "simple-high");
    assert_eq!(
        facts["facts"]["offers"]["deep-high"]["harnesses"],
        json!(["claude", "codex", "gemini", "opencode"])
    );

    let (ok, saved) = json_of(d, &["save"]);
    assert!(ok, "{saved:#}");
    let bytes = fs::read(d.join("out/outcome.json")).unwrap();
    let outcome: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(outcome["kind"], "saved");
    assert_eq!(outcome["artifact"], digest.as_str());
    assert_eq!(outcome["facts_digest"], facts["digest"]);
    assert_eq!(
        outcome["draft"],
        serde_json::from_str::<Value>(ROUTING_DRAFT).unwrap()
    );
    let names: Vec<&str> = outcome["cases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["bugfix", "chore", "design", "docs", "feature"]);
    assert!(bytes.len() <= MAX_OUTCOME_BYTES);
    // A run ends once: saving again is refused and leaves the file.
    let (ok, refused) = json_of(d, &["save"]);
    assert!(!ok);
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("a run ends once"),
        "{refused:#}"
    );
    assert_eq!(fs::read(d.join("out/outcome.json")).unwrap(), bytes);
    // From a fresh start, the same inputs write the same file.
    fs::remove_file(d.join("out/outcome.json")).unwrap();
    let (ok, _) = json_of(d, &["save"]);
    assert!(ok);
    assert_eq!(fs::read(d.join("out/outcome.json")).unwrap(), bytes);
    // A change to the document alone compares as wording only; a change to
    // what the playbook asks, as behaviour.
    fs::write(
        d.join("source.md"),
        markdown().replace("Sentence 3 of", "Sentence three of"),
    )
    .unwrap();
    let (ok, compared) = json_of(d, &["compare"]);
    assert!(ok, "{compared:#}");
    assert_eq!(compared["change"], "presentation", "{compared:#}");
    let mut draft: Value = serde_json::from_str(ROUTING_DRAFT).unwrap();
    draft["steps"]["ask"]["op"]["input"]["fields"]["question"]["value"] = json!("Classify.");
    write(d, "draft.json", &draft);
    let (ok, compared) = json_of(d, &["compare"]);
    assert!(ok, "{compared:#}");
    assert_eq!(compared["change"], "executable", "{compared:#}");
}

#[test]
fn extension_contracts_compile_and_run_in_authoring_cases() {
    let ws = temp("extension");
    let d = &ws.0;
    fs::write(d.join("source.md"), "# Echo\n\nEcho a test ping.\n").unwrap();
    write(
        d,
        "record.json",
        &json!({
            "id": "echo", "binding": "aq.echo", "scope": "project", "purpose": "Echo a test ping",
            "commands": ["ext.test.echo"], "events": ["ext.test.pinged"], "capabilities": ["ext.test"],
            "facts_version": "settings-1"
        }),
    );
    write(
        d,
        "catalog.json",
        &serde_json::to_value(Registry::default()).unwrap(),
    );
    write(d, "facts.json", &json!({}));
    fs::write(
        d.join("draft.json"),
        include_str!("data/test-extension-draft.json"),
    )
    .unwrap();
    fs::write(
        d.join("case.json"),
        include_str!("data/test-extension-case.json"),
    )
    .unwrap();

    let (ok, refused) = json_of(d, &["check"]);
    assert!(!ok);
    assert!(
        refused["diagnostics"]
            .as_array()
            .unwrap()
            .iter()
            .any(|fault| fault["code"] == "unknown_command"
                && fault["message"].as_str().unwrap().contains("ext.test.echo")),
        "{refused:#}"
    );

    let extension: CatalogExtension = serde_json::from_str(TEST_EXTENSION).unwrap();
    write(d, "extensions.json", &json!([extension]));
    let (ok, sealed) = json_of(d, &["seal", "--output", "sealed.json"]);
    assert!(ok, "{sealed:#}");
    let artifact =
        Artifact::from_canonical_bytes(&fs::read(d.join("sealed.json")).unwrap()).unwrap();
    assert_eq!(artifact.content.registry, *extension.registry());
    let command = &artifact.content.registry.commands["ext.test.echo"];
    assert_eq!(
        command.fingerprint().unwrap(),
        extension.registry().commands["ext.test.echo"]
            .fingerprint()
            .unwrap()
    );
    let (ok, described) = json_of(d, &["commands", "describe", "ext.test.echo"]);
    assert!(ok, "{described:#}");
    assert_eq!(
        described["notes"],
        extension.notes()["commands"]["ext.test.echo"]
    );
    let (ok, report) = json_of(d, &["try", "case.json"]);
    assert!(ok, "{report:#}");
    assert_eq!(report["outcome"], "done");
    assert_eq!(report["calls"][0]["outcome"], "echoed");

    let rendered = docs::catalog_with_extensions(
        &Registry::default(),
        "{}",
        &strings(&["ext.test.echo"]),
        &strings(&["ext.test.pinged"]),
        &[extension],
    )
    .unwrap();
    assert!(rendered.contains("## Extension `test`"));
    assert!(rendered.contains("Echoes text from a test playbook."));
    assert!(rendered.contains("Reports a test ping."));
}

#[test]
fn an_unused_extension_preserves_the_legacy_artifact_bytes() {
    let ws = temp("extension-golden");
    let d = &ws.0;
    for (name, text) in docs::example_files() {
        if let Some(
            name @ ("source.md" | "record.json" | "catalog.json" | "facts.json" | "draft.json"),
        ) = name.strip_prefix("notify-when-ready/")
        {
            fs::write(d.join(name), text).unwrap();
        }
    }
    let (ok, sealed) = json_of(d, &["seal", "--output", "legacy.json"]);
    assert!(ok, "{sealed:#}");
    let original = fs::read(d.join("legacy.json")).unwrap();
    write(
        d,
        "extensions.json",
        &json!([serde_json::from_str::<Value>(TEST_EXTENSION).unwrap()]),
    );
    let (ok, sealed) = json_of(d, &["seal", "--output", "overlay.json"]);
    assert!(ok, "{sealed:#}");
    assert_eq!(fs::read(d.join("overlay.json")).unwrap(), original);
    assert_eq!(
        hash_bytes(&original),
        "sha256:873718cbd68805b4ae07e3baac171e9e4c0d2241261e9fc086baa3192fca6975"
    );
}

#[test]
fn extensions_refuse_bad_names_outcomes_and_collisions() {
    let extension: CatalogExtension = serde_json::from_str(TEST_EXTENSION).unwrap();
    for forbidden in ["work.ensure", "ext.other.x"] {
        let mut registry = extension.registry().clone();
        let mut command = registry.commands.remove("ext.test.echo").unwrap();
        command.name = forbidden.into();
        registry.commands.insert(forbidden.into(), command);
        assert!(
            CatalogExtension::new("test", registry, json!({}))
                .unwrap_err()
                .to_string()
                .contains("under ext.test.")
        );
    }
    let mut registry = extension.registry().clone();
    registry
        .commands
        .get_mut("ext.test.echo")
        .unwrap()
        .outcomes
        .remove("refused");
    assert!(
        CatalogExtension::new("test", registry, json!({}))
            .unwrap_err()
            .to_string()
            .contains("requires refused")
    );
    assert!(
        extension
            .registry()
            .with_extensions(std::slice::from_ref(&extension))
            .unwrap_err()
            .to_string()
            .contains("duplicate registration")
    );
    let mut registry = extension.registry().clone();
    registry
        .events
        .get_mut("ext.test.pinged")
        .unwrap()
        .revision_field = Some("subject.id".into());
    assert!(
        CatalogExtension::new("test", registry, json!({}))
            .unwrap_err()
            .to_string()
            .contains("integer revision")
    );
}

#[test]
fn save_refuses_a_failing_case_and_writes_no_outcome() {
    let ws = workspace("refuse");
    let d = &ws.0;
    let mut wrong = routing_case("design", "design", "low", "complex-high");
    wrong["expect"]["calls"][2]["input"]["level"] = json!("standard-high");
    write(d, "cases/design.json", &wrong);
    let (ok, refused) = json_of(d, &["save"]);
    assert!(!ok, "{refused:#}");
    assert_eq!(refused["saved"], false);
    let reasons = refused["reasons"].to_string();
    assert!(reasons.contains("design"), "{refused:#}");
    assert!(!d.join("out/outcome.json").exists());
    // The failing case says why when tried.
    let (ok, report) = json_of(d, &["try", "cases/design.json"]);
    assert!(!ok);
    assert_eq!(report["passed"], false);
    assert!(
        report["problems"][0]
            .as_str()
            .unwrap()
            .contains("input.level"),
        "{report:#}"
    );
    // Four cases are too few.
    fs::remove_file(d.join("cases/design.json")).unwrap();
    let (ok, refused) = json_of(d, &["save"]);
    assert!(!ok);
    assert!(
        refused["reasons"].to_string().contains("at least 5"),
        "{refused:#}"
    );
    assert!(!d.join("out/outcome.json").exists());
}

#[test]
fn ask_and_unable_write_the_outcome_with_the_sentence() {
    let ws = workspace("ask");
    let d = &ws.0;
    let question = "Research that is clearly harder: which level should it get?";
    let (ok, printed) = json_of(d, &["ask", "--line", "21", question]);
    assert!(ok, "{printed:#}");
    let bytes = fs::read(d.join("out/outcome.json")).unwrap();
    let outcome: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(
        outcome,
        json!({"kind": "question", "question": question,
               "sentences": [{"line": 21, "text": LINE_21}]})
    );
    // A second ending is refused, whichever it is, and leaves the first.
    for args in [
        &["ask", "--line", "21", question][..],
        &["unable", "--line", "51", "Why not."],
        &["save"],
    ] {
        let (ok, refused) = json_of(d, args);
        assert!(!ok, "{args:?}");
        assert!(
            refused["error"].as_str().unwrap().contains("(question)"),
            "{refused:#}"
        );
    }
    assert_eq!(fs::read(d.join("out/outcome.json")).unwrap(), bytes);
    fs::remove_file(d.join("out/outcome.json")).unwrap();

    let reason = "No command can wait for a person to approve.";
    let (ok, printed) = json_of(d, &["unable", "--line", "51", reason]);
    assert!(ok, "{printed:#}");
    let outcome: Value =
        serde_json::from_slice(&fs::read(d.join("out/outcome.json")).unwrap()).unwrap();
    assert_eq!(
        outcome,
        json!({"kind": "unable", "reason": reason, "line": 51, "sentence": LINE_51})
    );
    // A sentence over two lines.
    fs::remove_file(d.join("out/outcome.json")).unwrap();
    let (ok, printed) = json_of(d, &["unable", "--lines", "21-22", reason]);
    assert!(ok, "{printed:#}");
    let outcome: Value =
        serde_json::from_slice(&fs::read(d.join("out/outcome.json")).unwrap()).unwrap();
    assert_eq!(
        outcome,
        json!({"kind": "unable", "reason": reason, "line": 21, "end_line": 22,
               "sentence": format!("{LINE_21}\n{LINE_22}")})
    );
    // A fault in one of the host's files names the file, not a sentence.
    fs::remove_file(d.join("out/outcome.json")).unwrap();
    let fault = "record.json allows a command catalog.json lacks.";
    let (ok, printed) = json_of(d, &["unable", "--file", "catalog.json", fault]);
    assert!(ok, "{printed:#}");
    let outcome: Value =
        serde_json::from_slice(&fs::read(d.join("out/outcome.json")).unwrap()).unwrap();
    assert_eq!(
        outcome,
        json!({"kind": "unable", "reason": fault, "file": "catalog.json"})
    );
    fs::remove_file(d.join("out/outcome.json")).unwrap();
    for args in [
        &["unable", "--file", "draft.json", fault][..],
        &["unable", "--file", "catalog.json", "--line", "51", fault],
    ] {
        let (ok, refused) = json_of(d, args);
        assert!(!ok, "{args:?}");
        assert!(refused["error"].is_string(), "{refused:#}");
    }
    assert!(!d.join("out/outcome.json").exists());
    // A blank line is not a sentence; one sentence is named once.
    for args in [
        &["unable", "--line", "2", reason][..],
        &["unable", "--line", "21", "--lines", "21-22", reason],
        &["unable", reason],
    ] {
        let (ok, refused) = json_of(d, args);
        assert!(!ok, "{args:?}");
        assert!(refused["error"].is_string(), "{refused:#}");
    }
    let (_, refused) = json_of(d, &["unable", "--line", "2", reason]);
    assert!(
        refused["error"].as_str().unwrap().contains("line 2"),
        "{refused:#}"
    );
}

#[test]
fn one_question_can_name_two_sentences() {
    let ws = workspace("two");
    let d = &ws.0;
    let question = "Which level does research that is clearly harder get, and which Complex level is \"at least Complex\"?";
    let (ok, printed) = json_of(d, &["ask", "--line", "21", "--line", "51", question]);
    assert!(ok, "{printed:#}");
    let outcome: Value =
        serde_json::from_slice(&fs::read(d.join("out/outcome.json")).unwrap()).unwrap();
    assert_eq!(
        outcome["sentences"],
        json!([{"line": 21, "text": LINE_21}, {"line": 51, "text": LINE_51}])
    );
    let before = fs::read(d.join("out/outcome.json")).unwrap();
    fs::remove_file(d.join("out/outcome.json")).unwrap();
    // A line past the end is refused, and nothing is written.
    let (ok, refused) = json_of(d, &["ask", "--line", "21", "--line", "61", question]);
    assert!(!ok);
    let error = refused["error"].as_str().unwrap();
    assert!(error.contains("61") && error.contains("60"), "{error}");
    assert!(!d.join("out/outcome.json").exists());
    let _ = before;
    // A sentence over lines 21-22, with another; recorded in document order.
    let (ok, printed) = json_of(d, &["ask", "--line", "51", "--lines", "21-22", question]);
    assert!(ok, "{printed:#}");
    let outcome: Value =
        serde_json::from_slice(&fs::read(d.join("out/outcome.json")).unwrap()).unwrap();
    assert_eq!(
        outcome["sentences"],
        json!([{"line": 21, "end_line": 22, "text": format!("{LINE_21}\n{LINE_22}")},
               {"line": 51, "text": LINE_51}])
    );
    fs::remove_file(d.join("out/outcome.json")).unwrap();
    for (args, says) in [
        (&["ask", "--lines", "41-50", question][..], "8 lines"),
        (&["ask", "--lines", "51-53", question], "blank"),
        (&["ask", "--lines", "22-21", question], "A-B"),
        (&["ask", "--lines", "59-61", question], "1 to 60"),
        (
            &["ask", "--line", "21", "--lines", "21-22", question],
            "overlap",
        ),
        (&["ask", "--lines", "21", question], "A-B"),
    ] {
        let (ok, refused) = json_of(d, args);
        assert!(!ok, "{args:?}");
        let error = refused["error"].as_str().unwrap();
        assert!(error.contains(says), "{args:?}: {error}");
    }
    assert!(!d.join("out/outcome.json").exists());
    // At most 16 sentences.
    let mut args = vec!["ask"];
    let lines: Vec<String> = (0..17).map(|i| (1 + 2 * i).to_string()).collect();
    for line in &lines {
        args.extend(["--line", line.as_str()]);
    }
    args.push(question);
    let (ok, refused) = json_of(d, &args);
    assert!(!ok);
    assert!(
        refused["error"].as_str().unwrap().contains("16"),
        "{refused:#}"
    );
}

#[test]
fn commands_list_defaults_to_the_workspace_catalog() {
    let ws = workspace("catalog");
    let d = &ws.0;
    let (ok, names) = json_of(d, &["commands", "list"]);
    assert!(ok);
    assert_eq!(
        names,
        json!([
            "judgment.assess",
            "operation.get",
            "work.read",
            "work.route"
        ])
    );
    let (ok, events) = json_of(d, &["events", "list"]);
    assert!(ok);
    assert_eq!(
        events,
        json!(["aq.judgment.assessed", "aq.work.route_needed"])
    );
    let (ok, route) = json_of(d, &["commands", "describe", "work.route"]);
    assert!(ok);
    assert_eq!(route["name"], "work.route");
    // Outside a workspace the synthetic 1.0.0 catalog is still the default.
    let elsewhere = temp("elsewhere");
    let (ok, names) = json_of(&elsewhere.0, &["commands", "list"]);
    assert!(ok);
    assert_eq!(names.as_array().unwrap().len(), 12);
}

/// Binding decision (agent mode security review): only relative paths that
/// stay inside the workspace; no symbolic or hard links; only in the
/// workspace directory itself.
#[test]
fn the_tools_accept_only_paths_inside_the_workspace() {
    let ws = workspace("paths");
    let d = &ws.0;
    let outside = temp("outside");
    write(
        &outside.0,
        "case.json",
        &routing_case("x", "chore", "low", "simple-high"),
    );
    let absolute = outside.0.join("case.json");
    for path in [
        absolute.to_str().unwrap(),
        "../outside/case.json",
        "cases/../../x.json",
    ] {
        let (ok, refused) = json_of(d, &["try", path]);
        assert!(!ok, "{path}");
        assert!(
            refused["error"]
                .as_str()
                .unwrap()
                .contains("inside the workspace"),
            "{path}: {refused:#}"
        );
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&absolute, d.join("cases/linked.json")).unwrap();
        let (ok, refused) = json_of(d, &["try", "cases/linked.json"]);
        assert!(!ok);
        assert!(
            refused["error"].as_str().unwrap().contains("symbolic link"),
            "{refused:#}"
        );
        // save reads every case, and refuses the link too.
        let (ok, refused) = json_of(d, &["save"]);
        assert!(!ok);
        assert!(refused.to_string().contains("symbolic link"), "{refused:#}");
        fs::remove_file(d.join("cases/linked.json")).unwrap();
        fs::hard_link(d.join("cases/chore.json"), d.join("hard.json")).unwrap();
        let (ok, refused) = json_of(d, &["try", "hard.json"]);
        assert!(!ok);
        assert!(
            refused["error"].as_str().unwrap().contains("hard link"),
            "{refused:#}"
        );
        fs::remove_file(d.join("hard.json")).unwrap();
        std::os::unix::fs::symlink(&outside.0, d.join("out")).unwrap();
        let (ok, refused) = json_of(d, &["ask", "--line", "21", "Which level?"]);
        assert!(!ok, "{refused:#}");
        assert!(!outside.0.join("outcome.json").exists());
        fs::remove_file(d.join("out")).unwrap();
    }
    // Discovery in a subdirectory of a workspace is refused too.
    let (ok, refused) = json_of(&d.join("cases"), &["commands", "list"]);
    assert!(!ok);
    assert!(
        refused["error"]
            .as_str()
            .unwrap()
            .contains("workspace directory itself"),
        "{refused:#}"
    );
    // Not in a workspace, or in a subdirectory of one: refused.
    for dir in [outside.0.clone(), d.join("cases")] {
        for args in [
            &["check"][..],
            &["save"],
            &["ask", "--line", "1", "Why?"],
            &["facts"],
        ] {
            let (ok, refused) = json_of(&dir, args);
            assert!(!ok, "{args:?}");
            assert!(
                refused["error"].as_str().unwrap().contains("workspace"),
                "{args:?}: {refused:#}"
            );
        }
    }
}

/// Argument errors are JSON on stdout too, with exit status 2 (1 is a
/// refusal of the command itself).
#[test]
fn argument_errors_print_json_and_exit_2() {
    let ws = workspace("usage");
    let d = &ws.0;
    for args in [
        &["ask", "--line", "abc", "Why?"][..],
        &["ask", "Why?", "--frobnicate"],
        &["tri", "cases/chore.json"],
    ] {
        let out = run(d, args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        let printed: Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|e| panic!("{args:?}: {e}: {}", String::from_utf8_lossy(&out.stdout)));
        assert!(
            printed["error"].as_str().unwrap().starts_with("error:"),
            "{printed:#}"
        );
    }
    // Help is still help.
    let out = run(d, &["ask", "--help"]);
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("--lines"));
}

#[test]
fn a_cases_entry_that_is_not_a_directory_is_reported() {
    let ws = workspace("not-a-dir");
    let d = &ws.0;
    fs::remove_dir_all(d.join("cases")).unwrap();
    fs::write(d.join("cases"), "not a directory").unwrap();
    let (ok, refused) = json_of(d, &["save"]);
    assert!(!ok);
    assert!(
        refused["reasons"]
            .to_string()
            .contains("cases is not a directory"),
        "{refused:#}"
    );
    fs::remove_file(d.join("cases")).unwrap();
    let (ok, refused) = json_of(d, &["save"]);
    assert!(!ok);
    assert!(
        refused["reasons"]
            .to_string()
            .contains("holds 0 case files"),
        "{refused:#}"
    );
}

/// The largest save an agent can make for the routing document: the
/// routing draft and 40 cases, each spelling out every call with its whole
/// input, fits the outcome bound.
#[test]
fn a_forty_case_routing_save_fits_the_outcome_bound() {
    let ws = workspace("forty");
    let d = &ws.0;
    fs::remove_dir_all(d.join("cases")).unwrap();
    let rows = [
        ("chore", "low", "simple-high"),
        ("feature", "low", "standard-high"),
        ("design", "low", "complex-high"),
        ("bugfix", "medium", "standard-high"),
        ("docs", "low", "standard-high"),
    ];
    for n in 0..MAX_CASES {
        let (kind, risk, level) = rows[n % rows.len()];
        let name = format!("case-{n:02}");
        write(
            d,
            &format!("cases/{name}.json"),
            &routing_case(&name, kind, risk, level),
        );
    }
    let (ok, saved) = json_of(d, &["save"]);
    assert!(ok, "{saved:#}");
    assert_eq!(saved["cases"].as_array().unwrap().len(), MAX_CASES);
    let bytes = fs::read(d.join("out/outcome.json")).unwrap().len();
    println!("40 routing cases and the draft: {bytes} bytes");
    assert!(bytes <= MAX_OUTCOME_BYTES, "{bytes}");
}

/// A quote holds whole sentences: a line whose sentence goes on, or starts
/// on the line before, is refused with the range to name instead (review
/// Minor 3).
#[test]
fn a_quote_must_hold_whole_sentences() {
    let ws = workspace("whole");
    let d = &ws.0;
    let text = "# Title\n\nWhen a task is made, send its\nowner a note. Then stop.\n\n- A list item.\n- Another.\n";
    fs::write(d.join("source.md"), text).unwrap();
    for (args, says) in [
        (
            &["ask", "--line", "3", "Who?"][..],
            "the sentence goes on in line 4. Name the whole sentence, to its last line, such as --lines 3-4.",
        ),
        (
            &["ask", "--line", "4", "Who?"],
            "the sentence starts on line 3, which it goes on from. Name the whole sentence, from its first line, such as --lines 3-4.",
        ),
        (
            &["unable", "--line", "3", "No."],
            "the sentence goes on in line 4",
        ),
    ] {
        let (ok, refused) = json_of(d, args);
        assert!(!ok, "{args:?}");
        let error = refused["error"].as_str().unwrap();
        assert!(error.contains(says), "{args:?}: {error}");
    }
    assert!(!d.join("out/outcome.json").exists());
    // List items are sentences of their own.
    let (ok, printed) = json_of(d, &["ask", "--lines", "3-4", "--line", "6", "Who?"]);
    assert!(ok, "{printed:#}");
}

/// The range a refusal suggests is itself accepted, however many lines the
/// sentence has and whichever of its lines was named (fix round 2, N2).
#[test]
fn the_suggested_range_is_accepted() {
    let ws = workspace("suggested");
    let d = &ws.0;
    let text = "# Title\n\nWhen a task is made, send its\nowner a note and\nthen stop.\n\nDone.\n";
    fs::write(d.join("source.md"), text).unwrap();
    for args in [
        &["ask", "--line", "3", "Who?"][..],
        &["ask", "--line", "4", "Who?"],
        &["ask", "--line", "5", "Who?"],
        &["ask", "--lines", "3-4", "Who?"],
        &["ask", "--lines", "4-5", "Who?"],
    ] {
        let (ok, refused) = json_of(d, args);
        assert!(!ok, "{args:?}");
        let error = refused["error"].as_str().unwrap();
        assert!(error.contains("such as --lines 3-5."), "{args:?}: {error}");
    }
    let (ok, printed) = json_of(d, &["ask", "--lines", "3-5", "Who?"]);
    assert!(ok, "{printed:#}");
}
