//! The documents an authoring agent works from (`docs::GUIDE`,
//! `docs::LANGUAGE`): every draft snippet they show is checked here, with
//! the diagnostic they say it gets; every expression and step parses; every
//! diagnostic code has an entry; and what the guide must teach is in it.
//! The commands and cases the documents show are run by the CLI's
//! `authoring_examples` test, with the real binary.
use aq_playbooks_compiler::Diagnostic;
use aq_playbooks_compiler::docs::{GUIDE, LANGUAGE, catalog, catalog_registry, example_files};
use aq_playbooks_compiler::draft::{self, MAX_DRAFT_JSON_BYTES, MAX_DRAFT_STEPS, Workspace};
use aq_playbooks_model::{Expr, MAX_DRAFT_EXPANSION, MAX_DRAFT_EXPANSION_TOTAL, MAX_DRAFT_SCOPE};
use serde_json::{Value, json};

const DOCUMENTS: [(&str, &str); 2] = [("GUIDE.md", GUIDE), ("LANGUAGE.md", LANGUAGE)];
const EXAMPLES: [&str; 2] = ["escalate-on-judgment", "notify-when-ready"];
/// Where the codes come from: every `issue(…, "<code>", …)` and
/// `Diagnostic::error("<code>", …)` call.
const SOURCES: [&str; 4] = [
    include_str!("../../aq-playbooks-model/src/lib.rs"),
    include_str!("../../aq-playbooks-model/src/draft.rs"),
    include_str!("../src/draft.rs"),
    include_str!("../src/lib.rs"),
];
/// Codes built with `format!`, which the scan cannot read.
const FORMATTED_CODES: [&str; 7] = [
    "unknown_command",
    "unknown_event",
    "draft_format",
    "record_format",
    "catalog_format",
    "extensions_format",
    "facts_format",
];
const CLI_AUTHORING: &str = include_str!("../../aq-playbooks-cli/src/authoring.rs");

/// A fenced block: its info words, its text (fence indentation removed), the
/// line it starts on and the `## ` section it is in.
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
                let (_, info, start, body) = open.take().unwrap();
                found.push(Block {
                    info,
                    body: body.join("\n"),
                    line: start,
                });
            }
            (Some((indent, _, _, body)), _) => {
                let cut = line.len() - line.trim_start().len();
                body.push(line[cut.min(*indent)..].to_string());
            }
            (None, None) => {}
        }
    }
    assert!(open.is_none(), "an unclosed fence");
    found
}
/// Lines outside fenced blocks, each with the `## ` section it is in.
fn prose(doc: &str) -> Vec<(String, String)> {
    let mut section = String::new();
    let mut fenced = false;
    let mut lines = vec![];
    for line in doc.lines() {
        if line.trim_start().starts_with("```") {
            fenced = !fenced;
            continue;
        }
        if fenced {
            continue;
        }
        if let Some(heading) = line.strip_prefix("## ") {
            section = heading.to_string();
        }
        lines.push((section.clone(), line.to_string()));
    }
    lines
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

fn example_file(example: &str, name: &str) -> &'static str {
    let path = format!("{example}/{name}");
    example_files()
        .into_iter()
        .find(|(p, _)| *p == path)
        .unwrap_or_else(|| panic!("no example file {path}"))
        .1
}
fn workspace(example: &str) -> Workspace {
    let parse =
        |name: &str| -> Value { serde_json::from_str(example_file(example, name)).unwrap() };
    Workspace {
        markdown: example_file(example, "source.md").to_string(),
        record: serde_json::from_value(parse("record.json")).unwrap(),
        catalog: serde_json::from_value(parse("catalog.json")).unwrap(),
        facts: serde_json::from_value(parse("facts.json")).unwrap(),
        draft: parse("draft.json"),
    }
}
/// Puts a snippet into a draft: each of its `steps` replaces or adds that
/// step, and any other field replaces the draft's.
fn patch(draft: &mut Value, snippet: &Value) {
    for (key, value) in snippet.as_object().expect("a draft snippet is an object") {
        if key == "steps" {
            for (name, step) in value.as_object().unwrap() {
                draft["steps"][name] = step.clone();
            }
        } else {
            draft[key] = value.clone();
        }
    }
}
fn diagnostics(ws: &Workspace) -> Vec<Value> {
    let found: Vec<Diagnostic> = draft::check(ws);
    found
        .iter()
        .map(|d| serde_json::to_value(d).unwrap())
        .collect()
}

fn reference(root: &str, path: &[&str]) -> Value {
    json!({"op": "ref", "root": root, "path": path})
}
/// `b0 = {k: input}`, `bj = {a: b(j-1), b: b(j-1)}`: a value that doubles at
/// every level.
fn doubling(depth: usize, prefix: &str) -> Value {
    let mut bindings = vec![
        json!({"name": format!("{prefix}0"), "value": {"op": "object", "fields": {"k": reference("input", &[])}}}),
    ];
    for j in 1..depth {
        let before = format!("{prefix}{}", j - 1);
        bindings.push(
            json!({"name": format!("{prefix}{j}"), "value": {"op": "object",
            "fields": {"a": reference(&before, &[]), "b": reference(&before, &[])}}}),
        );
    }
    json!({"op": "let", "bindings": bindings,
           "body": {"op": "exists", "value": reference(&format!("{prefix}{}", depth - 1), &[])}})
}
fn decide(predicate: Value, next: &str) -> Value {
    json!({"op": {"kind": "decide", "predicate": predicate},
           "edges": {"true": next, "false": next, "unknown": "failed"}, "on_error": null})
}
/// The notify-when-ready workspace made to pass one bound the documents
/// name.
fn past_the_bound(limit: &str) -> Workspace {
    let mut ws = workspace("notify-when-ready");
    let steps = ws.draft["steps"].as_object_mut().unwrap();
    match limit {
        "steps" => {
            let ret = steps["failed"].clone();
            let missing = MAX_DRAFT_STEPS + 1 - steps.len();
            for i in 0..missing {
                steps.insert(format!("r{i}"), ret.clone());
            }
        }
        "bytes" => {
            ws.draft["purpose"] = json!("x".repeat(MAX_DRAFT_JSON_BYTES));
        }
        "expression" => {
            steps["ready"]["op"]["predicate"] = json!({"op": "and", "values": [doubling(16, "b")]});
        }
        "total" => {
            for i in 0..8 {
                let next = if i < 7 {
                    format!("d{}", i + 1)
                } else {
                    "wait".into()
                };
                steps.insert(
                    format!("d{i}"),
                    decide(doubling(14, &format!("x{i}_")), &next),
                );
            }
            steps["ready"]["edges"]["true"] = json!("d0");
        }
        "scope" => {
            for i in 0..200 {
                let next = if i < 199 {
                    format!("d{}", i + 1)
                } else {
                    "wait".into()
                };
                steps.insert(
                    format!("d{i}"),
                    decide(json!({"op": "literal", "value": true}), &next),
                );
            }
            steps["ready"]["edges"]["true"] = json!("d0");
            let input = &mut ws.draft["entries"]["main"]["input"]["properties"];
            for i in 0..1500 {
                input[format!("p{i}")] = json!({"type": "boolean"});
            }
        }
        "shapes" => {
            let rows = vec![json!({"k": "a"}); 12_000];
            let reads: Vec<Value> = (0..100)
                .map(|i| {
                    json!({"op": "map", "collection": reference("t", &[]), "item": format!("r{i}"),
                                "value": reference(&format!("r{i}"), &["k"]), "max_items": 12_000})
                })
                .collect();
            steps["ready"]["op"]["predicate"] = json!({"op": "let",
                "bindings": [{"name": "t", "value": {"op": "literal", "value": rows}}],
                "body": {"op": "exists", "value": {"op": "array", "items": reads}}});
        }
        other => panic!("no such bound in the test: {other}"),
    }
    ws
}

#[test]
fn the_docs_module_returns_the_documents_and_the_examples() {
    assert!(GUIDE.starts_with("---\nname: aq-playbook-author\n"));
    assert!(GUIDE.contains("\n# Authoring guide\n"));
    assert!(LANGUAGE.starts_with("# Language reference\n"));
    // FORMAT.md is the engine's specification and is not shipped: nothing
    // sends the agent to it.
    for (name, doc) in DOCUMENTS {
        assert!(
            !doc.contains("FORMAT.md"),
            "{name} sends the agent to FORMAT.md"
        );
    }
    let files = example_files();
    for example in EXAMPLES {
        for name in [
            "source.md",
            "record.json",
            "catalog.json",
            "facts.json",
            "draft.json",
        ] {
            assert!(
                files.iter().any(|(p, _)| *p == format!("{example}/{name}")),
                "{example}/{name}"
            );
        }
        // Every example checks clean, so a snippet's diagnostic is its own.
        assert_eq!(
            diagnostics(&workspace(example)),
            Vec::<Value>::new(),
            "{example}"
        );
        // Its CATALOG.md is the one the host writes for its record.
        let record = workspace(example).record;
        assert_eq!(
            example_file(example, "CATALOG.md"),
            catalog(&record.commands, &record.events).unwrap(),
            "{example}/CATALOG.md is not docs::catalog of its record"
        );
    }
}

#[test]
fn every_json_block_in_language_md_parses_as_an_expr_or_step() {
    let mut ops = std::collections::BTreeSet::new();
    let mut kinds = std::collections::BTreeSet::new();
    for block in blocks(LANGUAGE) {
        let at = format!("LANGUAGE.md line {}", block.line);
        match role(&block) {
            ("json", "expr") => {
                let expr: Expr = serde_json::from_str(&block.body)
                    .unwrap_or_else(|e| panic!("{at}: not an expression: {e}"));
                collect_ops(&serde_json::to_value(expr).unwrap(), &mut ops);
            }
            ("json", "step") => {
                let step: Value = serde_json::from_str(&block.body).unwrap();
                kinds.insert(step["op"]["kind"].as_str().unwrap().to_string());
                // It fills into one of the examples: it parses, and its names
                // are ones a source may use.
                let fills = EXAMPLES.iter().any(|example| {
                    let mut ws = workspace(example);
                    ws.draft["steps"]["doc_step"] = step.clone();
                    draft::fill(&ws).is_ok()
                });
                assert!(fills, "{at}: does not fill into an example draft");
            }
            _ => {}
        }
    }
    let every_op = [
        "literal",
        "ref",
        "object",
        "array",
        "exists",
        "not",
        "and",
        "or",
        "eq",
        "lt",
        "contains",
        "map",
        "filter",
        "difference",
        "lookup",
        "let",
    ];
    for op in every_op {
        assert!(ops.contains(op), "LANGUAGE.md shows no `{op}` expression");
    }
    for kind in ["invoke", "decide", "wait", "for_each", "join", "return"] {
        assert!(kinds.contains(kind), "LANGUAGE.md shows no `{kind}` step");
    }
}
fn collect_ops(value: &Value, ops: &mut std::collections::BTreeSet<String>) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(op)) = map.get("op") {
                ops.insert(op.clone());
            }
            for (key, inner) in map {
                if key != "value" || !map.contains_key("op") || map["op"] != "literal" {
                    collect_ops(inner, ops);
                }
            }
        }
        Value::Array(items) => items.iter().for_each(|v| collect_ops(v, ops)),
        _ => {}
    }
}

/// The string literal that follows each `marker(<first argument>,` (or
/// `marker(` itself when `first` is false).
fn codes_after(
    source: &str,
    marker: &str,
    skip_first: bool,
    codes: &mut std::collections::BTreeSet<String>,
) {
    let mut rest = source;
    while let Some(at) = rest.find(marker) {
        rest = &rest[at + marker.len()..];
        let mut args = rest.trim_start();
        if skip_first {
            let Some(comma) = args.find(',') else { break };
            if args[..comma].contains(['(', ')', '"']) {
                continue;
            }
            args = args[comma + 1..].trim_start();
        }
        if let Some(text) = args.strip_prefix('"') {
            let code: String = text.chars().take_while(|c| *c != '"').collect();
            if code.len() > 1 && code.chars().all(|c| c.is_ascii_lowercase() || c == '_') {
                codes.insert(code);
            }
        }
    }
}

#[test]
fn every_validation_code_has_an_entry() {
    let mut codes = std::collections::BTreeSet::new();
    for source in SOURCES {
        codes_after(source, "issue(", true, &mut codes);
        codes_after(source, "Diagnostic::error(", false, &mut codes);
    }
    codes.extend(FORMATTED_CODES.map(String::from));
    // The scan finds the codes it must: a code from each source.
    for code in [
        "unhandled_outcome",
        "draft_limit",
        "workspace_file",
        "source_header",
        "call",
    ] {
        assert!(codes.contains(code), "the scan missed {code}");
    }
    let headings: std::collections::BTreeSet<String> = LANGUAGE
        .split("\n## Diagnostic codes\n")
        .nth(1)
        .expect("LANGUAGE.md has a Diagnostic codes section")
        .lines()
        .filter_map(|l| l.strip_prefix("### "))
        .map(String::from)
        .collect();
    let missing: Vec<&String> = codes.difference(&headings).collect();
    assert!(
        missing.is_empty(),
        "codes without an entry in LANGUAGE.md: {missing:?}"
    );
    let unknown: Vec<&String> = headings.difference(&codes).collect();
    assert!(
        unknown.is_empty(),
        "entries for codes no tool reports: {unknown:?}"
    );
}

/// The notes cover every contract of the pinned catalog exactly (the
/// renderer refuses notes that miss or add an input field, an outcome or a
/// payload field), and every rendered section lists each field and outcome.
#[test]
fn the_catalog_notes_cover_every_contract_of_the_pinned_catalog() {
    let registry = catalog_registry();
    let commands = registry.commands.keys().cloned().collect();
    let events = registry.events.keys().cloned().collect();
    let all = catalog(&commands, &events).unwrap();
    for (name, contract) in &registry.commands {
        let section = all
            .split(&format!("\n### `{name}` "))
            .nth(1)
            .unwrap_or_else(|| panic!("no section for {name}"))
            .split("\n### ")
            .next()
            .unwrap();
        if let aq_playbooks_model::Schema::Object { properties, .. } = &contract.input {
            for field in properties.keys() {
                assert!(
                    section.contains(&format!("| `{field}` |")),
                    "{name}: input {field}"
                );
            }
        }
        for outcome in contract.outcomes.keys() {
            assert!(
                section.contains(&format!("| `{outcome}` |")),
                "{name}: outcome {outcome}"
            );
        }
    }
    for name in registry.events.keys() {
        assert!(
            all.contains(&format!("\n### `{name}` ")),
            "no section for {name}"
        );
    }
    // A name outside the pinned catalog is refused.
    assert!(catalog(&["work.reed".to_string()].into(), &Default::default()).is_err());
    for hidden in [
        "sha256:",
        "crates/",
        ".rs",
        "aq-server",
        "docs/",
        "README",
        "Task ",
    ] {
        assert!(!all.contains(hidden), "CATALOG.md contains {hidden:?}");
    }
}

/// A union type must not break the Markdown tables of the rendered catalog:
/// every table row has as many columns as its header (fix round 2, N1).
#[test]
fn every_table_row_of_the_rendered_catalog_has_its_header_columns() {
    let registry = catalog_registry();
    let commands = registry.commands.keys().cloned().collect();
    let events = registry.events.keys().cloned().collect();
    let all = catalog(&commands, &events).unwrap();
    let columns = |row: &str| row.trim().trim_matches('|').split(" | ").count();
    let mut header = 0;
    let mut rows = 0;
    let mut previous_was_row = false;
    for line in all.lines() {
        if !line.starts_with('|') {
            previous_was_row = false;
            continue;
        }
        if !previous_was_row {
            header = columns(line);
        } else {
            assert_eq!(columns(line), header, "{line}");
            rows += 1;
        }
        previous_was_row = true;
    }
    assert!(rows > 100, "{rows} rows");
}

#[test]
fn the_guide_names_every_tool() {
    for tool in [
        "check", "try", "compare", "facts", "save", "ask", "unable", "commands", "events",
    ] {
        assert!(
            GUIDE.contains(&format!("aq-playbooks {tool}")),
            "the guide never shows `aq-playbooks {tool}`"
        );
    }
}

#[test]
fn the_guide_documents_the_case_format() {
    // The tools point a malformed case here.
    assert!(CLI_AUTHORING.contains("see the case format in GUIDE.md"));
    let section = GUIDE
        .split("\n## The case format\n")
        .nth(1)
        .expect("GUIDE.md has a section `The case format`");
    let section = section.split("\n## ").next().unwrap();
    for field in [
        "`name`",
        "`situation`",
        "`start`",
        "`returns`",
        "`expect.outcome`",
        "`expect.calls`",
        "`input`",
        "`payload`",
        "correlation",
        "\"kind\": \"entry\"",
        "\"kind\": \"event\"",
        "{\"pending\":",
        "`[]`",
        "At least 5 and at most 40",
        "every call the run makes, in order",
        "compared exactly",
        "never uses fails the",
    ] {
        assert!(
            section.contains(field),
            "the case format does not say {field}"
        );
    }
    let kinds: Vec<(String, String)> = blocks(section)
        .iter()
        .map(|b| (role(b).0.to_string(), role(b).1.to_string()))
        .collect();
    for role in ["case", "start", "returns", "report"] {
        assert!(
            kinds.iter().any(|(_, r)| r == role),
            "the case format shows no `json {role}` block"
        );
    }
}

#[test]
fn every_draft_snippet_checks_as_shown() {
    let mut shown = 0;
    for (name, doc) in DOCUMENTS {
        let found = blocks(doc);
        for (i, block) in found.iter().enumerate() {
            let at = format!("{name} line {}", block.line);
            let next = found
                .get(i + 1)
                .filter(|b| role(b) == ("json", "diagnostic"));
            match role(block) {
                ("json", "draft") => {
                    let example = block
                        .info
                        .get(2)
                        .unwrap_or_else(|| panic!("{at}: name the example"));
                    let snippet: Value =
                        serde_json::from_str(&block.body).unwrap_or_else(|e| panic!("{at}: {e}"));
                    let mut ws = workspace(example);
                    patch(&mut ws.draft, &snippet);
                    let got = diagnostics(&ws);
                    match next {
                        Some(want) => {
                            let want: Value = serde_json::from_str(&want.body).unwrap();
                            assert!(
                                got.iter().any(|d| fits(&want, d)),
                                "{at}: no diagnostic is {want:#}; the check says {got:#?}"
                            );
                        }
                        None => assert!(got.is_empty(), "{at}: shown as right, but {got:#?}"),
                    }
                    shown += 1;
                }
                ("json", "diagnostic") => {
                    let want: Value =
                        serde_json::from_str(&block.body).unwrap_or_else(|e| panic!("{at}: {e}"));
                    let follows_a_draft = i > 0 && role(&found[i - 1]) == ("json", "draft");
                    match option(block, "limit") {
                        Some(limit) => {
                            let got = diagnostics(&past_the_bound(limit));
                            assert!(
                                got.iter().any(|d| fits(&want, d)),
                                "{at}: past the {limit} bound the check says {got:#?}"
                            );
                            shown += 1;
                        }
                        None => assert!(follows_a_draft, "{at}: a diagnostic follows its draft"),
                    }
                }
                ("json", "") => panic!("{at}: give every json block a role"),
                _ => {}
            }
        }
    }
    assert!(shown >= 15, "only {shown} draft snippets were checked");
}

#[test]
fn every_strict_rule_and_bound_is_taught() {
    let flat = |doc: &str| doc.replace(',', "");
    let words = |doc: &str| doc.split_whitespace().collect::<Vec<_>>().join(" ");
    let guide = words(&flat(GUIDE));
    // Each strict rule, by the diagnostic the guide shows for breaking it
    // (checked against the tool by `every_draft_snippet_checks_as_shown`).
    for taught in [
        "is not there on every way to this step: outcome `refused` of step `read`",
        "`exists` only tells you whether a value is there",
        "does not narrow by a",
        "A lookup whose key is missing takes its default without a trace",
        "`task` is not bound",
        "may be null at this step",
        "Bind a table, or its result, to a `let` name once",
        "part of what the playbook means",
        "covers the whole run from its start",
        "ask the owner to split the document",
        "When both apply, ask first",
        "Never stand one thing in for another",
        "something no command returns",
        "read the event's **Sent** line in `CATALOG.md`",
        "a message's words come from the document",
    ] {
        assert!(
            words(GUIDE).contains(taught),
            "the guide does not teach: {taught}"
        );
    }
    let bounds = [
        MAX_DRAFT_STEPS as u64,
        MAX_DRAFT_JSON_BYTES as u64,
        MAX_DRAFT_EXPANSION,
        MAX_DRAFT_EXPANSION_TOTAL,
        MAX_DRAFT_SCOPE,
        1_000_000, // shapes followed
    ];
    assert!(
        guide.contains("4194304"),
        "the guide does not give the outcome bound"
    );
    for bound in bounds {
        for (name, doc) in DOCUMENTS {
            assert!(
                flat(doc).contains(&bound.to_string()),
                "{name} does not give the bound {bound}"
            );
        }
    }
    for limit in ["steps", "bytes", "expression", "total", "scope", "shapes"] {
        let shown = DOCUMENTS.iter().any(|(_, doc)| {
            blocks(doc)
                .iter()
                .any(|b| option(b, "limit") == Some(limit))
        });
        assert!(
            shown,
            "no document shows the diagnostic for the {limit} bound"
        );
    }
    for words in [
        "At least 5 and at most 40",
        "1 to 16 sentences",
        "at most 8 lines",
        "4096 bytes",
    ] {
        assert!(guide.contains(words), "the guide does not say {words}");
    }
}

#[test]
fn the_documents_show_no_identifier_an_agent_cannot_see() {
    for (name, doc) in DOCUMENTS {
        for hidden in [
            "sha256:",
            "crates/",
            ".rs",
            "aq-server",
            "docs/",
            "README",
            "Task ",
            "spec 0",
            "skills/",
        ] {
            assert!(!doc.contains(hidden), "{name} contains {hidden:?}");
        }
    }
}

/// What a catalog says about routing reaches the agent in `CATALOG.md`,
/// and only when its record allows those commands; the guide and the
/// language reference say nothing about it.
#[test]
fn the_guide_speaks_of_routing_nowhere() {
    for (section, line) in prose(GUIDE) {
        assert!(
            !line.to_lowercase().contains("rout"),
            "GUIDE.md ({section}): {line}"
        );
    }
    for (_, line) in prose(LANGUAGE) {
        assert!(!line.to_lowercase().contains("rout"), "LANGUAGE.md: {line}");
    }
}

/// Rewrites each example's `CATALOG.md` after the notes change:
/// `cargo test -p aq-playbooks-compiler --test docs -- --ignored write_example_catalogs`.
#[test]
#[ignore]
fn write_example_catalogs() {
    for example in EXAMPLES {
        let record = workspace(example).record;
        let path = format!(
            "{}/authoring-examples/{example}/CATALOG.md",
            env!("CARGO_MANIFEST_DIR")
        );
        std::fs::write(path, catalog(&record.commands, &record.events).unwrap()).unwrap();
    }
}

/// `check` adds up the waits on each way through the run (I1): one wait as
/// long as the run's limit, or two that together reach it, are refused,
/// naming the steps and the limit; a limit above them checks clean.
#[test]
fn a_way_whose_waits_reach_the_run_limit_is_refused() {
    let mut ws = workspace("notify-when-ready");
    let mut second = ws.draft["steps"]["wait"].clone();
    ws.draft["steps"]["wait"]["op"]["deadline_ms"] = json!(50_000_000);
    ws.draft["steps"]["wait"]["edges"]["timeout"] = json!("wait2");
    second["op"]["deadline_ms"] = json!(50_000_000);
    second["op"]["bind"] = json!("waited2");
    ws.draft["steps"]["wait2"] = second;
    let got = diagnostics(&ws);
    let want = json!({"code": "deadline", "path": "draft.json:limits",
        "message": "limits.max_duration_ms: a run from entry `main` may wait 100000000 ms on one way, at `wait` (50000000 ms), `wait2` (50000000 ms), which reaches the playbook's duration limit of 86400000 ms (the default).…"});
    assert!(got.iter().any(|d| fits(&want, d)), "{got:#?}");
    let mut limits = serde_json::to_value(aq_playbooks_model::Limits::default()).unwrap();
    limits["max_duration_ms"] = json!(100_000_001);
    ws.draft["limits"] = limits;
    assert_eq!(diagnostics(&ws), Vec::<Value>::new());
    // A wait inside a for_each body may run any number of times: it is not
    // added up (try reports a run that stops on the limit there).
    let mut ws = workspace("notify-when-ready");
    ws.draft["steps"]["wait"]["op"]["deadline_ms"] = json!(86_400_000);
    ws.draft["steps"]["wait"]["edges"]["timeout"] = json!("each");
    ws.draft["steps"]["each"] = json!({"op": {"kind": "for_each",
        "collection": {"op": "literal", "value": [1]}, "item": "n", "body": "wait",
        "next": "read", "max_items": 1, "concurrency": 1, "on_failure": "stop"},
        "edges": {}, "on_error": "failed"});
    ws.draft["steps"]["ready"]["edges"]["true"] = json!("each");
    let got = diagnostics(&ws);
    assert!(got.iter().all(|d| d["code"] != "deadline"), "{got:#?}");
}

/// A name the record allows that the catalog lacks is the host's fault
/// (`catalog`), not the draft's.
#[test]
fn a_record_name_the_catalog_lacks_is_a_host_fault() {
    let mut ws = workspace("notify-when-ready");
    ws.catalog.commands.remove("message.send");
    let got = diagnostics(&ws);
    let want = json!({"code": "catalog", "path": "catalog.json",
        "message": "record.json allows the command message.send, but catalog.json has no contract for it: the host's files disagree, and no draft can fix that.…"});
    assert!(got.iter().any(|d| fits(&want, d)), "{got:#?}");
}
