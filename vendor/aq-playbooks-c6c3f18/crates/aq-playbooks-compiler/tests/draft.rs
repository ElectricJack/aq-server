//! A draft names contracts; the tool fills identities, header and definitions.
use aq_playbooks_compiler::draft::*;
use aq_playbooks_compiler::{COMPILER_VERSION, ChangeKind, CompileInput, Proposal, compile};
use aq_playbooks_model::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn strings(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}
fn command(
    name: &str,
    input: Schema,
    outcomes: &[(&str, Schema)],
    effect: Effect,
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
        recovery: if effect == Effect::Observation {
            Recovery::Observation
        } else {
            Recovery::NaturallyIdempotent
        },
        effect,
        asynchronous: None,
        cancellation: false,
        public_result_fields: BTreeSet::new(),
        sensitive_input_fields: BTreeSet::new(),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 4096,
        deadline_ms: 1000,
    }
}
fn task() -> Schema {
    Schema::object(BTreeMap::from([("task".into(), Schema::string(64))]))
}
fn catalog() -> Registry {
    let read = command(
        "work.read",
        task(),
        &[
            (
                "read",
                Schema::object(BTreeMap::from([("kind".into(), Schema::string(32))])),
            ),
            ("missing", Schema::Null),
        ],
        Effect::Observation,
    );
    let route = command(
        "work.route",
        Schema::object(BTreeMap::from([
            ("task".into(), Schema::string(64)),
            ("level".into(), Schema::string(32)),
        ])),
        &[("routed", Schema::Null), ("stale", Schema::Null)],
        Effect::Mutation,
    );
    // In the catalog, but not a command this source may use.
    let assess = command(
        "judgment.assess",
        Schema::Null,
        &[("assessed", Schema::Null)],
        Effect::Observation,
    );
    let needed = EventContract {
        sensitive_payload_fields: BTreeSet::new(),
        name: "aq.work.route_needed".into(),
        version: "1.0.0".into(),
        payload: task(),
        trusted_producers: strings(&["host"]),
        scopes: strings(&["project"]),
        correlation_fields: BTreeSet::new(),
        subject_field: None,
        revision_field: None,
        max_payload_bytes: 4096,
        retention_ms: 86_400_000,
    };
    Registry {
        commands: [read, route, assess]
            .into_iter()
            .map(|c| (c.name.clone(), c))
            .collect(),
        events: BTreeMap::from([(needed.name.clone(), needed)]),
    }
}
fn record() -> SourceRecord {
    SourceRecord {
        id: "routing".into(),
        binding: "aq.routing".into(),
        scope: "project".into(),
        purpose: "Route each task to a level".into(),
        commands: strings(&["work.read", "work.route"]),
        events: strings(&["aq.work.route_needed"]),
        capabilities: strings(&["work.read", "work.route"]),
        facts_version: "facts-1".into(),
    }
}
const MARKDOWN: &str =
    "# Routing\n\nChores are simple.\nEverything else is standard.\n\n\n\n\n\n\n\nA last line.\n";
fn reference(root: &str, path: &[&str]) -> Value {
    json!({"op": "ref", "root": root, "path": path})
}
fn ret(outcome: &str) -> Value {
    json!({"op": {"kind": "return", "outcome": outcome, "output": {"op": "literal", "value": null}},
           "edges": {}, "on_error": null})
}
fn read_step() -> Value {
    json!({"op": {"kind": "invoke", "command": "work.read", "bind": "work", "retry": null,
                  "input": {"op": "object", "fields": {"task": reference("input", &["task"])}}},
           "edges": {"read": "route", "missing": "failed"}, "on_error": "failed"})
}
fn route_step() -> Value {
    json!({"op": {"kind": "invoke", "command": "work.route", "bind": "routed", "retry": null,
                  "input": {"op": "object", "fields": {
                      "task": reference("input", &["task"]),
                      "level": {"op": "lookup", "key": reference("work", &["kind"]),
                                "cases": [{"when": "chore", "then": {"op": "literal", "value": "simple"}}],
                                "default": {"op": "literal", "value": "standard"}}}}},
           "edges": {"routed": "done", "stale": "failed"}, "on_error": "failed"})
}
fn draft() -> Value {
    json!({
        "purpose": "Route each task by its kind",
        "entries": {"main": {
            "input": {"type": "object", "properties": {"task": {"type": "string", "max_length": 64}},
                      "required": ["task"]},
            "outcomes": {"routed": {"type": "null"}, "failed": {"type": "null"}},
            "start": "read"}},
        "steps": {"read": read_step(), "route": route_step(), "done": ret("routed"), "failed": ret("failed")},
        "triggers": [{"id": "needed", "event": "aq.work.route_needed", "entry": "main",
                      "input": {"op": "object", "fields": {"task": reference("event", &["task"])}}}],
        "source_map": {"route": 3}
    })
}
fn workspace() -> Workspace {
    Workspace {
        markdown: MARKDOWN.into(),
        record: record(),
        catalog: catalog(),
        facts: BTreeMap::from([("levels".into(), json!(["simple", "standard"]))]),
        draft: draft(),
    }
}
fn has(diagnostics: &[aq_playbooks_compiler::Diagnostic], code: &str, path: &str) -> bool {
    diagnostics.iter().any(|d| d.code == code && d.path == path)
}

#[test]
fn a_draft_with_names_fills_to_the_same_artifact_as_a_hand_built_proposal() {
    let ws = workspace();
    let sealed = seal(&ws).unwrap_or_else(|d| panic!("{d:#?}"));
    // The same playbook, built by hand the way builders do today.
    let c = catalog();
    let mut playbook = draft();
    for (step, name) in [("read", "work.read"), ("route", "work.route")] {
        playbook["steps"][step]["op"]["command"] =
            json!({"name": name, "fingerprint": c.commands[name].fingerprint().unwrap()});
    }
    playbook["triggers"][0]["event"] = json!({"name": "aq.work.route_needed",
        "fingerprint": c.events["aq.work.route_needed"].fingerprint().unwrap()});
    let object = playbook.as_object_mut().unwrap();
    object.remove("source_map");
    object.insert("id".into(), json!("aq.routing"));
    object.insert("scope".into(), json!("project"));
    object.insert(
        "required_capabilities".into(),
        json!(["work.read", "work.route"]),
    );
    object.insert(
        "limits".into(),
        serde_json::to_value(Limits::default()).unwrap(),
    );
    let header = SourceHeader {
        format_version: FORMAT_VERSION,
        id: "aq.routing".into(),
        scope: "project".into(),
        interfaces: strings(&["main"]),
        definitions: strings(&["levels"]),
    };
    let mut registry = c.clone();
    registry.commands.remove("judgment.assess");
    let levels = json!(["simple", "standard"]);
    let input = CompileInput {
        source: format!(
            "---aq-playbook\n{}\n---\n{MARKDOWN}",
            serde_json::to_string(&header).unwrap()
        ),
        proposal: Proposal {
            playbook: serde_json::from_value(playbook).unwrap(),
            source_map: BTreeMap::from([(
                "steps.route".into(),
                SourceLocation { line: 6, column: 1 },
            )]),
        },
        registry,
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::from([(
            "levels".into(),
            CapturedDefinition {
                version: "facts-1".into(),
                digest: digest(&levels).unwrap(),
                value: levels,
            },
        )]),
        compiler: COMPILER_VERSION.into(),
    };
    let by_hand = compile(&input).unwrap_or_else(|d| panic!("{d:#?}"));
    assert_eq!(sealed.digest, by_hand.digest);
    assert_eq!(
        canonical_bytes(&sealed).unwrap(),
        canonical_bytes(&by_hand).unwrap()
    );
    assert_eq!(fill(&ws).unwrap().source, input.source);
    assert!(check(&ws).is_empty());
}

#[test]
fn an_unknown_command_names_the_allowed_ones() {
    let mut ws = workspace();
    ws.draft["steps"]["read"]["op"]["command"] = json!("work.reed");
    let diagnostics = check(&ws);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let d = &diagnostics[0];
    assert_eq!(d.code, "unknown_command");
    assert_eq!(d.path, "draft.json:steps.read.op.command");
    assert!(d.message.contains("\"work.reed\""), "{}", d.message);
    assert!(d.message.contains("work.read, work.route"), "{}", d.message);
    assert!(!d.message.contains("fingerprint"), "{}", d.message);
    // In the catalog is not enough: the source record must allow it.
    ws.draft["steps"]["read"]["op"]["command"] = json!("judgment.assess");
    let diagnostics = check(&ws);
    assert!(has(
        &diagnostics,
        "unknown_command",
        "draft.json:steps.read.op.command"
    ));
    let (_, allowed) = diagnostics[0]
        .message
        .split_once("closest first): ")
        .unwrap();
    assert!(!allowed.contains("judgment"), "{}", diagnostics[0].message);
    assert!(allowed.contains("work.read") && allowed.contains("work.route"));
    // Events: a wait or a trigger on an event outside the record.
    let mut ws = workspace();
    ws.draft["triggers"][0]["event"] = json!("aq.work.route_neded");
    let diagnostics = check(&ws);
    assert!(
        has(
            &diagnostics,
            "unknown_event",
            "draft.json:triggers[0].event"
        ),
        "{diagnostics:#?}"
    );
    assert!(diagnostics[0].message.contains("aq.work.route_needed"));
    // A call: an authoring source is offered no callable playbooks.
    let mut ws = workspace();
    ws.draft["steps"]["read"]["op"] = json!({"kind": "call", "dependency": "review",
        "entry": "main", "input": {"op": "literal", "value": null}, "mode": "wait", "bind": "work"});
    let diagnostics = check(&ws);
    assert!(
        has(&diagnostics, "call", "draft.json:steps.read.op"),
        "{diagnostics:#?}"
    );
    assert!(
        diagnostics[0].message.contains("work.read, work.route"),
        "{diagnostics:#?}"
    );
}

#[test]
fn an_async_command_brings_its_reconciliation_command_and_completion_event() {
    let catalog: Registry = serde_json::from_str(include_str!(
        "../../../contracts/aq-host-2.1.0/registry.json"
    ))
    .unwrap();
    let (registry, capabilities) =
        retain_registry(&catalog, &strings(&["judgment.request"]), &BTreeSet::new()).unwrap();
    assert_eq!(
        registry.commands.keys().cloned().collect::<BTreeSet<_>>(),
        strings(&["judgment.request", "operation.get"])
    );
    assert_eq!(
        registry.events.keys().cloned().collect::<BTreeSet<_>>(),
        strings(&["aq.judgment.completed"])
    );
    assert_eq!(
        capabilities,
        strings(&["judgment.request.v2", "operation.get"])
    );
    assert_eq!(
        registry.commands["operation.get"],
        catalog.commands["operation.get"]
    );
    let err =
        retain_registry(&catalog, &strings(&["judgment.reqest"]), &BTreeSet::new()).unwrap_err();
    assert!(err.contains("judgment.reqest"), "{err}");

    let ask = json!({"question": "Should this change be approved?",
        "subject": {"id": "work-1", "revision": 1},
        "context": {"text": "Work item summary.", "artifacts": []},
        "output": {"choices": ["approve", "decline"], "rationale_max_bytes": 2048},
        "budget": {"deadline_ms": 60000, "max_cost_microunits": 50000,
                   "max_input_tokens": 8192, "max_output_tokens": 512}});
    let ws = Workspace {
        markdown: "# Ask\n".into(),
        record: SourceRecord {
            id: "ask".into(),
            binding: "ask".into(),
            scope: "project".into(),
            purpose: "Ask".into(),
            commands: strings(&["judgment.request"]),
            events: BTreeSet::new(),
            capabilities: strings(&["judgment.request.v2", "operation.get"]),
            facts_version: String::new(),
        },
        catalog: catalog.clone(),
        facts: BTreeMap::new(),
        draft: json!({
            "purpose": "Ask once",
            "entries": {"main": {"input": {"type": "null"}, "outcomes": {"done": {"type": "null"}}, "start": "ask"}},
            "steps": {
                "ask": {"op": {"kind": "invoke", "command": "judgment.request", "bind": "j", "retry": null,
                               "input": {"op": "literal", "value": ask}},
                        "edges": {"accepted": "done", "assessed": "done", "failed": "done", "refused": "done"},
                        "on_error": "done"},
                "done": ret("done")},
            "triggers": []
        }),
    };
    let input = fill(&ws).unwrap_or_else(|d| panic!("{d:#?}"));
    assert_eq!(input.registry, registry);
    assert_eq!(input.proposal.playbook.required_capabilities, capabilities);
    assert!(check(&ws).is_empty(), "{:#?}", check(&ws));
    // The reconciliation command's capability must be one the source may use.
    let mut narrow = ws.clone();
    narrow.record.capabilities = strings(&["judgment.request.v2"]);
    let diagnostics = check(&narrow);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].code, "capability");
    assert_eq!(diagnostics[0].path, "draft.json:steps.ask.op.command");
    assert!(
        diagnostics[0].message.contains("operation.get"),
        "{}",
        diagnostics[0].message
    );
}

#[test]
fn the_header_is_written_by_the_tool_and_lines_shift() {
    let r = record();
    let (text, lines) = source_text(&r, &strings(&["main"]), &strings(&["levels"]), MARKDOWN);
    assert_eq!(lines, 3);
    assert!(text.ends_with(MARKDOWN));
    let header = aq_playbooks_compiler::parse_source_header(&text).unwrap();
    assert_eq!(header.id, "aq.routing");
    assert_eq!(header.scope, "project");
    assert_eq!(header.interfaces, strings(&["main"]));
    assert_eq!(header.definitions, strings(&["levels"]));
    // A diagnostic on a step mapped to source.md line 12 reports line 12 + header lines.
    let mut ws = workspace();
    ws.draft["source_map"] = json!({"done": 12});
    ws.draft["steps"]["done"]["op"]["outcome"] = json!("finished");
    let diagnostics = check(&ws);
    let d = diagnostics
        .iter()
        .find(|d| d.path == "draft.json:steps.done")
        .unwrap_or_else(|| panic!("{diagnostics:#?}"));
    // Ruling 2026-11-13 (A3 review I1), replacing the brief's `12 + lines`:
    // line and column are only ever positions in draft.json, so a mapped step
    // carries none; its message names the source.md line.
    assert_eq!((d.line, d.column), (None, None));
    assert!(d.message.contains("source.md line 12"), "{}", d.message);
    assert_eq!(
        fill(&ws).unwrap().proposal.source_map["steps.done"].line,
        12 + lines
    );
    // A line past the end of source.md is refused where it is written.
    ws.draft["source_map"] = json!({"done": 13});
    assert!(has(&check(&ws), "source_map", "draft.json:source_map.done"));
    // The agent cannot write the header itself.
    let mut ws = workspace();
    ws.draft["header"] = json!({"id": "aq.routing"});
    assert!(has(&check(&ws), "draft_format", "draft.json:header"));
}

#[test]
fn unknown_fields_are_still_refused_with_a_draft_position() {
    let dir = std::env::temp_dir().join(format!("aq-draft-{}-unknown", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ws = workspace();
    let mut text = serde_json::to_string_pretty(&ws.draft).unwrap();
    std::fs::write(dir.join("source.md"), &ws.markdown).unwrap();
    std::fs::write(
        dir.join("record.json"),
        serde_json::to_vec(&ws.record).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join("catalog.json"),
        serde_json::to_vec(&ws.catalog).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join("facts.json"),
        serde_json::to_vec(&ws.facts).unwrap(),
    )
    .unwrap();
    std::fs::write(dir.join("draft.json"), &text).unwrap();
    let read = Workspace::read(&dir).unwrap_or_else(|d| panic!("{d:#?}"));
    assert_eq!(read, ws);
    // "stepz" at top level.
    text = text.replacen("\"steps\"", "\"stepz\"", 1);
    std::fs::write(dir.join("draft.json"), &text).unwrap();
    let diagnostics = Workspace::read(&dir).unwrap_err();
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let d = &diagnostics[0];
    assert_eq!(d.code, "draft_format");
    assert_eq!(d.path, "draft.json:stepz");
    let line = text.lines().position(|l| l.contains("\"stepz\"")).unwrap() as u32 + 1;
    assert_eq!(d.line, Some(line));
    assert!(d.column.is_some());
    assert!(d.message.contains("unknown field `stepz`"), "{}", d.message);
    assert!(
        d.message.starts_with(&format!("draft.json line {line}")),
        "{}",
        d.message
    );
    // Deeper, the JSON path names the step.
    let text = serde_json::to_string_pretty(&ws.draft).unwrap().replacen(
        "\"on_error\": \"failed\"",
        "\"on_eror\": \"failed\"",
        1,
    );
    std::fs::write(dir.join("draft.json"), &text).unwrap();
    let diagnostics = Workspace::read(&dir).unwrap_err();
    assert_eq!(
        diagnostics[0].path, "draft.json:steps.read.on_eror",
        "{diagnostics:#?}"
    );
    // Inside an operation, which serde reads whole and reports at the end of
    // the step, the operation is named, at its own line.
    let text = serde_json::to_string_pretty(&ws.draft).unwrap().replacen(
        "\"command\": \"work.read\"",
        "\"comand\": \"work.read\"",
        1,
    );
    std::fs::write(dir.join("draft.json"), &text).unwrap();
    let diagnostics = Workspace::read(&dir).unwrap_err();
    assert_eq!(
        diagnostics[0].path, "draft.json:steps.read.op",
        "{diagnostics:#?}"
    );
    assert!(
        diagnostics[0].message.contains("unknown field `comand`"),
        "{diagnostics:#?}"
    );
    let lines: Vec<&str> = text.lines().collect();
    let comand = lines.iter().position(|l| l.contains("\"comand\"")).unwrap();
    let op = lines[..comand]
        .iter()
        .rposition(|l| l.trim() == "\"op\": {")
        .unwrap() as u32
        + 1;
    assert_eq!(diagnostics[0].line, Some(op), "{diagnostics:#?}");
    // And inside an expression, the expression.
    let text =
        serde_json::to_string_pretty(&ws.draft)
            .unwrap()
            .replacen("\"default\"", "\"defualt\"", 1);
    std::fs::write(dir.join("draft.json"), &text).unwrap();
    let diagnostics = Workspace::read(&dir).unwrap_err();
    assert_eq!(
        diagnostics[0].path, "draft.json:steps.route.op.input.fields.level",
        "{diagnostics:#?}"
    );
    assert!(
        diagnostics[0].message.contains("`defualt`"),
        "{diagnostics:#?}"
    );
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_draft_claiming_another_scope_or_capabilities_is_refused() {
    for (field, value) in [
        ("id", json!("other.playbook")),
        ("scope", json!("installation")),
        (
            "required_capabilities",
            json!(["work.read", "work.route", "judgment.assess"]),
        ),
    ] {
        let mut ws = workspace();
        ws.draft[field] = value;
        let diagnostics = check(&ws);
        assert_eq!(diagnostics.len(), 1, "{field}: {diagnostics:#?}");
        assert_eq!(diagnostics[0].code, "draft_identity");
        assert_eq!(diagnostics[0].path, format!("draft.json:{field}"));
        assert!(
            diagnostics[0].message.contains("Leave"),
            "{}",
            diagnostics[0].message
        );
    }
    // Carrying exactly what the tool writes is accepted.
    let mut ws = workspace();
    ws.draft["id"] = json!("aq.routing");
    ws.draft["scope"] = json!("project");
    ws.draft["required_capabilities"] = json!(["work.read", "work.route"]);
    assert!(check(&ws).is_empty(), "{:#?}", check(&ws));
    // Identities and registries cannot be smuggled in.
    let c = catalog();
    let mut ws = workspace();
    ws.draft["steps"]["read"]["op"]["command"] =
        json!({"name": "work.read", "fingerprint": c.commands["work.read"].fingerprint().unwrap()});
    let diagnostics = check(&ws);
    assert!(
        has(
            &diagnostics,
            "draft_format",
            "draft.json:steps.read.op.command"
        ),
        "{diagnostics:#?}"
    );
    assert!(
        diagnostics[0].message.contains("as a string"),
        "{}",
        diagnostics[0].message
    );
    let mut ws = workspace();
    ws.draft["registry"] = serde_json::to_value(&c).unwrap();
    assert!(has(&check(&ws), "draft_format", "draft.json:registry"));
    // A stray `command` on an operation that takes none is an unknown field.
    let mut ws = workspace();
    ws.draft["steps"]["done"]["op"]["command"] = json!({"name": "work.read"});
    let diagnostics = check(&ws);
    assert!(
        has(&diagnostics, "draft_format", "draft.json:steps.done.op"),
        "{diagnostics:#?}"
    );
    assert!(
        diagnostics[0].message.contains("unknown field `command`"),
        "{diagnostics:#?}"
    );
}

#[test]
fn compare_names_wording_only_behaviour_and_abilities() {
    let ws = workspace();
    let before = seal(&ws).unwrap();
    let mut wording = ws.clone();
    wording.markdown = MARKDOWN.replace("Chores are simple.", "A chore is simple.");
    let c = compare(&before, &seal(&wording).unwrap()).unwrap();
    assert_eq!(c.change, ChangeKind::Presentation);
    assert!(!c.abilities_changed);
    let mut behaviour = ws.clone();
    behaviour.draft["steps"]["route"]["op"]["input"]["fields"]["level"]["default"]["value"] =
        json!("complex");
    let c = compare(&before, &seal(&behaviour).unwrap()).unwrap();
    assert_eq!(c.change, ChangeKind::Executable);
    assert!(!c.abilities_changed);
    assert_eq!(c.diff.changed_steps, vec!["route"]);
    // A version that never routes, then one that does: a new command.
    let mut reads_only = ws.clone();
    reads_only.draft["steps"]["read"]["edges"]["read"] = json!("done");
    reads_only.draft["steps"]
        .as_object_mut()
        .unwrap()
        .remove("route");
    reads_only.draft["source_map"] = json!({});
    let older = seal(&reads_only).unwrap_or_else(|d| panic!("{d:#?}"));
    let c = compare(&older, &before).unwrap();
    assert_eq!(c.change, ChangeKind::Executable);
    assert!(c.abilities_changed);
    assert_eq!(c.diff.changed_commands, vec!["work.route"]);
    assert_eq!(c.diff.capabilities_added, strings(&["work.route"]));
    let c = compare(&before, &before).unwrap();
    assert_eq!(c.change, ChangeKind::Identical);
    assert!(!c.abilities_changed);
    // A new outcome of the entry changes the interface.
    let mut interface = ws.clone();
    interface.draft["entries"]["main"]["outcomes"]["held"] = json!({"type": "null"});
    let c = compare(&before, &seal(&interface).unwrap()).unwrap();
    assert_eq!(c.change, ChangeKind::Interface);
    assert!(!c.abilities_changed);
    // The same command name with a new contract that needs more.
    let mut contract = ws.clone();
    contract
        .catalog
        .commands
        .get_mut("work.route")
        .unwrap()
        .capabilities
        .insert("work.read".into());
    let c = compare(&before, &seal(&contract).unwrap()).unwrap();
    assert_eq!(c.change, ChangeKind::Executable);
    assert!(c.abilities_changed);
    assert_eq!(c.diff.changed_commands, vec!["work.route"]);
    assert!(c.diff.capabilities_added.is_empty());
}

#[test]
fn unfill_then_fill_round_trips() {
    let ws = workspace();
    let artifact = seal(&ws).unwrap();
    let draft = unfill(&artifact);
    let text = serde_json::to_string(&draft).unwrap();
    assert!(!text.contains("fingerprint"), "{text}");
    assert!(!text.contains("sha256:"), "{text}");
    for field in ["id", "scope", "required_capabilities", "limits"] {
        assert!(draft.get(field).is_none(), "{field}: {text}");
    }
    assert_eq!(draft["source_map"], json!({"route": 3}));
    assert_eq!(draft["steps"]["read"]["op"]["command"], json!("work.read"));
    assert_eq!(draft["triggers"][0]["event"], json!("aq.work.route_needed"));
    let again = Workspace {
        draft: draft.clone(),
        ..ws
    };
    let resealed = seal(&again).unwrap_or_else(|d| panic!("{d:#?}"));
    assert_eq!(
        canonical_bytes(&resealed).unwrap(),
        canonical_bytes(&artifact).unwrap()
    );
    assert_eq!(unfill(&resealed), draft);
}

// A binding read on a way that never bound it (A2 re-review R1-I2). Every
// outcome of these commands has the field, so ordinary typing accepts each
// read and only the draft check can refuse it.
fn kinds() -> Schema {
    Schema::object(BTreeMap::from([
        ("kind".into(), Schema::string(32)),
        (
            "kinds".into(),
            Schema::Array {
                items: Box::new(Schema::string(32)),
                max_items: 2,
            },
        ),
    ]))
}
fn object_outcomes() -> Workspace {
    let mut ws = workspace();
    let read = ws.catalog.commands.get_mut("work.read").unwrap();
    read.outcomes = BTreeMap::from([("read".into(), kinds()), ("missing".into(), kinds())]);
    let level = Schema::object(BTreeMap::from([("level".into(), Schema::string(32))]));
    let route = ws.catalog.commands.get_mut("work.route").unwrap();
    route.outcomes = BTreeMap::from([("routed".into(), level.clone()), ("stale".into(), level)]);
    ws.draft["source_map"] = json!({});
    ws
}
fn decide(predicate: Value, next: &str) -> Value {
    json!({"op": {"kind": "decide", "predicate": predicate},
           "edges": {"true": next, "false": next, "unknown": next}, "on_error": null})
}
fn kind_is(value: &str) -> Value {
    json!({"op": "eq", "left": reference("work", &["kind"]), "right": {"op": "literal", "value": value}})
}
fn unbound(ws: &Workspace, step: &str, texts: &[&str]) {
    let diagnostics = check(ws);
    let d = diagnostics
        .iter()
        .find(|d| d.path == format!("draft.json:steps.{step}"))
        .unwrap_or_else(|| panic!("{diagnostics:#?}"));
    assert_eq!(d.code, "expression_type", "{diagnostics:#?}");
    for text in texts {
        assert!(d.message.contains(text), "{text}: {}", d.message);
    }
    // The stored form of the same draft still verifies.
    let mut input = fill(ws).unwrap();
    input.proposal.source_map.clear();
    let header = aq_playbooks_compiler::parse_source_header(&input.source).unwrap();
    let content = ArtifactContent {
        format_version: FORMAT_VERSION,
        compiler: input.compiler.clone(),
        source: SourceBinding {
            digest: hash_bytes(input.source.as_bytes()),
            header,
        },
        playbook: input.proposal.playbook,
        registry: input.registry,
        dependencies: input.dependencies,
        definitions: input.definitions,
        source_map: BTreeMap::new(),
    };
    assert!(Artifact::seal(content).unwrap().verify().is_ok());
}

#[test]
fn a_shared_step_after_branches_cannot_read_what_only_one_branch_bound() {
    // The routing draft's natural single final step: `done` returns
    // `routed.level`, but the `missing` way goes through `assume`, which binds
    // `assumed`, never `routed`.
    let mut ws = object_outcomes();
    ws.draft["entries"]["main"]["outcomes"]["routed"] =
        serde_json::to_value(Schema::string(32)).unwrap();
    ws.draft["steps"]["read"]["edges"]["missing"] = json!("assume");
    let mut assume = route_step();
    assume["op"]["bind"] = json!("assumed");
    ws.draft["steps"]["assume"] = assume;
    ws.draft["steps"]["done"]["op"]["output"] = reference("routed", &["level"]);
    unbound(
        &ws,
        "done",
        &[
            "`routed.level` is not there on every way to this step",
            "the way from the start of entry `main` that does not pass step `route`, `routed` is not bound",
        ],
    );
    // Written as one return per route, it checks.
    ws.draft["steps"]["assume"]["edges"]["routed"] = json!("assumed_done");
    ws.draft["steps"]["assumed_done"] = json!({"op": {"kind": "return", "outcome": "routed",
        "output": reference("assumed", &["level"])}, "edges": {}, "on_error": null});
    assert!(check(&ws).is_empty(), "{:#?}", check(&ws));
}

#[test]
fn a_loop_cannot_read_on_its_first_pass_what_a_later_step_of_its_body_binds() {
    let mut ws = object_outcomes();
    ws.draft["entries"]["main"]["start"] = json!("each");
    ws.draft["steps"]["each"] = json!({"op": {"kind": "for_each",
        "collection": {"op": "literal", "value": ["a", "b"]}, "item": "t", "body": "note",
        "next": "done", "max_items": 2, "concurrency": 1, "on_failure": "stop"},
        "edges": {}, "on_error": "failed"});
    ws.draft["steps"]["note"] = decide(kind_is("chore"), "read");
    ws.draft["steps"]["read"]["edges"] = json!({"read": "each", "missing": "each"});
    ws.draft["steps"].as_object_mut().unwrap().remove("route");
    unbound(
        &ws,
        "note",
        &[
            "`work.kind`",
            "that does not pass step `read`, `work` is not bound",
        ],
    );
    // Tested with `exists` first, it checks.
    ws.draft["steps"]["note"] = decide(
        json!({"op": "exists", "value": reference("work", &["kind"])}),
        "read",
    );
    assert!(check(&ws).is_empty(), "{:#?}", check(&ws));
}

#[test]
fn after_a_for_each_that_may_not_run_its_body_bindings_are_not_bound() {
    // Over an empty collection the body never runs, so `after` is reached
    // with `work` never bound.
    let mut ws = object_outcomes();
    ws.draft["entries"]["main"]["start"] = json!("each");
    ws.draft["steps"]["each"] = json!({"op": {"kind": "for_each",
        "collection": {"op": "literal", "value": []}, "item": "t", "body": "read",
        "next": "after", "max_items": 1, "concurrency": 1, "on_failure": "stop"},
        "edges": {}, "on_error": "failed"});
    ws.draft["steps"]["read"]["edges"] = json!({"read": "each", "missing": "each"});
    ws.draft["steps"]["after"] = decide(kind_is("chore"), "done");
    ws.draft["steps"].as_object_mut().unwrap().remove("route");
    unbound(
        &ws,
        "after",
        &[
            "`work.kind`",
            "that does not pass step `read`, `work` is not bound",
        ],
    );
}

#[test]
fn the_entry_step_reached_again_by_a_back_edge_cannot_read_what_the_loop_binds() {
    // `each` is the entry's start and the anchor its body returns to; its
    // collection reads `work`, which only the body binds.
    let mut ws = object_outcomes();
    ws.draft["entries"]["main"]["start"] = json!("each");
    ws.draft["steps"]["each"] = json!({"op": {"kind": "for_each",
        "collection": reference("work", &["kinds"]), "item": "t", "body": "read",
        "next": "done", "max_items": 2, "concurrency": 1, "on_failure": "stop"},
        "edges": {}, "on_error": "failed"});
    ws.draft["steps"]["read"]["edges"] = json!({"read": "each", "missing": "each"});
    ws.draft["steps"].as_object_mut().unwrap().remove("route");
    unbound(
        &ws,
        "each",
        &[
            "`work.kinds`",
            "that does not pass step `read`, `work` is not bound",
        ],
    );
}

#[test]
fn a_refined_path_names_only_the_object_serde_complained_about() {
    // A3 re-review I2, probe 1: two faulty expressions in one operation, the
    // one serde reports (`zz`, first in the file) after the other in key order.
    let dir = std::env::temp_dir().join(format!("aq-draft-{}-refined", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let ws = workspace();
    std::fs::write(dir.join("source.md"), &ws.markdown).unwrap();
    std::fs::write(
        dir.join("record.json"),
        serde_json::to_vec(&ws.record).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join("catalog.json"),
        serde_json::to_vec(&ws.catalog).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join("facts.json"),
        serde_json::to_vec(&ws.facts).unwrap(),
    )
    .unwrap();
    let text = serde_json::to_string_pretty(&ws.draft).unwrap().replacen(
        "\"fields\": {",
        "\"fields\": {\"zz\": {\"op\": \"literal\", \"valu\": 1},\n \"aa\": {\"op\": \"ref\", \"rooot\": \"input\"},",
        1,
    );
    std::fs::write(dir.join("draft.json"), &text).unwrap();
    let diagnostics = Workspace::read(&dir).unwrap_err();
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let d = &diagnostics[0];
    assert!(d.message.contains("unknown field `valu`"), "{d:#?}");
    assert_eq!(d.path, "draft.json:steps.read.op.input.fields.zz", "{d:#?}");
    let line = text.lines().position(|l| l.contains("\"zz\"")).unwrap() as u32 + 1;
    assert_eq!(d.line, Some(line), "{d:#?}");
    std::fs::remove_dir_all(&dir).unwrap();
    // Probe 2: literal data that has a `type` key is data, never a schema.
    let mut ws = workspace();
    ws.draft["steps"]["done"]["op"]["output"] =
        json!({"extra": 1, "op": "literal", "value": {"type": "user"}});
    let diagnostics = check(&ws);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        diagnostics[0].path, "draft.json:steps.done.op.output",
        "{diagnostics:#?}"
    );
    assert!(
        diagnostics[0].message.contains("unknown field `extra`"),
        "{diagnostics:#?}"
    );
    // A lookup case's `when` is data too.
    let mut ws = workspace();
    ws.draft["steps"]["route"]["op"]["input"]["fields"]["level"]["cases"][0]["when"] =
        json!({"op": "nonsense"});
    ws.draft["steps"]["route"]["op"]["input"]["fields"]["level"]["cases"][0]["thn"] = json!(1);
    let diagnostics = check(&ws);
    assert_eq!(
        diagnostics[0].path, "draft.json:steps.route.op.input.fields.level",
        "{diagnostics:#?}"
    );
}

/// The fixture with `extra` decide steps chained in front of `read`.
fn chained(extra: usize) -> Workspace {
    let mut ws = workspace();
    let steps = ws.draft["steps"].as_object_mut().unwrap();
    for i in 0..extra {
        let next = if i + 1 == extra {
            "read".to_string()
        } else {
            format!("d{}", i + 1)
        };
        steps.insert(
            format!("d{i}"),
            decide(json!({"op": "literal", "value": true}), &next),
        );
    }
    ws.draft["entries"]["main"]["start"] = json!("d0");
    ws
}

#[test]
fn a_draft_is_bounded_in_steps_and_bytes_before_it_is_checked() {
    // 252 + the fixture's 4 steps = 256: accepted, and it checks.
    let ws = chained(252);
    assert_eq!(
        ws.draft["steps"].as_object().unwrap().len(),
        MAX_DRAFT_STEPS
    );
    assert!(check(&ws).is_empty(), "{:#?}", check(&ws));
    // 257: refused, with the limit and what to do.
    let diagnostics = check(&chained(253));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let d = &diagnostics[0];
    assert_eq!(
        (d.code.as_str(), d.path.as_str()),
        ("draft_limit", "draft.json:steps")
    );
    assert!(
        d.message
            .contains("has 257 steps; a draft may have at most 256"),
        "{}",
        d.message
    );
    assert!(
        d.message.contains("Shorten it")
            && d.message.contains("ask the owner to split the document"),
        "{}",
        d.message
    );
    assert!(!d.message.contains("sub-workflow"), "{}", d.message);
    // Bytes: the serialized draft, and the file.
    let mut big = workspace();
    let padding = "x".repeat(MAX_DRAFT_JSON_BYTES);
    big.draft["steps"]["done"]["op"]["output"] = json!({"op": "literal", "value": padding});
    let diagnostics = check(&big);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        (diagnostics[0].code.as_str(), diagnostics[0].path.as_str()),
        ("draft_limit", "draft.json")
    );
    assert!(
        diagnostics[0]
            .message
            .contains("a draft may be at most 786432 bytes")
    );
    let dir = std::env::temp_dir().join(format!("aq-draft-{}-big", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("source.md"), &big.markdown).unwrap();
    std::fs::write(
        dir.join("record.json"),
        serde_json::to_vec(&big.record).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join("catalog.json"),
        serde_json::to_vec(&big.catalog).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join("facts.json"),
        serde_json::to_vec(&big.facts).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join("draft.json"),
        serde_json::to_vec(&big.draft).unwrap(),
    )
    .unwrap();
    let diagnostics = Workspace::read(&dir).unwrap_err();
    assert_eq!(diagnostics[0].code, "draft_limit", "{diagnostics:#?}");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn a_reused_binding_name_is_reported_without_the_costly_read_check() {
    let mut ws = workspace();
    ws.draft["steps"]["route"]["op"]["bind"] = json!("work");
    let diagnostics = check(&ws);
    assert!(
        diagnostics.iter().any(|d| d.code == "binding"),
        "{diagnostics:#?}"
    );
    // The fault names the other step that binds the name (fix round 2, N3).
    let binding = diagnostics.iter().find(|d| d.code == "binding").unwrap();
    assert!(
        binding.message.contains("binds `work`, which step `")
            && binding.message.contains("` already binds"),
        "{binding:#?}"
    );
    assert!(
        diagnostics
            .iter()
            .all(|d| !d.message.contains("is not there on every way")),
        "{diagnostics:#?}"
    );
}

// Computed aliases (A2 re-review round 2, I1). `read` reaches `peek` on both
// outcomes and `missing` is `{}`, with no `kind`; ordinary typing accepts
// every read here, because `kind` is merely optional in the merged type.
fn eqx(left: Value, value: &str) -> Value {
    json!({"op": "eq", "left": left, "right": {"op": "literal", "value": value}})
}
fn let1(name: &str, value: Value, body: Value) -> Value {
    json!({"op": "let", "bindings": [{"name": name, "value": value}], "body": body})
}
fn keyed_by(key: Value) -> Value {
    json!({"op": "lookup", "key": key,
           "cases": [{"when": "chore", "then": {"op": "literal", "value": true}}],
           "default": {"op": "literal", "value": false}})
}
fn partial(predicate: Value) -> Workspace {
    let mut ws = object_outcomes();
    ws.catalog
        .commands
        .get_mut("work.read")
        .unwrap()
        .outcomes
        .insert("missing".into(), Schema::object(BTreeMap::new()));
    ws.draft["steps"]["read"]["edges"] = json!({"read": "peek", "missing": "peek"});
    ws.draft["steps"]["peek"] = decide(predicate, "route");
    ws.draft["steps"]["route"]["op"]["input"]["fields"]["level"]["key"] =
        reference("input", &["task"]);
    ws
}
/// The diagnostics at `peek`, and the reasons ("…this step: <reasons>.
/// Read it only…") of the first.
fn at_peek(predicate: Value) -> (Vec<aq_playbooks_compiler::Diagnostic>, String) {
    let diagnostics: Vec<_> = check(&partial(predicate))
        .into_iter()
        .filter(|d| d.path == "draft.json:steps.peek")
        .collect();
    let reasons = diagnostics
        .first()
        .and_then(|d| d.message.split_once("this step: "))
        .and_then(|(_, rest)| rest.split_once(". Read it only"))
        .map(|(reasons, _)| reasons.to_string())
        .unwrap_or_default();
    (diagnostics, reasons)
}

#[test]
fn a_computed_alias_is_held_to_the_same_check() {
    let w = |p: &[&str]| reference("work", p);
    let (direct, reasons) = at_peek(eqx(w(&["kind"]), "chore"));
    assert_eq!(direct.len(), 1, "{direct:#?}");
    assert!(
        reasons.contains(
            "outcome `missing` of step `read` has no `kind` in `work` (it has no fields)"
        ),
        "{reasons}"
    );
    let object = |field: Value| json!({"op": "object", "fields": {"k": field}});
    let refused = [
        // Probe 10: an object holding the binding, read through.
        let1(
            "o",
            object(w(&[])),
            eqx(reference("o", &["k", "kind"]), "chore"),
        ),
        // Probe 11: a lookup that yields the binding.
        let1(
            "o",
            json!({"op": "lookup", "key": {"op": "literal", "value": "a"},
                   "cases": [{"when": "a", "then": w(&[])}], "default": w(&[])}),
            eqx(reference("o", &["kind"]), "chore"),
        ),
        // A lookup that yields the binding in one case and a literal otherwise.
        let1(
            "o",
            json!({"op": "lookup", "key": reference("input", &["task"]),
                   "cases": [{"when": "a", "then": w(&[])}],
                   "default": {"op": "literal", "value": {"kind": "chore", "kinds": []}}}),
            eqx(reference("o", &["kind"]), "chore"),
        ),
        // Probe 12, and a let nested deeper.
        let1(
            "o",
            let1("p", w(&[]), reference("p", &[])),
            eqx(reference("o", &["kind"]), "chore"),
        ),
        let1(
            "o",
            let1(
                "p",
                object(w(&[])),
                let1("q", reference("p", &["k"]), reference("q", &[])),
            ),
            eqx(reference("o", &["kind"]), "chore"),
        ),
        // An array of bindings, indexed by position.
        let1(
            "a",
            json!({"op": "array", "items": [reference("input", &[]), w(&[])]}),
            eqx(reference("a", &["1", "kind"]), "chore"),
        ),
        // Probe 13: a map item of an array of objects holding the binding.
        json!({"op": "contains", "value": {"op": "literal", "value": "chore"},
               "collection": {"op": "map", "item": "it", "max_items": 1,
                   "collection": {"op": "array", "items": [object(w(&[]))]},
                   "value": reference("it", &["k", "kind"])}}),
    ];
    for (i, predicate) in refused.into_iter().enumerate() {
        let (diagnostics, alias_reasons) = at_peek(predicate);
        assert_eq!(diagnostics.len(), 1, "{i}: {diagnostics:#?}");
        assert_eq!(diagnostics[0].code, "expression_type", "{i}");
        assert_eq!(alias_reasons, reasons, "{i}: {}", diagnostics[0].message);
    }
    // Probe 16: as a lookup key, the silent default is refused too.
    let (diagnostics, alias_reasons) = at_peek(let1(
        "o",
        object(w(&[])),
        keyed_by(reference("o", &["k", "kind"])),
    ));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(alias_reasons, reasons);
    assert!(
        diagnostics[0]
            .message
            .contains("a lookup key is held to this too")
    );
    // `exists` stays the one exemption, and a field every way has is fine.
    for accepted in [
        let1(
            "o",
            object(w(&[])),
            json!({"op": "exists", "value": reference("o", &["k", "kind"])}),
        ),
        let1(
            "o",
            object(reference("input", &[])),
            eqx(reference("o", &["k", "task"]), "chore"),
        ),
        let1(
            "a",
            json!({"op": "array", "items": [reference("input", &[]), w(&[])]}),
            eqx(reference("a", &["0", "task"]), "chore"),
        ),
    ] {
        let (diagnostics, _) = at_peek(accepted.clone());
        assert!(diagnostics.is_empty(), "{accepted}: {diagnostics:#?}");
    }
    // A field read where it is embedded is refused there, once.
    let (diagnostics, _) = at_peek(let1(
        "o",
        object(w(&["kind"])),
        eqx(reference("o", &["k"]), "chore"),
    ));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert!(
        diagnostics[0]
            .message
            .starts_with("steps.peek.predicate.let.bindings[0].value.k: `work.kind`"),
        "{diagnostics:#?}"
    );
}

/// `depth` nested bindings, each an object holding the previous one twice:
/// the value doubles at every level once names are replaced by their values.
fn doubling(depth: usize) -> Value {
    let mut bindings = vec![json!({"name": "b0", "value": {"op": "object",
        "fields": {"k": reference("input", &[])}}})];
    for j in 1..depth {
        let p = format!("b{}", j - 1);
        bindings.push(json!({"name": format!("b{j}"), "value": {"op": "object",
            "fields": {"a": reference(&p, &[]), "b": reference(&p, &[])}}}));
    }
    json!({"op": "let", "bindings": bindings,
           "body": {"op": "exists", "value": reference(&format!("b{}", depth - 1), &[])}})
}
/// The fixture with one decide step per predicate chained in front of `read`.
fn deciding(predicates: Vec<Value>) -> Workspace {
    let mut ws = workspace();
    let n = predicates.len();
    let steps = ws.draft["steps"].as_object_mut().unwrap();
    for (i, predicate) in predicates.into_iter().enumerate() {
        let next = if i + 1 == n {
            "read".to_string()
        } else {
            format!("d{}", i + 1)
        };
        steps.insert(format!("d{i}"), decide(predicate, &next));
    }
    ws.draft["entries"]["main"]["start"] = json!("d0");
    ws
}

#[test]
fn an_expression_that_expands_past_the_bound_is_refused_before_it_is_typed() {
    // Depth 15 expands to about 197 000 nodes: accepted, and it checks.
    assert!(check(&deciding(vec![doubling(15)])).is_empty());
    // Depth 16, about 393 000: refused, alone, before typing and the read
    // check run (either would follow every copy).
    let diagnostics = check(&deciding(vec![doubling(16)]));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let d = &diagnostics[0];
    assert_eq!(
        (d.code.as_str(), d.path.as_str()),
        ("expression_limit", "draft.json:steps.d0")
    );
    assert!(
        d.message.starts_with("steps.d0.predicate: expands to "),
        "{}",
        d.message
    );
    assert!(
        d.message
            .contains("an expression may expand to at most 200000"),
        "{}",
        d.message
    );
    // Far past it the count saturates and still refuses at once.
    let diagnostics = check(&deciding(vec![doubling(70)]));
    assert!(
        diagnostics[0].message.contains("expands to too many nodes"),
        "{diagnostics:#?}"
    );
    // Five such expressions fit the whole draft's bound; six do not. (Six fit
    // while a reference to `input` counted as one node; it now counts the two
    // nodes of its type, so each expression is about 197 000.)
    assert!(check(&deciding(vec![doubling(15); 5])).is_empty());
    let diagnostics = check(&deciding(vec![doubling(15); 6]));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        (diagnostics[0].code.as_str(), diagnostics[0].path.as_str()),
        ("expression_limit", "draft.json")
    );
    assert!(
        diagnostics[0]
            .message
            .contains("a draft may expand to at most 1000000")
    );
}

// Values that come out of `map`, `filter` and `difference`, and `for_each`
// items (A2 re-review round 3, I1; probes 25, 26, 28 and 30). The generated
// test in the model crate covers every composition; these pin what the
// author is told.
#[test]
fn what_comes_out_of_map_filter_and_difference_is_held_to_the_same_check() {
    let w = |p: &[&str]| reference("work", p);
    let (_, reasons) = at_peek(eqx(w(&["kind"]), "chore"));
    let over = |collection: Value, item: &str, value: Value| json!({"op": "map", "item": item, "max_items": 1, "collection": collection, "value": value});
    let one = |e: Value| json!({"op": "array", "items": [e]});
    let kept = json!({"op": "filter", "item": "x", "max_items": 1, "collection": one(w(&[])),
        "predicate": {"op": "literal", "value": true}});
    let has_chore = |collection: Value| {
        json!({"op": "contains", "collection": collection,
               "value": {"op": "literal", "value": "chore"}})
    };
    let copied = over(one(w(&[])), "x", reference("x", &[]));
    for (predicate, at, written) in [
        // Probe 25: an item of what `filter` keeps.
        (
            has_chore(over(kept, "y", reference("y", &["kind"]))),
            "steps.peek.predicate.contains.collection.map.value",
            "`y.kind` (`y` is an item of its collection)",
        ),
        // Probe 26: an item of a `map` result bound to a name.
        (
            let1(
                "m",
                copied.clone(),
                has_chore(over(reference("m", &[]), "y", reference("y", &["kind"]))),
            ),
            "steps.peek.predicate.let.body.contains.collection.map.value",
            "`y.kind` (`y` is an item of its collection)",
        ),
        // Probe 28: a position of a `map` result as a lookup key, which
        // would take the default silently.
        (
            let1("m", copied, keyed_by(reference("m", &["0", "kind"]))),
            "steps.peek.predicate.let.body.lookup.key",
            "`m.0.kind` (`m` is computed from other values)",
        ),
    ] {
        let (diagnostics, found) = at_peek(predicate);
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        let message = &diagnostics[0].message;
        assert!(
            message.starts_with(&format!("{at}: {written} is not there")),
            "{message}"
        );
        assert_eq!(found, reasons, "{message}");
    }
    // A value read where it is bound and used through its name outside
    // `exists` is refused where it is read, and the name is given.
    let (diagnostics, found) = at_peek(let1(
        "k",
        w(&["kind"]),
        json!({"op": "and", "values": [
            {"op": "exists", "value": reference("k", &[])},
            eqx(reference("k", &[]), "chore")]}),
    ));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert!(
        diagnostics[0].message.starts_with(
            "steps.peek.predicate.let.bindings[0].value: `work.kind` (its value is used \
             through `k`, outside any `exists`) is not there"
        ),
        "{diagnostics:#?}"
    );
    assert_eq!(found, reasons);
    // The identity of a `difference` is read in every item, and no `exists`
    // makes up for an item without it.
    let (diagnostics, found) = at_peek(json!({"op": "exists", "value": {
        "op": "difference", "max_items": 1, "identity": ["kind"],
        "left": one(w(&[])), "right": {"op": "literal", "value": []}}}));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let message = &diagnostics[0].message;
    assert!(
        message.starts_with("steps.peek.predicate.exists.value.difference.identity: `left.kind`"),
        "{message}"
    );
    assert!(message.contains("`exists` cannot test it"), "{message}");
    assert_eq!(found, "");
    assert!(message.contains(&reasons), "{message}");
}

#[test]
fn a_for_each_item_carries_what_its_collection_holds_and_only_in_the_body() {
    // Probe 30: a loop over `[work]` whose body keys a lookup on `t.kind`.
    let w = |p: &[&str]| reference("work", p);
    let (_, reasons) = at_peek(eqx(w(&["kind"]), "chore"));
    let mut ws = partial(json!({"op": "literal", "value": true}));
    ws.draft["steps"]["peek"] = json!({"op": {"kind": "for_each",
        "collection": {"op": "array", "items": [w(&[])]}, "item": "t", "body": "note",
        "next": "after", "max_items": 1, "concurrency": 1, "on_failure": "stop"},
        "edges": {}, "on_error": "failed"});
    ws.draft["steps"]["note"] = decide(keyed_by(reference("t", &["kind"])), "peek");
    ws.draft["steps"]["after"] = decide(json!({"op": "literal", "value": true}), "route");
    let diagnostics = check(&ws);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let d = &diagnostics[0];
    assert_eq!(
        (d.code.as_str(), d.path.as_str()),
        ("expression_type", "draft.json:steps.note")
    );
    assert!(
        d.message.starts_with(
            "steps.note.predicate.lookup.key: `t.kind` (`t` is an item of the collection of \
             for_each `peek`) is not there on every way to this step: "
        ),
        "{}",
        d.message
    );
    assert!(d.message.contains(&reasons), "{}", d.message);
    assert!(
        d.message.contains("a lookup key is held to this too"),
        "{}",
        d.message
    );
    // A field every outcome has is read through the item without complaint.
    ws.draft["steps"]["note"] = decide(keyed_by(reference("t", &["kinds", "0"])), "peek");
    ws.catalog
        .commands
        .get_mut("work.read")
        .unwrap()
        .outcomes
        .insert("missing".into(), kinds());
    assert!(check(&ws).is_empty(), "{:#?}", check(&ws));
    // The item is bound in the body only: after the loop it is not there.
    ws.draft["steps"]["after"] = decide(eqx(reference("t", &["kind"]), "chore"), "route");
    let diagnostics = check(&ws);
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert!(
        diagnostics[0].message.starts_with(
            "steps.after.predicate.eq.left: `t.kind` (`t` is an item of the collection of \
             for_each `peek`) is not there on every way to this step: on the `next` exit of \
             for_each `peek`, `t` is not bound."
        ),
        "{diagnostics:#?}"
    );
}

#[test]
fn a_failure_in_a_loop_body_is_followed_to_where_the_loop_takes_it() {
    // `read` runs before the loop and again in its body. Its `missing`
    // outcome leaves the loop by `noted`, so on ordinary edges `after` sees
    // only the `read` outcome, which has `kind`. But a failure of `noted`
    // inside the loop is the loop's to handle: with `on_failure: continue`
    // it goes on to the next item, or to `after`, with `work` as `missing`
    // left it.
    let looped = |on_failure: &str| {
        let mut ws = partial(json!({"op": "literal", "value": true}));
        let steps = ws.draft["steps"].as_object_mut().unwrap();
        steps.remove("peek");
        let mut noted = steps.remove("route").unwrap();
        noted["edges"] = json!({"routed": "done", "stale": "failed"});
        steps.insert("noted".into(), noted);
        steps.insert(
            "each".into(),
            json!({"op": {"kind": "for_each",
                "collection": {"op": "literal", "value": ["a"]}, "item": "t", "body": "again",
                "next": "after", "max_items": 1, "concurrency": 1, "on_failure": on_failure},
                "edges": {}, "on_error": "failed"}),
        );
        steps.insert(
            "again".into(),
            decide(json!({"op": "literal", "value": true}), "read"),
        );
        steps.insert("after".into(), decide(kind_is("chore"), "done"));
        ws.draft["steps"]["read"]["edges"] = json!({"read": "each", "missing": "noted"});
        ws
    };
    let diagnostics = check(&looped("continue"));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let d = &diagnostics[0];
    assert_eq!(
        (d.code.as_str(), d.path.as_str()),
        ("expression_type", "draft.json:steps.after")
    );
    assert!(
        d.message
            .contains("outcome `missing` of step `read` has no `kind` in `work`"),
        "{}",
        d.message
    );
    // With `stop` the failure leaves by the loop's `on_error`, and `after`
    // is reached only with the outcome that has the field.
    let diagnostics = check(&looped("stop"));
    assert!(diagnostics.is_empty(), "{diagnostics:#?}");
}

// A literal is read as the value it is (A2 re-review round 4, I1; probes L1,
// L1b and L2): each row keeps its own fields and an array its own length, as
// for the same value written with `object` and `array` expressions.
#[test]
fn a_literal_table_is_read_row_by_row_and_an_array_to_its_end() {
    let table = |rows: Value, body: Value| json!({"op": "exists", "value": let1("t", json!({"op": "literal", "value": rows}), body)});
    let rows = json!([{"k": "chore"}, {}]);
    let over = |op: &str, field: &str, body: Value| json!({"op": op, "item": "r", "max_items": 2, "collection": reference("t", &[]), field: body});
    // L1: a lookup keyed on a field one row lacks would take its default
    // for that row without a trace.
    let (diagnostics, reasons) = at_peek(table(
        rows.clone(),
        over("map", "value", keyed_by(reference("r", &["k"]))),
    ));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let message = &diagnostics[0].message;
    assert!(
        message.starts_with(
            "steps.peek.predicate.exists.value.let.body.map.value.lookup.key: `r.k` (`r` is an \
             item of its collection) is not there on every way to this step: "
        ),
        "{message}"
    );
    assert_eq!(reasons, "`k` is not in an object written with no fields");
    assert!(
        message.contains("a lookup key is held to this too"),
        "{message}"
    );
    // L1b: outside `exists`, a filter on that field is Missing for that row.
    let Value::Object(mut unwrapped) = table(
        rows.clone(),
        json!({"op": "contains", "value": {"op": "literal", "value": {"k": "chore"}},
            "collection": over("filter", "predicate", eqx(reference("r", &["k"]), "chore"))}),
    ) else {
        unreachable!()
    };
    let (diagnostics, reasons) = at_peek(unwrapped.remove("value").unwrap());
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(reasons, "`k` is not in an object written with no fields");
    // L2: a position past the end of a literal array, as a lookup key.
    let (diagnostics, reasons) = at_peek(table(
        json!(["chore", "x"]),
        keyed_by(reference("t", &["5"])),
    ));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(
        reasons,
        "position `5` is past the end of an array written with 2 items"
    );
    // A table whose rows all have the field, and a position that is there,
    // check clean.
    for accepted in [
        table(
            json!([{"k": "chore"}, {"k": "fix"}]),
            over("map", "value", keyed_by(reference("r", &["k"]))),
        ),
        table(json!(["chore", "x"]), keyed_by(reference("t", &["1"]))),
    ] {
        let (diagnostics, _) = at_peek(accepted.clone());
        assert!(diagnostics.is_empty(), "{accepted}: {diagnostics:#?}");
    }
}

// What the diagnostic tells the author to do (A2 re-review round 4, m1):
// `exists` is a test, and testing does not make a read allowed.
#[test]
fn the_remedy_does_not_promise_that_exists_makes_a_read_allowed() {
    let w = |p: &[&str]| reference("work", p);
    // Probe E1: the value read in the branch its own `exists` selects.
    let (diagnostics, _) = at_peek(json!({"op": "lookup",
        "key": {"op": "exists", "value": w(&["kind"])},
        "cases": [{"when": true, "then": eqx(w(&["kind"]), "chore")}],
        "default": {"op": "literal", "value": false}}));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let message = &diagnostics[0].message;
    assert!(
        message.starts_with("steps.peek.predicate.lookup.cases[0].then.eq.left: `work.kind`"),
        "{message}"
    );
    for text in [
        "after the outcome edge of the step that returns it",
        "`exists` only tells you whether it is there: testing it does not make a read of it allowed",
        "branch on the edge first and read the value in the step that edge leads to",
    ] {
        assert!(message.contains(text), "{text}: {message}");
    }
    assert!(!message.contains("Or test it first"), "{message}");
}
