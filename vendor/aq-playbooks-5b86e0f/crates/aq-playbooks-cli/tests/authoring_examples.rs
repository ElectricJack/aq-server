//! The worked authoring examples (`crates/aq-playbooks-compiler/
//! authoring-examples`), which an authoring agent reads and imitates. Each one is written out by
//! `aq-playbooks examples` and run with the real tools from its root, as an
//! agent runs them: `check` is clean, every case passes under `try`, `seal`
//! writes an artifact that verifies, `compare` reports the kind of change the
//! example shows, and `save` writes the outcome. Each document variant under
//! `stuck/` writes exactly the outcome it shows. So the examples cannot rot.
use aq_playbooks_cli::catalog::catalog_v2_2;
use aq_playbooks_compiler::docs::example_files;
use aq_playbooks_compiler::draft::{SourceRecord, retain_registry};
use aq_playbooks_model::{Artifact, Registry};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{SystemTime, UNIX_EPOCH},
};

/// Each example, the change `compare` reports against its
/// `previous.artifact.json`, and the most steps its draft has.
const EXAMPLES: [(&str, &str, usize); 2] = [
    ("escalate-on-judgment", "presentation", 13),
    ("notify-when-ready", "executable", 10),
];
const ON_DISK: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../aq-playbooks-compiler/authoring-examples"
);

struct Dir(PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn temp(name: &str) -> Dir {
    let dir = std::env::temp_dir().join(format!(
        "aq-playbooks-examples-{name}-{}-{}",
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
/// Runs the command in `dir`, asserts that it succeeded and returns what it
/// printed.
fn ok(dir: &Path, args: &[&str]) -> Value {
    let out = run(dir, args);
    let printed = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{} {args:?} failed:\n{printed}\n{}",
        dir.display(),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout)
        .unwrap_or_else(|e| panic!("{args:?} printed no JSON ({e}): {printed}"))
}
/// Writes every example with `aq-playbooks examples` and returns the directory.
fn written(name: &str) -> Dir {
    let dir = temp(name);
    let printed = ok(&dir.0, &["examples", "--output", "."]);
    let names: Vec<&str> = EXAMPLES.iter().map(|(name, _, _)| *name).collect();
    assert_eq!(printed["examples"], serde_json::json!(names));
    dir
}
fn json_file(path: &Path) -> Value {
    serde_json::from_slice(&fs::read(path).unwrap())
        .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[test]
fn every_worked_example_checks_saves_and_its_cases_pass() {
    let dir = written("tools");
    for (name, change, max_steps) in EXAMPLES {
        let root = dir.0.join(name);
        let checked = ok(&root, &["check"]);
        assert_eq!(checked["ok"], true, "{name}: {checked:#}");
        assert_eq!(checked["diagnostics"], serde_json::json!([]), "{name}");
        let steps = json_file(&root.join("draft.json"))["steps"]
            .as_object()
            .unwrap()
            .len();
        assert!(steps <= max_steps, "{name} has {steps} steps");

        // The catalog is the one the host places: catalog 2.2.0 kept to what
        // the record allows.
        let record: SourceRecord =
            serde_json::from_value(json_file(&root.join("record.json"))).unwrap();
        let catalog: Registry =
            serde_json::from_value(json_file(&root.join("catalog.json"))).unwrap();
        let (kept, _) = retain_registry(&catalog_v2_2(), &record.commands, &record.events).unwrap();
        assert_eq!(
            catalog, kept,
            "{name}: catalog.json is not catalog 2.2.0 as the record keeps it"
        );

        // Every case passes, and is named after its file.
        let mut cases: Vec<String> = fs::read_dir(root.join("cases"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        cases.sort();
        assert!(cases.len() >= 5, "{name} has {} cases", cases.len());
        for file in &cases {
            let report = ok(&root, &["try", &format!("cases/{file}")]);
            assert_eq!(report["passed"], true, "{name} {file}: {report:#}");
            // A case lists every call the run makes, in order, each with
            // its whole input, and gives no return the run does not use.
            let case = json_file(&root.join("cases").join(file));
            let made: Vec<Value> = report["calls"]
                .as_array()
                .unwrap()
                .iter()
                .map(|c| serde_json::json!({"command": c["command"], "input": c["input"]}))
                .collect();
            assert_eq!(
                Value::Array(made.clone()),
                case["expect"]["calls"],
                "{name} {file}"
            );
            for (command, returns) in case["returns"].as_object().unwrap() {
                let used = made.iter().filter(|c| c["command"] == *command).count();
                assert_eq!(
                    used,
                    returns.as_array().unwrap().len(),
                    "{name} {file}: {command}"
                );
            }
            assert_eq!(
                format!("{}.json", report["name"].as_str().unwrap()),
                *file,
                "{name}: a case is named after its file"
            );
        }

        // The sealed artifact verifies, and is the one save records.
        let sealed = ok(&root, &["seal", "--output", "sealed.artifact.json"]);
        let bytes = fs::read(root.join("sealed.artifact.json")).unwrap();
        let artifact = Artifact::from_canonical_bytes(&bytes).unwrap();
        artifact.verify().unwrap();
        assert_eq!(sealed["artifact"], artifact.digest.as_str(), "{name}");

        let compared = ok(&root, &["compare"]);
        assert_eq!(compared["change"], change, "{name}: {compared:#}");
        assert_eq!(compared["abilities_changed"], false, "{name}");
        assert_eq!(compared["diff"]["source_changed"], true, "{name}");

        let saved = ok(&root, &["save"]);
        assert_eq!(saved["saved"], true, "{name}: {saved:#}");
        assert_eq!(saved["artifact"], artifact.digest.as_str(), "{name}");
        let outcome = json_file(&root.join("out/outcome.json"));
        assert_eq!(outcome["kind"], "saved", "{name}");
        assert_eq!(outcome["artifact"], artifact.digest.as_str(), "{name}");
        assert_eq!(outcome["cases"].as_array().unwrap().len(), cases.len());
    }
}

/// `stuck/<name>.json` shows a document the agent cannot turn into steps
/// (`stuck/<name>.md`), the exact command the agent runs, and the outcome it
/// writes. Run on a workspace holding that document, it writes exactly that.
#[test]
fn each_stuck_document_writes_the_outcome_it_shows() {
    let mut shown = BTreeSet::new();
    for (name, _, _) in EXAMPLES {
        let stuck: Vec<&str> = example_files()
            .into_iter()
            .map(|(path, _)| path)
            .filter(|path| path.starts_with(&format!("{name}/stuck/")) && path.ends_with(".json"))
            .collect();
        assert!(!stuck.is_empty(), "{name} shows no stuck document");
        for path in stuck {
            let dir = written("stuck");
            let root = dir.0.join(name);
            let example = json_file(&dir.0.join(path));
            let document = example["document"].as_str().unwrap();
            fs::copy(root.join(document), root.join("source.md")).unwrap();
            let args: Vec<&str> = example["run"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap())
                .collect();
            shown.insert(args[0].to_string());
            let printed = ok(&root, &args);
            assert_eq!(printed["outcome"], example["writes"], "{path}");
            assert_eq!(
                json_file(&root.join("out/outcome.json")),
                example["writes"],
                "{path}"
            );
            assert!(!example["why"].as_str().unwrap().is_empty(), "{path}");
        }
    }
    assert_eq!(shown, BTreeSet::from(["ask".into(), "unable".into()]));
}

/// `example_files()` is every file under `crates/aq-playbooks-compiler/authoring-examples`, byte for
/// byte, and the documents are written as a person writes them: no
/// identifiers, no code.
#[test]
fn example_files_are_the_examples_on_disk() {
    fn walk(root: &Path, dir: &Path, found: &mut BTreeMap<String, String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(root, &path, found);
            } else {
                let relative = path.strip_prefix(root).unwrap().to_str().unwrap();
                found.insert(relative.into(), fs::read_to_string(&path).unwrap());
            }
        }
    }
    let mut on_disk = BTreeMap::new();
    walk(Path::new(ON_DISK), Path::new(ON_DISK), &mut on_disk);
    let listed: BTreeMap<String, String> = example_files()
        .into_iter()
        .map(|(path, text)| (path.to_string(), text.to_string()))
        .collect();
    assert_eq!(
        listed.keys().collect::<Vec<_>>(),
        on_disk.keys().collect::<Vec<_>>()
    );
    assert_eq!(listed, on_disk);
    for (path, text) in &listed {
        // The owner's documents (each example's source.md and its stuck
        // variants), not the host's CATALOG.md.
        if path.ends_with(".md") && !path.ends_with("/CATALOG.md") {
            for code in ["`", "work.", "judgment.", "message.", "aq.", "_"] {
                assert!(!text.contains(code), "{path} contains {code:?}");
            }
        }
    }
}

/// A fenced block of `GUIDE.md` or `LANGUAGE.md`: its info words, its text
/// (fence indentation removed) and the line it starts on.
struct Block {
    info: Vec<String>,
    body: String,
    line: usize,
}
fn blocks(doc: &str) -> Vec<Block> {
    let mut found = vec![];
    let mut open: Option<(usize, Vec<String>, usize, Vec<String>)> = None;
    for (i, line) in doc.lines().enumerate() {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        match (&mut open, trimmed.strip_prefix("```")) {
            (None, Some(info)) => {
                let info = info.split_whitespace().map(String::from).collect();
                open = Some((indent, info, i + 1, vec![]));
            }
            (Some(_), Some("")) => {
                let (_, info, line, body) = open.take().unwrap();
                found.push(Block {
                    info,
                    body: body.join("\n"),
                    line,
                });
            }
            (Some((indent, _, _, body)), _) => {
                let cut = line.len() - line.trim_start().len();
                body.push(line[cut.min(*indent)..].to_string());
            }
            (None, None) => {}
        }
    }
    found
}
fn role(block: &Block) -> (&str, &str) {
    (
        block.info.first().map_or("", String::as_str),
        block.info.get(1).map_or("", String::as_str),
    )
}
fn option<'b>(block: &'b Block, key: &str) -> Option<&'b str> {
    block
        .info
        .iter()
        .find_map(|word| word.strip_prefix(key)?.strip_prefix('='))
}
/// `want` is in `got`: objects by their listed fields, arrays item by item,
/// and a string with `…` as its pieces in order.
fn fits(want: &Value, got: &Value) -> bool {
    match (want, got) {
        (Value::Object(w), Value::Object(g)) => {
            w.iter().all(|(k, v)| g.get(k).is_some_and(|g| fits(v, g)))
        }
        (Value::Array(w), Value::Array(g)) => {
            w.len() == g.len() && w.iter().zip(g).all(|(a, b)| fits(a, b))
        }
        (Value::String(w), Value::String(g)) if w.contains('…') => {
            let mut rest = g.as_str();
            w.split('…').all(|piece| match rest.find(piece) {
                Some(at) => {
                    rest = &rest[at + piece.len()..];
                    true
                }
                None => false,
            })
        }
        _ => want == got,
    }
}
/// The words of a command line; `"…"` quotes one word.
fn words(line: &str) -> Vec<String> {
    let mut words = vec![];
    let mut word = String::new();
    let mut quoted = false;
    let mut any = false;
    for c in line.chars() {
        match c {
            '"' => {
                quoted = !quoted;
                any = true;
            }
            ' ' if !quoted => {
                if any || !word.is_empty() {
                    words.push(std::mem::take(&mut word));
                }
                any = false;
            }
            c => word.push(c),
        }
    }
    if any || !word.is_empty() {
        words.push(word);
    }
    assert!(!quoted, "an unclosed quote in {line:?}");
    words
}
const DOCUMENTS: [(&str, &str); 2] = [
    ("GUIDE.md", aq_playbooks_compiler::docs::GUIDE),
    ("LANGUAGE.md", aq_playbooks_compiler::docs::LANGUAGE),
];

/// Every command the two documents show runs in the worked example it names
/// and prints what the document says it prints; every case they show runs
/// under `try` as the document says; every case fragment parses; and every
/// `aq-playbooks …` the prose names is one of the commands run.
#[test]
fn every_command_and_case_the_documents_show_runs_as_shown() {
    use aq_playbooks_simulation::{CaseReturn, CaseStart};
    let mut run_lines: Vec<Vec<String>> = vec![];
    let mut shown = 0;
    for (name, doc) in DOCUMENTS {
        let found = blocks(doc);
        for (i, block) in found.iter().enumerate() {
            let at = format!("{name} line {}", block.line);
            let next = found.get(i + 1);
            let expected = |r: &str| {
                next.filter(|b| role(b) == ("json", r)).map(|b| -> Value {
                    serde_json::from_str(&b.body).unwrap_or_else(|e| panic!("{at}: {e}"))
                })
            };
            match role(block) {
                ("sh", example) => {
                    let dir = written("docs");
                    let root = dir.0.join(example);
                    if let Some(document) = option(block, "doc") {
                        fs::copy(root.join(document), root.join("source.md")).unwrap();
                    }
                    let cwd = option(block, "in").map_or(root.clone(), |sub| root.join(sub));
                    let lines: Vec<&str> = block
                        .body
                        .lines()
                        .filter(|l| !l.trim().is_empty())
                        .collect();
                    let want = expected("output");
                    for (k, line) in lines.iter().enumerate() {
                        let args = words(line);
                        assert_eq!(args[0], "aq-playbooks", "{at}: {line}");
                        let args: Vec<&str> = args[1..].iter().map(String::as_str).collect();
                        let out = run(&cwd, &args);
                        let printed: Value = serde_json::from_slice(&out.stdout)
                            .unwrap_or_else(|e| panic!("{at}: {line} printed no JSON ({e})"));
                        match (&want, k + 1 == lines.len()) {
                            (Some(want), true) => assert!(
                                fits(want, &printed),
                                "{at}: {line} printed {printed:#}, not {want:#}"
                            ),
                            _ => assert!(out.status.success(), "{at}: {line} failed: {printed:#}"),
                        }
                        run_lines.push(words(line));
                    }
                    shown += 1;
                }
                ("json", "case") => {
                    let example = block
                        .info
                        .get(2)
                        .unwrap_or_else(|| panic!("{at}: name the example"));
                    let case: Value =
                        serde_json::from_str(&block.body).unwrap_or_else(|e| panic!("{at}: {e}"));
                    let dir = written("docs-case");
                    let root = dir.0.join(example);
                    let file = format!("cases/{}.json", case["name"].as_str().unwrap());
                    fs::write(root.join(&file), &block.body).unwrap();
                    let out = run(&root, &["try", &file]);
                    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
                    match expected("report") {
                        Some(want) => assert!(fits(&want, &report), "{at}: try reports {report:#}"),
                        None => assert_eq!(report["passed"], true, "{at}: {report:#}"),
                    }
                    shown += 1;
                }
                ("json", "start") => {
                    serde_json::from_str::<CaseStart>(&block.body)
                        .unwrap_or_else(|e| panic!("{at}: {e}"));
                }
                ("json", "returns") => {
                    serde_json::from_str::<BTreeMap<String, Vec<CaseReturn>>>(&block.body)
                        .unwrap_or_else(|e| panic!("{at}: {e}"));
                }
                // An output or a report shows what the block before it
                // printed, so it must follow its command.
                ("json", "output") => {
                    let before = i.checked_sub(1).map(|j| role(&found[j]).0);
                    assert_eq!(before, Some("sh"), "{at}: an output follows its command");
                }
                ("json", "report") => {
                    let before = i.checked_sub(1).map(|j| role(&found[j]));
                    assert_eq!(
                        before,
                        Some(("json", "case")),
                        "{at}: a report follows its case"
                    );
                }
                ("json", _) | ("", _) => {}
                (other, _) => panic!("{at}: a {other} block is not run by any test"),
            }
        }
    }
    assert!(shown >= 12, "only {shown} commands and cases were run");
    // Every `aq-playbooks …` named in the text is a command run above, with
    // `<…>` standing for any one word, or the first words of one.
    for (name, doc) in DOCUMENTS {
        let mut fenced = false;
        for line in doc.lines() {
            if line.trim_start().starts_with("```") {
                fenced = !fenced;
                continue;
            }
            if fenced {
                continue;
            }
            for span in line.split('`').skip(1).step_by(2) {
                let Some(rest) = span.strip_prefix("aq-playbooks ") else {
                    continue;
                };
                let named = words(rest);
                let matches = run_lines.iter().any(|ran| {
                    ran.len() > named.len()
                        && named
                            .iter()
                            .zip(&ran[1..])
                            .all(|(n, r)| (n.starts_with('<') && n.ends_with('>')) || n == r)
                });
                assert!(
                    matches,
                    "{name}: `{span}` is not a command the documents run"
                );
            }
        }
    }
}

/// `commands describe` and `events describe` carry the same notes as the
/// workspace's `CATALOG.md`, and `CATALOG.md` lists exactly the contract's
/// input fields and outcomes that `describe` shows.
#[test]
fn describe_carries_the_notes_catalog_md_renders() {
    let dir = written("describe");
    for (name, _, _) in EXAMPLES {
        let root = dir.0.join(name);
        let catalog = fs::read_to_string(root.join("CATALOG.md")).unwrap();
        for (kind, list) in [("commands", "commands"), ("events", "events")] {
            let names = ok(&root, &[list, "list"]);
            for contract in names.as_array().unwrap() {
                let contract = contract.as_str().unwrap();
                let described = ok(&root, &[list, "describe", contract]);
                assert_eq!(
                    described["notes"],
                    aq_playbooks_compiler::docs::notes(kind, contract).unwrap(),
                    "{name}: {contract}"
                );
                let section = catalog
                    .split(&format!("\n### `{contract}` "))
                    .nth(1)
                    .unwrap_or_else(|| panic!("{name}: CATALOG.md has no {contract}"))
                    .split("\n### ")
                    .next()
                    .unwrap();
                let rows = |schema: &Value| -> Vec<String> {
                    schema["properties"]
                        .as_object()
                        .map(|m| m.keys().cloned().collect())
                        .unwrap_or_default()
                };
                let fields = if kind == "commands" {
                    rows(&described["input"])
                } else {
                    rows(&described["payload"])
                };
                for field in fields {
                    assert!(
                        section.contains(&format!("| `{field}` |")),
                        "{name}: {contract}.{field}"
                    );
                }
                if let Some(outcomes) = described["outcomes"].as_object() {
                    for outcome in outcomes.keys() {
                        assert!(
                            section.contains(&format!("| `{outcome}` |")),
                            "{name}: {contract} {outcome}"
                        );
                    }
                }
            }
        }
    }
}

/// A run that stops on the playbook's own duration limit says so under
/// `try` (here a wait inside a loop body, which `check` does not add up).
#[test]
fn a_run_that_stops_on_its_duration_limit_says_so() {
    let dir = written("duration");
    let root = dir.0.join("notify-when-ready");
    let mut draft = json_file(&root.join("draft.json"));
    draft["steps"]["wait"]["op"]["deadline_ms"] = serde_json::json!(86_400_000);
    draft["steps"]["wait"]["edges"]["timeout"] = serde_json::json!("each");
    draft["steps"]["each"] = serde_json::json!({"op": {"kind": "for_each",
        "collection": {"op": "literal", "value": [1]}, "item": "n", "body": "wait",
        "next": "read", "max_items": 1, "concurrency": 1, "on_failure": "stop"},
        "edges": {}, "on_error": "failed"});
    draft["steps"]["ready"]["edges"]["true"] = serde_json::json!("each");
    fs::write(
        root.join("draft.json"),
        serde_json::to_vec_pretty(&draft).unwrap(),
    )
    .unwrap();
    assert_eq!(ok(&root, &["check"])["ok"], true);
    let out = run(&root, &["try", "cases/still-no-description.json"]);
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(report["passed"], false);
    assert!(
        report["problems"][0]
            .as_str()
            .unwrap()
            .starts_with("the run stopped at step wait on the playbook's own duration limit: it had run 86400000 ms, and `limits.max_duration_ms` is 86400000."),
        "{report:#}"
    );
}

/// An asynchronous command can answer at once with one of its own outcomes,
/// and the invoke's own edge is taken; a synchronous command cannot be
/// given `at_once`.
#[test]
fn an_answer_at_once_takes_the_invokes_own_edge() {
    let dir = written("at-once");
    let root = dir.0.join("escalate-on-judgment");
    let report = ok(&root, &["try", "cases/judgment-refused-at-once.json"]);
    let steps: Vec<(&str, &str)> = report["calls"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| (c["step"].as_str().unwrap(), c["outcome"].as_str().unwrap()))
        .collect();
    assert_eq!(
        steps,
        [
            ("read", "observed"),
            ("ask", "refused"),
            ("assume", "routed")
        ]
    );
    let notify = dir.0.join("notify-when-ready");
    let mut case = json_file(&notify.join("cases/task-gone.json"));
    case["returns"]["work.read"][0]["at_once"] = serde_json::json!(true);
    fs::write(
        notify.join("cases/x.json"),
        serde_json::to_vec(&case).unwrap(),
    )
    .unwrap();
    let out = run(&notify, &["try", "cases/x.json"]);
    let report: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        report["problems"][0]
            .as_str()
            .unwrap()
            .contains("work.read always answers at once; leave at_once out."),
        "{report:#}"
    );
}
