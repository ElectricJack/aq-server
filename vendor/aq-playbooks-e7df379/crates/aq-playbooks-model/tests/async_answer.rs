//! Reading an asynchronous answer: a named join over an operation handle types
//! the answer per outcome on its `completed` edge, and the handle is joined by
//! its binding (`{op: ref, root: <bind>}`, no path), exactly as the runner
//! resolves it.
use aq_playbooks_model::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

const REGISTRY_2_1: &str = include_str!("../../../contracts/aq-host-2.1.0/registry.json");

fn lit(v: Value) -> Expr {
    Expr::literal(v)
}
fn r(root: &str, path: &[&str]) -> Expr {
    Expr::Ref {
        root: root.into(),
        path: path.iter().map(|s| s.to_string()).collect(),
    }
}
fn edges(xs: &[(&str, &str)]) -> BTreeMap<String, String> {
    xs.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}
fn ret(outcome: &str, output: Expr) -> Step {
    Step {
        op: Operation::Return {
            outcome: outcome.into(),
            output,
        },
        edges: BTreeMap::new(),
        on_error: None,
    }
}
/// The 2.1.0 judgment command with what it needs to be valid: its completion
/// event and its reconciliation command.
fn registry() -> Registry {
    let all: Registry = serde_json::from_str(REGISTRY_2_1).unwrap();
    let keep = ["judgment.request", "operation.get"];
    Registry {
        commands: all
            .commands
            .into_iter()
            .filter(|(name, _)| keep.contains(&name.as_str()))
            .collect(),
        events: all
            .events
            .into_iter()
            .filter(|(name, _)| name == "aq.judgment.completed")
            .collect(),
    }
}
fn judgment_input() -> Value {
    json!({
        "budget": {"deadline_ms": 60000, "max_cost_microunits": 50000, "max_input_tokens": 8192, "max_output_tokens": 512},
        "context": {"artifacts": [], "text": "Work item summary."},
        "output": {"choices": ["approve", "decline"], "rationale_max_bytes": 2048},
        "question": "Should this change be approved?",
        "subject": {"id": "work-1", "revision": 1},
    })
}
/// `ask` invokes `judgment.request` and binds `j`; `read` joins `member` under
/// the key `c` and binds `answer`. `completed`, `no_match` and `timeout` lead
/// to `chosen`, `unanswered` and `late`; the caller supplies those steps.
fn content(
    member: Expr,
    chosen: Step,
    unanswered: Step,
    late: Step,
    chosen_type: Schema,
) -> ArtifactContent {
    let registry = registry();
    let judgment = &registry.commands["judgment.request"];
    let header = SourceHeader {
        format_version: FORMAT_VERSION,
        id: "ask-then-read".into(),
        scope: "project".into(),
        interfaces: BTreeSet::from(["main".into()]),
        definitions: BTreeSet::new(),
    };
    ArtifactContent {
        format_version: FORMAT_VERSION,
        compiler: "test".into(),
        source: SourceBinding {
            digest: hash_bytes(b"source"),
            header,
        },
        playbook: Playbook {
            id: "ask-then-read".into(),
            purpose: "ask, then read the answer".into(),
            scope: "project".into(),
            entries: BTreeMap::from([(
                "main".into(),
                EntryPoint {
                    input: Schema::Null,
                    outcomes: BTreeMap::from([
                        ("chosen".into(), chosen_type),
                        ("unanswered".into(), Schema::Null),
                        ("late".into(), Schema::Null),
                        ("error".into(), Schema::Null),
                    ]),
                    start: "ask".into(),
                },
            )]),
            steps: BTreeMap::from([
                (
                    "ask".into(),
                    Step {
                        op: Operation::Invoke {
                            command: ContractRef {
                                name: judgment.name.clone(),
                                fingerprint: judgment.fingerprint().unwrap(),
                            },
                            input: lit(judgment_input()),
                            bind: "j".into(),
                            retry: None,
                        },
                        edges: edges(&[
                            ("accepted", "read"),
                            ("assessed", "error"),
                            ("failed", "error"),
                            ("refused", "error"),
                        ]),
                        on_error: Some("error".into()),
                    },
                ),
                (
                    "read".into(),
                    Step {
                        op: Operation::Join {
                            members: vec![member],
                            member_keys: vec!["c".into()],
                            mode: JoinMode::All,
                            accepted_outcomes: BTreeSet::from(["assessed".into()]),
                            deadline_ms: 60_000,
                            remaining: RemainingPolicy::Track,
                            bind: "answer".into(),
                        },
                        edges: edges(&[
                            ("completed", "chosen"),
                            ("no_match", "unanswered"),
                            ("timeout", "late"),
                        ]),
                        on_error: Some("error".into()),
                    },
                ),
                ("chosen".into(), chosen),
                ("unanswered".into(), unanswered),
                ("late".into(), late),
                ("error".into(), ret("error", lit(Value::Null))),
            ]),
            triggers: vec![],
            limits: Limits::default(),
            required_capabilities: BTreeSet::from(["judgment.request.v2".into()]),
        },
        registry,
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::new(),
        source_map: BTreeMap::new(),
    }
}
fn choice() -> Expr {
    r("answer", &["c", "output", "choice"])
}
fn returns_choice() -> Step {
    ret("chosen", choice())
}
fn returns_null(outcome: &str) -> Step {
    ret(outcome, lit(Value::Null))
}
fn has_issue(issues: &[ValidationIssue], code: &str, path: &str, text: &str) -> bool {
    issues
        .iter()
        .any(|i| i.code == code && i.path == path && i.message.contains(text))
}

#[test]
fn join_over_an_invoke_handle_narrows_completed_to_the_accepted_outcome() {
    let valid = content(
        r("j", &[]),
        returns_choice(),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::string(512),
    );
    assert_eq!(valid.validate_structure(), vec![]);
    assert_eq!(valid.draft_issues(&[]), vec![]);
    // The answer is exactly `assessed`'s `choice`: a string of at most 512 bytes.
    let narrower = content(
        r("j", &[]),
        returns_choice(),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::string(511),
    )
    .validate_structure();
    assert!(
        has_issue(&narrower, "schema_compatibility", "steps.chosen", "512"),
        "{narrower:?}"
    );
    // The same handle written through `vars` is the same member.
    let through_vars = content(
        r("vars", &["j"]),
        returns_choice(),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::string(512),
    );
    assert_eq!(through_vars.validate_structure(), vec![]);
}

/// What compiling a new draft reports: everything `verify` checks, plus the
/// checks only a draft is held to.
fn draft(c: &ArtifactContent) -> Vec<ValidationIssue> {
    let mut issues = c.validate_structure();
    let strict = c.draft_issues(&issues);
    issues.extend(strict);
    issues
}
/// A stored artifact with this content still verifies (and so loads).
fn stored_verifies(c: &ArtifactContent) -> bool {
    Artifact::seal(c.clone()).unwrap().verify().is_ok()
}

#[test]
fn the_handle_path_member_form_is_refused_by_validation() {
    let c = content(
        r("j", &["handle"]),
        returns_null("chosen"),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::Null,
    );
    let issues = draft(&c);
    assert!(
        issues.iter().any(|i| i.code == "join_member_type"),
        "{issues:?}"
    );
    // The diagnostic alone is enough to fix the draft: it names the step, the
    // member as written, and the form that is accepted.
    let message = &issues
        .iter()
        .find(|i| i.code == "join_member_type")
        .unwrap();
    assert_eq!(message.path, "steps.read");
    for text in [
        "members[0] (key c)",
        "`j.handle`",
        "step `ask` (judgment.request)",
        r#"{"op":"ref","root":"j","path":[]}"#,
    ] {
        assert!(
            message.message.contains(text),
            "{text}: {}",
            message.message
        );
    }
    // A stored artifact with the old form keeps verifying, so it can never
    // make its namespace unloadable; the runner refuses it at the join.
    assert_eq!(c.validate_structure(), vec![]);
    assert!(stored_verifies(&c));
    // The same form through `vars` is refused the same way.
    let issues = draft(&content(
        r("vars", &["j", "handle"]),
        returns_null("chosen"),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::Null,
    ));
    assert!(
        has_issue(&issues, "join_member_type", "steps.read", "`vars.j.handle`"),
        "{issues:?}"
    );
}

#[test]
fn the_handle_path_is_refused_in_a_join_without_member_keys() {
    let mut c = content(
        r("j", &["handle"]),
        returns_null("chosen"),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::Null,
    );
    let Operation::Join { member_keys, .. } = &mut c.playbook.steps.get_mut("read").unwrap().op
    else {
        unreachable!()
    };
    member_keys.clear();
    let issues = draft(&c);
    assert!(
        has_issue(
            &issues,
            "join_member_type",
            "steps.read",
            "members[0]: `j.handle`"
        ),
        "{issues:?}"
    );
    assert!(stored_verifies(&c));
}

#[test]
fn every_bad_member_is_reported_at_once() {
    let mut c = content(
        r("j", &["handle"]),
        returns_null("chosen"),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::Null,
    );
    let Operation::Join {
        members,
        member_keys,
        ..
    } = &mut c.playbook.steps.get_mut("read").unwrap().op
    else {
        unreachable!()
    };
    members.push(r("vars", &["j", "handle"]));
    member_keys.push("d".into());
    let issues = draft(&c);
    for text in [
        "members[0] (key c): `j.handle`",
        "members[1] (key d): `vars.j.handle`",
    ] {
        assert!(
            has_issue(&issues, "join_member_type", "steps.read", text),
            "{text}: {issues:?}"
        );
    }
}

#[test]
fn a_synchronous_result_is_not_offered_as_a_handle() {
    let mut c = content(
        r("j", &["handle"]),
        returns_null("chosen"),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::Null,
    );
    c.registry
        .commands
        .get_mut("judgment.request")
        .unwrap()
        .asynchronous = None;
    let issues = c.draft_issues(&[]);
    assert!(
        has_issue(
            &issues,
            "join_member_type",
            "steps.read",
            "whose command judgment.request is synchronous"
        ),
        "{issues:?}"
    );
    assert!(!issues.iter().any(|i| i.message.contains(r#""path":[]"#)));
}

#[test]
fn no_match_and_timeout_edges_do_not_see_a_narrowed_answer() {
    let issues = content(
        r("j", &[]),
        returns_choice(),
        returns_choice(),
        returns_choice(),
        Schema::string(512),
    )
    .validate_structure();
    for step in ["steps.unanswered", "steps.late"] {
        assert!(
            has_issue(&issues, "expression_type", step, ""),
            "{step}: {issues:?}"
        );
    }
    assert!(
        !issues.iter().any(|i| i.path == "steps.chosen"),
        "{issues:?}"
    );
}

#[test]
fn a_read_the_answer_cannot_have_names_the_reference_and_what_is_there() {
    let issues = content(
        r("j", &[]),
        returns_choice(),
        returns_choice(),
        returns_null("late"),
        Schema::string(512),
    )
    .validate_structure();
    let unanswered = issues
        .iter()
        .find(|i| i.path == "steps.unanswered")
        .unwrap_or_else(|| panic!("{issues:?}"));
    // Step, location, the reference as written, and why it cannot be read here.
    for text in [
        "steps.unanswered.output",
        "`answer.c.output.choice`",
        "`answer.c.output` may be null",
        "`completed` edge",
    ] {
        assert!(
            unanswered.message.contains(text),
            "{text}: {}",
            unanswered.message
        );
    }
    // A field no outcome has: the fields that do exist are listed.
    let issues = content(
        r("j", &[]),
        ret("chosen", r("answer", &["c", "output", "verdict"])),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::string(512),
    )
    .validate_structure();
    assert!(
        has_issue(
            &issues,
            "expression_type",
            "steps.chosen",
            "unknown field verdict: `answer.c.output` has fields choice, provenance, rationale, reference, subject"
        ),
        "{issues:?}"
    );
}

/// The "ask, then read the answer" example in FORMAT.md, with its `return`
/// steps added and the real fingerprint filled in, is a valid playbook.
#[test]
fn the_format_example_validates() {
    let format = include_str!("../FORMAT.md");
    let start = format.find("    \"ask\":").expect("example in FORMAT.md");
    let end = start + format[start..].find("\n\n").unwrap();
    let fingerprint = registry().commands["judgment.request"]
        .fingerprint()
        .unwrap();
    let block = format[start..end].replace("sha256:…", &fingerprint);
    let mut steps: BTreeMap<String, Step> =
        serde_json::from_str(&format!("{{{block}}}")).expect("example steps parse");
    for outcome in ["approved", "declined", "unanswered", "late", "error"] {
        steps.insert(outcome.into(), returns_null(outcome));
    }
    let mut c = content(
        r("j", &[]),
        returns_null("chosen"),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::Null,
    );
    c.playbook.steps = steps;
    c.playbook.entries.get_mut("main").unwrap().outcomes =
        ["approved", "declined", "unanswered", "late", "error"]
            .into_iter()
            .map(|o| (o.to_string(), Schema::Null))
            .collect();
    assert_eq!(draft(&c), vec![]);
    let Operation::Join { members, .. } = &c.playbook.steps["read"].op else {
        panic!("read is a join")
    };
    assert_eq!(members, &vec![r("j", &[])]);
}

#[test]
fn a_field_only_some_alternatives_have_names_the_common_fields() {
    let mut c = content(
        r("j", &[]),
        ret("chosen", r("input", &["choice"])),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::string(512),
    );
    let object = |fields: &[&str]| {
        Schema::object(
            fields
                .iter()
                .map(|f| (f.to_string(), Schema::string(512)))
                .collect(),
        )
    };
    c.playbook.entries.get_mut("main").unwrap().input = Schema::Union {
        variants: vec![
            object(&["choice", "reference"]),
            object(&["reason", "reference"]),
        ],
    };
    let issues = c.validate_structure();
    assert!(
        has_issue(
            &issues,
            "expression_type",
            "steps.chosen",
            "`input.choice` cannot be read: `input` has a different shape per alternative and only some of them have `choice`; every alternative has fields reference."
        ),
        "{issues:?}"
    );
}

fn accepting(c: &mut ArtifactContent, outcomes: &[&str]) {
    let Operation::Join {
        accepted_outcomes, ..
    } = &mut c.playbook.steps.get_mut("read").unwrap().op
    else {
        unreachable!()
    };
    *accepted_outcomes = outcomes.iter().map(|o| o.to_string()).collect();
}
/// The draft refuses the read at `step` (and only there, once) with a message
/// containing every one of `texts`; the same content stored still verifies.
fn refused_only_as_a_draft(c: &ArtifactContent, step: &str, texts: &[&str]) {
    assert_eq!(c.validate_structure(), vec![], "ordinary typing accepts it");
    assert!(stored_verifies(c));
    let issues = draft(c);
    assert_eq!(issues.len(), 1, "{issues:?}");
    let issue = &issues[0];
    assert_eq!(
        (issue.code.as_str(), issue.path.as_str()),
        ("expression_type", step)
    );
    for text in texts {
        assert!(issue.message.contains(text), "{text}: {}", issue.message);
    }
}

#[test]
fn a_join_accepting_every_outcome_cannot_read_a_field_only_one_has() {
    // Probe P1: `accepted_outcomes: []` qualifies every outcome on `completed`.
    let mut c = content(
        r("j", &[]),
        returns_choice(),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::string(512),
    );
    accepting(&mut c, &[]);
    refused_only_as_a_draft(
        &c,
        "steps.chosen",
        &[
            "steps.chosen.output: `answer.c.output.choice` is not there on every way to this step",
            "outcome `failed` of member `c` on edge `completed` of join `read` has no `choice` in `answer.c.output`",
            "outcome `refused` of member `c` on edge `completed` of join `read` has no `choice`",
            "`accepted_outcomes` lists only outcomes that have it",
            "`exists`",
        ],
    );
    // Probe P2: two accepted outcomes of different shapes.
    accepting(&mut c, &["assessed", "refused"]);
    refused_only_as_a_draft(&c, "steps.chosen", &["outcome `refused` of member `c`"]);
    // A field both accepted outcomes have stays readable.
    c.playbook.steps.insert(
        "chosen".into(),
        ret("chosen", r("answer", &["c", "output", "reference"])),
    );
    c.playbook
        .entries
        .get_mut("main")
        .unwrap()
        .outcomes
        .insert("chosen".into(), Schema::string(4096));
    assert_eq!(draft(&c), vec![]);
}

#[test]
fn where_two_outcome_edges_meet_a_field_only_one_has_is_refused() {
    // Probe P3: `assessed` and `failed` of one invoke lead to one step.
    let mut c = content(
        r("j", &[]),
        returns_choice(),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::string(512),
    );
    let ask = c.playbook.steps.get_mut("ask").unwrap();
    ask.edges.insert("assessed".into(), "either".into());
    ask.edges.insert("failed".into(), "either".into());
    c.playbook
        .steps
        .insert("either".into(), ret("chosen", r("j", &["choice"])));
    refused_only_as_a_draft(
        &c,
        "steps.either",
        &[
            "`j.choice`",
            "outcome `failed` of step `ask` has no `choice` in `j`",
            "after the outcome edge of the step that returns it",
        ],
    );
}

#[test]
fn on_timeout_a_member_that_has_not_answered_has_no_key() {
    // Probe P5: `timeout` alone reads `answer.c.participant`.
    let c = content(
        r("j", &[]),
        returns_choice(),
        returns_null("unanswered"),
        ret("late", r("answer", &["c", "participant"])),
        Schema::string(512),
    );
    let mut c = c;
    c.playbook
        .entries
        .get_mut("main")
        .unwrap()
        .outcomes
        .insert("late".into(), Schema::string(128));
    refused_only_as_a_draft(
        &c,
        "steps.late",
        &[
            "on edge `timeout` of join `read`, when that member has not answered, `answer.c` is absent",
        ],
    );
    // Probe P6: `completed` and `timeout` meet at one step.
    let mut c = content(
        r("j", &[]),
        ret("chosen", r("answer", &["c", "participant"])),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::string(128),
    );
    c.playbook
        .steps
        .get_mut("read")
        .unwrap()
        .edges
        .insert("timeout".into(), "chosen".into());
    c.playbook.steps.remove("late");
    refused_only_as_a_draft(&c, "steps.chosen", &["`answer.c` is absent"]);
}

#[test]
fn exists_may_test_what_a_lookup_key_may_not_read() {
    let mut c = content(
        r("j", &[]),
        ret(
            "chosen",
            Expr::Exists {
                value: Box::new(choice()),
            },
        ),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::Boolean,
    );
    accepting(&mut c, &[]);
    assert_eq!(draft(&c), vec![]);
    // Not exempt as a lookup key: a missing key would silently take the default.
    let table = Expr::Lookup {
        key: Box::new(choice()),
        cases: vec![LookupCase {
            when: json!("approve"),
            then: lit(json!("yes")),
        }],
        default: Box::new(lit(json!("no"))),
    };
    c.playbook
        .steps
        .insert("chosen".into(), ret("chosen", table.clone()));
    c.playbook
        .entries
        .get_mut("main")
        .unwrap()
        .outcomes
        .insert("chosen".into(), Schema::string(8));
    refused_only_as_a_draft(
        &c,
        "steps.chosen",
        &[
            "steps.chosen.output.lookup.key: `answer.c.output.choice`",
            "takes its default",
        ],
    );
    // Not even inside `exists`.
    c.playbook.steps.insert(
        "chosen".into(),
        ret(
            "chosen",
            Expr::Exists {
                value: Box::new(table),
            },
        ),
    );
    c.playbook
        .entries
        .get_mut("main")
        .unwrap()
        .outcomes
        .insert("chosen".into(), Schema::Boolean);
    refused_only_as_a_draft(&c, "steps.chosen", &["exists.value.lookup.key"]);
}

#[test]
fn a_field_optional_in_its_one_outcome_stays_readable_as_a_lookup_key() {
    let mut c = content(
        r("j", &[]),
        ret(
            "chosen",
            Expr::Lookup {
                key: Box::new(r("answer", &["c", "output", "rationale"])),
                cases: vec![LookupCase {
                    when: json!("covered"),
                    then: lit(json!("yes")),
                }],
                default: Box::new(lit(json!("no"))),
            },
        ),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::string(8),
    );
    let judgment = c.registry.commands.get_mut("judgment.request").unwrap();
    let Some(Schema::Object { required, .. }) = judgment.outcomes.get_mut("assessed") else {
        unreachable!()
    };
    assert!(required.remove("rationale"));
    let fingerprint = judgment.fingerprint().unwrap();
    let Operation::Invoke { command, .. } = &mut c.playbook.steps.get_mut("ask").unwrap().op else {
        unreachable!()
    };
    command.fingerprint = fingerprint;
    assert_eq!(draft(&c), vec![]);
}

#[test]
fn a_nullable_field_that_is_not_a_join_answer_gets_no_join_advice() {
    let mut c = content(
        r("j", &[]),
        returns_null("chosen"),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::string(256),
    );
    c.playbook
        .steps
        .get_mut("ask")
        .unwrap()
        .edges
        .insert("failed".into(), "chosen".into());
    c.playbook.steps.insert(
        "chosen".into(),
        ret("chosen", r("j", &["provenance", "model"])),
    );
    let issues = c.validate_structure();
    let issue = issues
        .iter()
        .find(|i| i.path == "steps.chosen")
        .unwrap_or_else(|| panic!("{issues:?}"));
    assert!(
        issue
            .message
            .contains("`j.provenance` may be null at this step"),
        "{}",
        issue.message
    );
    assert!(issue.message.contains("`exists`"), "{}", issue.message);
    assert!(!issue.message.contains("join"), "{}", issue.message);
}

/// `peek` decides on `predicate`; every edge leads to `error`.
fn peek(predicate: Expr) -> Step {
    Step {
        op: Operation::Decide { predicate },
        edges: edges(&[("true", "error"), ("false", "error"), ("unknown", "error")]),
        on_error: None,
    }
}
fn handle_is(value: &str) -> Expr {
    Expr::Eq {
        left: Box::new(r("j", &["handle"])),
        right: Box::new(lit(json!(value))),
    }
}
fn base() -> ArtifactContent {
    content(
        r("j", &[]),
        returns_choice(),
        returns_null("unanswered"),
        returns_null("late"),
        Schema::string(512),
    )
}

#[test]
fn a_read_of_a_binding_after_its_step_failed_is_refused_in_a_draft() {
    // `ask` binds `j` on its outcome edges; its `on_error` leads to `peek`,
    // where `j` was never bound.
    let with = |predicate: Expr| {
        let mut c = base();
        c.playbook.steps.get_mut("ask").unwrap().on_error = Some("peek".into());
        c.playbook.steps.insert("peek".into(), peek(predicate));
        c
    };
    let c = with(handle_is("h"));
    // Ordinary typing accepts it, so a stored artifact keeps loading.
    assert!(
        c.validate_structure()
            .iter()
            .all(|i| i.path != "steps.peek"),
        "{:?}",
        c.validate_structure()
    );
    assert!(stored_verifies(&c));
    let issues = draft(&c);
    assert_eq!(issues.len(), 1, "{issues:?}");
    let message = &issues[0].message;
    assert_eq!(issues[0].code, "expression_type");
    assert_eq!(issues[0].path, "steps.peek");
    assert!(
        message.starts_with("steps.peek.predicate.eq.left: `j.handle` is not there on every way"),
        "{message}"
    );
    assert!(
        message.contains("on the `on_error` exit of step `ask`, `j` is not bound"),
        "{message}"
    );
    // `exists` may test it.
    let tested = with(Expr::Exists {
        value: Box::new(r("j", &["handle"])),
    });
    assert_eq!(draft(&tested), vec![]);
    // A lookup key is not exempt: a missing key would take the default silently.
    let keyed = with(Expr::Lookup {
        key: Box::new(r("j", &["handle"])),
        cases: vec![LookupCase {
            when: json!("h"),
            then: lit(json!(true)),
        }],
        default: Box::new(lit(json!(false))),
    });
    let issues = draft(&keyed);
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert!(
        issues[0]
            .message
            .contains("a lookup key is held to this too"),
        "{}",
        issues[0].message
    );
    // Through `vars` too.
    let through_vars = with(Expr::Eq {
        left: Box::new(r("vars", &["j", "handle"])),
        right: Box::new(lit(json!("h"))),
    });
    assert!(has_issue(
        &draft(&through_vars),
        "expression_type",
        "steps.peek",
        "`j` is not bound"
    ));
}

#[test]
fn a_read_of_a_binding_reached_around_its_step_is_refused_in_a_draft() {
    // `gate` leads to `ask` or straight to `peek`; `ask`'s `accepted` edge
    // also leads to `peek`, so `j` is bound on one way and not on the other.
    let mut c = base();
    c.playbook.entries.get_mut("main").unwrap().start = "gate".into();
    c.playbook.steps.insert(
        "gate".into(),
        Step {
            op: Operation::Decide {
                predicate: lit(json!(true)),
            },
            edges: edges(&[("true", "ask"), ("false", "peek"), ("unknown", "error")]),
            on_error: None,
        },
    );
    c.playbook
        .steps
        .get_mut("ask")
        .unwrap()
        .edges
        .insert("accepted".into(), "peek".into());
    c.playbook.steps.insert("peek".into(), {
        let mut s = peek(handle_is("h"));
        s.edges = edges(&[("true", "read"), ("false", "error"), ("unknown", "error")]);
        s
    });
    assert!(stored_verifies(&c));
    let issues = draft(&c);
    let at_peek = issues
        .iter()
        .find(|i| i.path == "steps.peek")
        .unwrap_or_else(|| panic!("{issues:?}"));
    assert!(
        at_peek.message.contains(
            "on the way from the start of entry `main` that does not pass step `ask`, `j` is not bound"
        ),
        "{}",
        at_peek.message
    );
    // The join after it reads the handle on the same ways.
    assert!(has_issue(
        &issues,
        "expression_type",
        "steps.read",
        "`j` is not bound"
    ));
    // Bound on every way, it is accepted.
    c.playbook
        .steps
        .get_mut("gate")
        .unwrap()
        .edges
        .insert("false".into(), "ask".into());
    assert_eq!(draft(&c), vec![]);
}

fn let_(name: &str, value: Expr, body: Expr) -> Expr {
    Expr::Let {
        bindings: vec![LetBinding {
            name: name.into(),
            value,
        }],
        body: Box::new(body),
    }
}
/// `content` whose `chosen` step returns `output`, typed `chosen_type`.
fn choosing(output: Expr, chosen_type: Schema, accepted: &[&str]) -> ArtifactContent {
    let mut c = content(
        r("j", &[]),
        ret("chosen", output),
        returns_null("unanswered"),
        returns_null("late"),
        chosen_type,
    );
    accepting(&mut c, accepted);
    c
}

#[test]
fn a_read_through_a_let_alias_is_held_to_the_same_check() {
    // Probe F4: the alias of `answer.c.output` read for `choice`, where every
    // outcome is accepted, is refused exactly as the direct read is.
    let alias = || let_("out", r("answer", &["c", "output"]), r("out", &["choice"]));
    let c = choosing(alias(), Schema::string(512), &[]);
    refused_only_as_a_draft(
        &c,
        "steps.chosen",
        &[
            "`out.choice` (`out` is `answer.c.output`) is not there on every way to this step",
            "outcome `failed` of member `c` on edge `completed` of join `read` has no `choice` in `answer.c.output`",
            "outcome `refused` of member `c` on edge `completed` of join `read` has no `choice`",
        ],
    );
    let direct = choosing(choice(), Schema::string(512), &[]);
    let lacks = |c: &ArtifactContent| {
        let message = draft(c)[0].message.clone();
        message.split_once(": ").unwrap().1.to_string()
    };
    let (alias_reasons, direct_reasons) = (lacks(&c), lacks(&direct));
    let ways = |m: &str| m.split_once("this step: ").unwrap().1.to_string();
    assert_eq!(ways(&alias_reasons), ways(&direct_reasons));
    // Probe F6: as a lookup key the alias would take the default silently.
    let keyed = choosing(
        let_(
            "out",
            r("answer", &["c", "output"]),
            Expr::Lookup {
                key: Box::new(r("out", &["choice"])),
                cases: vec![LookupCase {
                    when: json!("approve"),
                    then: lit(json!("a")),
                }],
                default: Box::new(lit(json!("b"))),
            },
        ),
        Schema::string(512),
        &[],
    );
    refused_only_as_a_draft(
        &keyed,
        "steps.chosen",
        &["a lookup key is held to this too"],
    );
    // An alias of a field every way has is fine, and so is an alias of an
    // alias.
    assert_eq!(
        draft(&choosing(alias(), Schema::string(512), &["assessed"])),
        vec![]
    );
    let twice = let_(
        "a",
        r("answer", &["c"]),
        let_("b", r("a", &["output"]), r("b", &["choice"])),
    );
    assert_eq!(
        draft(&choosing(twice.clone(), Schema::string(512), &["assessed"])),
        vec![]
    );
    refused_only_as_a_draft(
        &choosing(twice, Schema::string(512), &[]),
        "steps.chosen",
        &[
            "(`b` is `answer.c.output`)",
            "outcome `failed` of member `c`",
        ],
    );
    // Probe F7: an alias used only inside `exists` is fine.
    let tested = let_(
        "x",
        choice(),
        Expr::Exists {
            value: Box::new(r("x", &[])),
        },
    );
    assert_eq!(draft(&choosing(tested, Schema::Boolean, &[])), vec![]);
}

#[test]
fn an_item_of_a_collection_derived_from_an_outcome_is_held_to_the_same_check() {
    // Probe F5: a `map` item that is `answer.c.output`, read for `choice`.
    let list = Schema::Array {
        items: Box::new(Schema::string(512)),
        max_items: 1,
    };
    let over = |make: fn(Box<Expr>, Box<Expr>) -> Expr| {
        make(
            Box::new(Expr::Array {
                items: vec![r("answer", &["c", "output"])],
            }),
            Box::new(r("o", &["choice"])),
        )
    };
    let map = |collection, value| Expr::Map {
        collection,
        item: "o".into(),
        value,
        max_items: 1,
    };
    refused_only_as_a_draft(
        &choosing(over(map), list.clone(), &[]),
        "steps.chosen",
        &[
            "`o.choice` (`o` is an item of its collection) is not there on every way",
            "outcome `refused` of member `c` on edge `completed` of join `read` has no `choice`",
        ],
    );
    assert_eq!(
        draft(&choosing(over(map), list.clone(), &["assessed"])),
        vec![]
    );
    // A `filter` predicate likewise.
    let filter = |collection, value: Box<Expr>| Expr::Filter {
        collection,
        item: "o".into(),
        predicate: Box::new(Expr::Eq {
            left: value,
            right: Box::new(lit(json!("approve"))),
        }),
        max_items: 1,
    };
    let filtered = choosing(
        over(filter),
        Schema::Array {
            items: Box::new(Schema::object(BTreeMap::new())),
            max_items: 1,
        },
        &[],
    );
    let issues = draft(&filtered);
    assert!(
        issues.iter().any(
            |i| i.code == "expression_type" && i.message.contains("`o.choice` (`o` is an item")
        ),
        "{issues:?}"
    );
}

#[test]
fn an_item_named_like_a_binding_gets_no_second_unbound_diagnostic() {
    // A3 re-review m1: `error` is reached where `answer` was never bound; an
    // item named `answer` there is refused by typing alone.
    let mut c = base();
    c.playbook.steps.insert(
        "error".into(),
        ret(
            "error",
            Expr::Map {
                collection: Box::new(lit(json!(["x"]))),
                item: "answer".into(),
                value: Box::new(r("answer", &[])),
                max_items: 1,
            },
        ),
    );
    let issues = draft(&c);
    assert!(!issues.is_empty());
    assert!(
        issues.iter().all(|i| !i.message.contains("is not bound")),
        "{issues:?}"
    );
}

#[test]
fn a_single_member_any_accepted_join_reads_its_member_on_completed() {
    // A2 re-review R1-M1: with one member, that member is the one that
    // qualified, so `answer.c` is there on `completed`.
    let mut c = base();
    let Operation::Join { mode, .. } = &mut c.playbook.steps.get_mut("read").unwrap().op else {
        unreachable!()
    };
    *mode = JoinMode::AnyAccepted;
    assert_eq!(c.validate_structure(), vec![]);
    assert_eq!(draft(&c), vec![]);
    // The member's outcome is still held to the accepted outcomes.
    accepting(&mut c, &["assessed", "refused"]);
    let issues = draft(&c);
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert!(
        issues[0]
            .message
            .contains("outcome `refused` of member `c`"),
        "{}",
        issues[0].message
    );
    assert!(
        !issues[0].message.contains("another member"),
        "{}",
        issues[0].message
    );
}
