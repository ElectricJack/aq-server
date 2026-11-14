//! `lookup` and `let` expressions: evaluation, typing, structural bounds and
//! the stability of artifacts that do not use them.
use aq_playbooks_model::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

const LEVELS: [&str; 3] = ["complex-high", "simple-high", "standard-high"];

fn lit(v: Value) -> Expr {
    Expr::literal(v)
}
fn r(root: &str, path: &[&str]) -> Expr {
    Expr::Ref {
        root: root.into(),
        path: path.iter().map(|s| s.to_string()).collect(),
    }
}
fn case(when: Value, then: Expr) -> LookupCase {
    LookupCase { when, then }
}
fn lookup(key: Expr, cases: Vec<LookupCase>, default: Expr) -> Expr {
    Expr::Lookup {
        key: Box::new(key),
        cases,
        default: Box::new(default),
    }
}
fn bind(name: &str, value: Expr) -> LetBinding {
    LetBinding {
        name: name.into(),
        value,
    }
}
fn let_(bindings: Vec<LetBinding>, body: Expr) -> Expr {
    Expr::Let {
        bindings,
        body: Box::new(body),
    }
}
fn lt(left: Expr, right: Expr) -> Expr {
    Expr::Lt {
        left: Box::new(left),
        right: Box::new(right),
    }
}
fn ev(expr: &Expr, context: &EvalContext) -> EvalValue {
    eval(expr, context, &EvalLimits::default()).unwrap()
}
fn known(v: Value) -> EvalValue {
    EvalValue::Known(v)
}
fn input_context(input: Value) -> EvalContext {
    BTreeMap::from([("input".into(), EvalValue::Known(input))])
}
/// The kind table used across tests: three named kinds, default Standard.
fn kind_table(key: Expr) -> Expr {
    lookup(
        key,
        vec![
            case(json!("chore"), lit(json!("simple-high"))),
            case(json!("design"), lit(json!("complex-high"))),
            case(json!("bugfix"), lit(json!("standard-high"))),
        ],
        lit(json!("standard-high")),
    )
}
fn level_schema(values: &[&str]) -> Schema {
    Schema::String {
        max_length: values.iter().map(|v| v.len()).max().unwrap(),
        values: values.iter().map(|v| v.to_string()).collect(),
    }
}

/// Every field is required except `hint`, an optional integer fact.
fn entry_input() -> Schema {
    let Schema::Object {
        mut properties,
        required,
    } = required_input()
    else {
        unreachable!()
    };
    properties.insert(
        "hint".into(),
        Schema::Integer {
            minimum: 0,
            maximum: 8,
        },
    );
    Schema::Object {
        properties,
        required,
    }
}
fn required_input() -> Schema {
    Schema::object(BTreeMap::from([
        (
            "kind".into(),
            Schema::String {
                max_length: 16,
                values: ["bugfix", "chore", "design", "research"]
                    .map(String::from)
                    .into(),
            },
        ),
        (
            "risk".into(),
            Schema::String {
                max_length: 16,
                values: ["high", "low", "medium"].map(String::from).into(),
            },
        ),
        ("narrow".into(), Schema::Boolean),
        (
            "rank".into(),
            Schema::Integer {
                minimum: 0,
                maximum: 1000,
            },
        ),
        ("note".into(), Schema::string(4096)),
    ]))
}
fn route_command() -> CommandContract {
    CommandContract {
        name: "work.route".into(),
        version: "1.0.0".into(),
        input: Schema::object(BTreeMap::from([("level".into(), level_schema(&LEVELS))])),
        outcomes: BTreeMap::from([("ok".into(), Schema::Null)]),
        capabilities: BTreeSet::new(),
        scopes: BTreeSet::from(["project".into()]),
        effect: Effect::Observation,
        recovery: Recovery::Observation,
        asynchronous: None,
        cancellation: false,
        public_result_fields: BTreeSet::new(),
        sensitive_input_fields: BTreeSet::new(),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 1024,
        deadline_ms: 1000,
    }
}
/// One entry: `route` invokes `work.route` with `route_input` and binds
/// `routed`; `finish` returns `output` as outcome `done` typed `done`.
fn content(route_input: Expr, output: Expr, done: Schema) -> ArtifactContent {
    let c = route_command();
    let header = SourceHeader {
        format_version: FORMAT_VERSION,
        id: "lookups".into(),
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
            id: "lookups".into(),
            purpose: "lookup and let".into(),
            scope: "project".into(),
            entries: BTreeMap::from([(
                "main".into(),
                EntryPoint {
                    input: entry_input(),
                    outcomes: BTreeMap::from([("done".into(), done)]),
                    start: "route".into(),
                },
            )]),
            steps: BTreeMap::from([
                (
                    "route".into(),
                    Step {
                        op: Operation::Invoke {
                            command: ContractRef {
                                name: c.name.clone(),
                                fingerprint: c.fingerprint().unwrap(),
                            },
                            input: route_input,
                            bind: "routed".into(),
                            retry: None,
                        },
                        edges: BTreeMap::from([("ok".into(), "finish".into())]),
                        on_error: Some("finish".into()),
                    },
                ),
                (
                    "finish".into(),
                    Step {
                        op: Operation::Return {
                            outcome: "done".into(),
                            output,
                        },
                        edges: BTreeMap::new(),
                        on_error: None,
                    },
                ),
            ]),
            triggers: vec![],
            limits: Limits::default(),
            required_capabilities: BTreeSet::new(),
        },
        registry: Registry {
            commands: BTreeMap::from([(c.name.clone(), c)]),
            events: BTreeMap::new(),
        },
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::new(),
        source_map: BTreeMap::new(),
    }
}
fn valid_route_input() -> Expr {
    Expr::Object {
        fields: BTreeMap::from([("level".into(), lit(json!("standard-high")))]),
    }
}
/// Issues for an artifact whose `finish` step returns `output` typed `done`.
fn returning(output: Expr, done: Schema) -> Vec<ValidationIssue> {
    content(valid_route_input(), output, done).validate_structure()
}
fn has_issue(issues: &[ValidationIssue], code: &str, path: &str, text: &str) -> bool {
    issues
        .iter()
        .any(|i| i.code == code && i.path == path && i.message.contains(text))
}

#[test]
fn lookup_evaluates_only_the_matching_case() {
    let ctx = input_context(json!({"kind": "bugfix"}));
    let table = lookup(
        r("input", &["kind"]),
        vec![
            case(json!("chore"), r("absent", &["level"])),
            case(json!("bugfix"), lit(json!("standard-high"))),
        ],
        r("absent", &[]),
    );
    assert_eq!(ev(&table, &ctx), known(json!("standard-high")));
    // An unevaluated case still costs its place in the table: a lookup charges
    // cases.len() to the element budget on top of its own node.
    let limits = |max_elements| EvalLimits {
        max_elements,
        ..EvalLimits::default()
    };
    // Nodes: lookup, key ref, chosen literal = 3; cases = 2.
    assert!(eval(&table, &ctx, &limits(5)).is_ok());
    assert!(eval(&table, &ctx, &limits(4)).is_err());
}

#[test]
fn lookup_default_on_no_match_and_unresolved_key_propagates() {
    let ctx = BTreeMap::from([
        ("input".into(), known(json!({"kind": "zzz"}))),
        ("stale".into(), EvalValue::Unknown("revision".into())),
    ]);
    assert_eq!(
        ev(&kind_table(r("input", &["kind"])), &ctx),
        known(json!("standard-high"))
    );
    assert_eq!(
        ev(&kind_table(r("stale", &[])), &ctx),
        EvalValue::Unknown("revision".into())
    );
    // A Missing key takes the default, as a key that matches no case does.
    assert_eq!(
        ev(&kind_table(r("input", &["missing"])), &ctx),
        known(json!("standard-high"))
    );
    // An Unknown key never falls through to the default, even when the
    // default is a literal that could be produced without the key.
    assert_eq!(
        ev(
            &lookup(r("stale", &[]), vec![], lit(json!("standard-high"))),
            &ctx
        ),
        EvalValue::Unknown("revision".into())
    );
    // lookup(optional_hint, ..., default 0) is 0 when the hint is absent.
    let hint = lookup(
        r("input", &["hint"]),
        vec![case(json!(4), lit(json!(4))), case(json!(7), lit(json!(7)))],
        lit(json!(0)),
    );
    let rank = Schema::Integer {
        minimum: 0,
        maximum: 8,
    };
    assert!(returning(hint.clone(), rank).is_empty());
    assert_eq!(ev(&hint, &input_context(json!({}))), known(json!(0)));
    assert_eq!(
        ev(&hint, &input_context(json!({"hint": 7}))),
        known(json!(7))
    );
    assert_eq!(
        ev(&hint, &input_context(json!({"hint": 5}))),
        known(json!(0))
    );
}

#[test]
fn lookup_result_type_is_the_union_of_its_results() {
    let output = kind_table(r("input", &["kind"]));
    let issues = returning(output.clone(), level_schema(&LEVELS));
    assert!(issues.is_empty(), "{issues:?}");
    // Each level is in the inferred enum: dropping any one from the target
    // makes the lookup unassignable.
    for missing in LEVELS {
        let rest: Vec<_> = LEVELS.into_iter().filter(|l| *l != missing).collect();
        let issues = returning(output.clone(), level_schema(&rest));
        assert!(
            has_issue(&issues, "schema_compatibility", "steps.finish", ""),
            "{missing}: {issues:?}"
        );
    }
    // The enum is closed: an open string of the same length accepts it, an
    // enum-typed target narrower in length does not.
    assert!(returning(output.clone(), Schema::string(13)).is_empty());
    let mut short = level_schema(&LEVELS);
    if let Schema::String { max_length, .. } = &mut short {
        *max_length = 12;
    }
    assert!(!returning(output.clone(), short).is_empty());
    // Nested inside another expression the lookup is typed by inference.
    let nested = Expr::Contains {
        collection: Box::new(lit(json!(["complex-high"]))),
        value: Box::new(output),
    };
    assert!(returning(nested, Schema::Boolean).is_empty());
}

#[test]
fn lookup_refuses_duplicate_incompatible_or_too_many_cases() {
    let target = Schema::string(16);
    let key = || r("input", &["kind"]);
    let s = |v: &str| lit(json!(v));
    // Duplicate canonical `when`.
    let duplicate = lookup(
        key(),
        vec![
            case(json!("bugfix"), s("a")),
            case(json!("chore"), s("b")),
            case(json!("bugfix"), s("c")),
        ],
        s("d"),
    );
    let issues = returning(duplicate, target.clone());
    assert!(
        has_issue(
            &issues,
            "lookup_case",
            "steps.finish",
            "steps.finish.output.lookup.cases[2].when: \"bugfix\" equals cases[0].when"
        ),
        "{issues:?}"
    );
    // Canonically equal objects are duplicates whatever their key order.
    let objects = lookup(
        lit(json!({"a": 1, "b": 2})),
        vec![
            case(serde_json::from_str(r#"{"a":1,"b":2}"#).unwrap(), s("x")),
            case(serde_json::from_str(r#"{"b":2,"a":1}"#).unwrap(), s("y")),
        ],
        s("z"),
    );
    assert!(has_issue(
        &returning(objects, target.clone()),
        "lookup_case",
        "steps.finish",
        "cases[1].when: {\"a\":1,\"b\":2} equals cases[0].when"
    ));
    // A `when` the key's type cannot take: integer against a string key.
    let incompatible = lookup(key(), vec![case(json!(1), s("a"))], s("b"));
    let issues = returning(incompatible, target.clone());
    assert!(
        has_issue(
            &issues,
            "expression_type",
            "steps.finish",
            "steps.finish.output.lookup.cases[0].when: `when` 1 can never match: the key (output.lookup.key) has type string, one of \"bugfix\", \"chore\", \"design\", \"research\""
        ),
        "{issues:?}"
    );
    // At most 256 cases.
    let ranks = |n: i64| {
        lookup(
            r("input", &["rank"]),
            (0..n).map(|i| case(json!(i), s("x"))).collect(),
            s("y"),
        )
    };
    assert!(returning(ranks(256), target.clone()).is_empty());
    let issues = returning(ranks(257), target.clone());
    assert!(
        has_issue(&issues, "expression_limit", "steps.finish", "257 cases"),
        "{issues:?}"
    );
    // Each `when` is at most 1024 canonical bytes (a string's two quotes count).
    let sized = |bytes: usize| {
        lookup(
            r("input", &["note"]),
            vec![case(json!("n".repeat(bytes - 2)), s("x"))],
            s("y"),
        )
    };
    assert!(returning(sized(1024), target.clone()).is_empty());
    let issues = returning(sized(1025), target);
    assert!(
        has_issue(
            &issues,
            "expression_limit",
            "steps.finish",
            "is 1025 canonical bytes"
        ),
        "{issues:?}"
    );
}

#[test]
fn lookup_keys_may_be_arrays_and_integers() {
    let pair = lookup(
        Expr::Array {
            items: vec![r("input", &["risk"]), r("input", &["narrow"])],
        },
        vec![
            case(json!(["high", true]), lit(json!("standard-high"))),
            case(json!(["high", false]), lit(json!("complex-high"))),
        ],
        lit(json!("simple-high")),
    );
    let ranks = lookup(
        r("input", &["rank"]),
        vec![
            case(json!(1), lit(json!("simple-high"))),
            case(json!(4), lit(json!("standard-high"))),
            case(json!(7), lit(json!("complex-high"))),
        ],
        lit(json!("standard-high")),
    );
    let ctx = input_context(json!({"risk": "high", "narrow": true, "rank": 7}));
    assert_eq!(ev(&pair, &ctx), known(json!("standard-high")));
    assert_eq!(ev(&ranks, &ctx), known(json!("complex-high")));
    let ctx = input_context(json!({"risk": "high", "narrow": false, "rank": 2}));
    assert_eq!(ev(&pair, &ctx), known(json!("complex-high")));
    assert_eq!(ev(&ranks, &ctx), known(json!("standard-high")));
    for e in [pair, ranks] {
        let issues = returning(e, level_schema(&LEVELS));
        assert!(issues.is_empty(), "{issues:?}");
    }
}

#[test]
fn let_binds_in_order_and_once() {
    let e = let_(
        vec![
            bind("a", r("input", &["kind"])),
            bind("b", kind_table(r("a", &[]))),
        ],
        Expr::Array {
            items: vec![r("a", &[]), r("b", &[])],
        },
    );
    let ctx = input_context(json!({"kind": "chore"}));
    assert_eq!(ev(&e, &ctx), known(json!(["chore", "simple-high"])));
    assert!(
        returning(
            e,
            Schema::Array {
                items: Box::new(Schema::string(16)),
                max_items: 2,
            }
        )
        .is_empty()
    );
    // A binding sees only the bindings before it.
    let forward = let_(
        vec![bind("a", r("b", &[])), bind("b", lit(json!(1)))],
        r("a", &[]),
    );
    assert!(has_issue(
        &returning(
            forward,
            Schema::Integer {
                minimum: 0,
                maximum: 9
            }
        ),
        "expression_type",
        "steps.finish",
        "b"
    ));
    // Each binding is evaluated once, however often the body uses it: an
    // expensive binding used three times fits a budget that three inlined
    // copies do not.
    let costly = Expr::Map {
        collection: Box::new(lit(json!((0..50).collect::<Vec<_>>()))),
        item: "n".into(),
        value: Box::new(r("n", &[])),
        max_items: 50,
    };
    let used_thrice = let_(
        vec![bind("xs", costly.clone())],
        Expr::Array {
            items: vec![r("xs", &[]), r("xs", &[]), r("xs", &[])],
        },
    );
    let inlined = Expr::Array {
        items: vec![costly.clone(), costly.clone(), costly],
    };
    let limits = EvalLimits {
        max_elements: 100,
        ..EvalLimits::default()
    };
    assert!(eval(&used_thrice, &BTreeMap::new(), &limits).is_ok());
    assert!(eval(&inlined, &BTreeMap::new(), &limits).is_err());
}

#[test]
fn let_unknown_binding_is_not_dropped_and_missing_binding_is_testable() {
    let ctx = BTreeMap::from([
        ("input".into(), known(json!({"kind": "chore"}))),
        ("stale".into(), EvalValue::Unknown("revision".into())),
    ]);
    // Unknown is never lost, even when the body does not use the binding,
    // as `and(false, unknown)` stays unknown.
    let unused = let_(
        vec![bind("s", r("stale", &[]))],
        lit(json!("standard-high")),
    );
    assert_eq!(ev(&unused, &ctx), EvalValue::Unknown("revision".into()));
    // Missing is an observation, bound as such: `exists` reads it as false,
    // and any other use propagates Missing exactly as the inlined reference.
    let optional = let_(
        vec![bind("hint", r("input", &["hint"]))],
        Expr::Exists {
            value: Box::new(r("hint", &[])),
        },
    );
    assert_eq!(ev(&optional, &ctx), known(json!(false)));
    let used = let_(vec![bind("hint", r("input", &["hint"]))], r("hint", &[]));
    assert_eq!(ev(&used, &ctx), EvalValue::Missing);
    // Evaluation refuses a name already in the context.
    let shadow = let_(vec![bind("input", lit(json!(1)))], r("input", &[]));
    assert!(eval(&shadow, &ctx, &EvalLimits::default()).is_err());
}

#[test]
fn let_refuses_reserved_and_shadowing_names() {
    let route = |e: Expr| content(e, lit(Value::Null), Schema::Null).validate_structure();
    let body = valid_route_input();
    for name in ["input", "event", "definitions", "vars", "routed"] {
        let issues = route(let_(vec![bind(name, lit(json!(1)))], body.clone()));
        assert!(
            has_issue(&issues, "binding", "steps.route", &format!("`{name}`")),
            "{name}: {issues:?}"
        );
    }
    // A name repeated in one let, or shadowing an enclosing let.
    let repeated = let_(
        vec![bind("a", lit(json!(1))), bind("a", lit(json!(2)))],
        body.clone(),
    );
    assert!(has_issue(&route(repeated), "binding", "steps.route", "`a`"));
    let nested = let_(
        vec![bind("a", lit(json!(1)))],
        let_(vec![bind("a", lit(json!(2)))], body.clone()),
    );
    let issues = route(nested);
    assert!(
        has_issue(
            &issues,
            "binding",
            "steps.route",
            "steps.route.input.let.body.let.bindings[0].name: `a` is already bound"
        ),
        "{issues:?}"
    );
    // At most 16 bindings.
    let many = |n: usize| {
        let_(
            (0..n)
                .map(|i| bind(&format!("b{i}"), lit(json!(i))))
                .collect(),
            body.clone(),
        )
    };
    assert!(route(many(16)).is_empty());
    let issues = route(many(17));
    assert!(
        has_issue(&issues, "expression_limit", "steps.route", "17 bindings"),
        "{issues:?}"
    );
}

#[test]
fn max_of_two_ranks_without_arithmetic() {
    let max = |a: i64, b: i64| {
        let_(
            vec![bind("a", lit(json!(a))), bind("b", lit(json!(b)))],
            lookup(
                lt(r("a", &[]), r("b", &[])),
                vec![case(json!(true), r("b", &[]))],
                r("a", &[]),
            ),
        )
    };
    assert_eq!(ev(&max(5, 7), &BTreeMap::new()), known(json!(7)));
    assert_eq!(ev(&max(7, 5), &BTreeMap::new()), known(json!(7)));
}

#[test]
fn an_invoke_input_using_lookup_and_let_validates_against_an_enum_target() {
    let input: Expr = serde_json::from_value(json!({
        "op": "let",
        "bindings": [
            {"name": "kind", "value": {"op": "ref", "root": "input", "path": ["kind"]}},
            {"name": "base", "value": {
                "op": "lookup",
                "key": {"op": "ref", "root": "kind"},
                "cases": [
                    {"when": "chore", "then": {"op": "literal", "value": "simple-high"}},
                    {"when": "design", "then": {"op": "literal", "value": "complex-high"}}
                ],
                "default": {"op": "literal", "value": "standard-high"}
            }}
        ],
        "body": {"op": "object", "fields": {"level": {"op": "ref", "root": "base"}}}
    }))
    .unwrap();
    let c = content(input.clone(), lit(Value::Null), Schema::Null);
    assert_eq!(c.validate_structure(), vec![]);
    let artifact = Artifact::seal(c).unwrap();
    artifact.verify().unwrap();
    let bytes = canonical_bytes(&artifact).unwrap();
    assert_eq!(Artifact::from_canonical_bytes(&bytes).unwrap(), artifact);
    assert_eq!(
        ev(&input, &input_context(json!({"kind": "chore"}))),
        known(json!({"level": "simple-high"}))
    );
    // A level outside the command's enum is refused at the invoke step.
    let wider = let_(
        vec![bind(
            "level",
            lookup(
                r("input", &["kind"]),
                vec![case(json!("research"), lit(json!("deep-high")))],
                lit(json!("standard-high")),
            ),
        )],
        Expr::Object {
            fields: BTreeMap::from([("level".into(), r("level", &[]))]),
        },
    );
    let issues = content(wider, lit(Value::Null), Schema::Null).validate_structure();
    assert!(
        has_issue(&issues, "schema_compatibility", "steps.route", ""),
        "{issues:?}"
    );
}

/// A chain of `n` lookups, each taking its default on `narrow: false`.
fn lookup_chain(n: usize) -> Expr {
    (0..n).fold(lit(json!("deepest")), |next, _| {
        lookup(
            r("input", &["narrow"]),
            vec![case(json!(true), lit(json!("found")))],
            next,
        )
    })
}
/// `n` nested lets, each binding one distinct name.
fn let_chain(n: usize) -> Expr {
    (0..n).fold(lit(json!("deepest")), |body, i| {
        let_(vec![bind(&format!("v{i}"), lit(json!(i)))], body)
    })
}

#[test]
fn validation_refuses_what_evaluation_would_refuse_for_depth_and_budget() {
    let ctx = input_context(json!({"narrow": false}));
    let limits = EvalLimits::default();
    for chain in [lookup_chain, let_chain] {
        // 32 levels evaluate and validate; 33 fail both ways.
        assert_eq!(ev(&chain(32), &ctx), known(json!("deepest")));
        assert!(returning(chain(32), Schema::string(16)).is_empty());
        assert!(eval(&chain(33), &ctx, &limits).is_err());
        for n in [33, 40] {
            let issues = returning(chain(n), Schema::string(16));
            assert_eq!(issues.len(), 1, "{n}: {issues:?}");
            assert!(
                has_issue(
                    &issues,
                    "expression_limit",
                    "steps.finish",
                    &format!(
                        "steps.finish.output: nests {n} levels deep; evaluation stops past depth 32"
                    )
                ),
                "{n}: {issues:?}"
            );
        }
    }
    // Every case is charged on every map item: 100 items of a 256-case table
    // cannot fit the 10 000-element budget; 30 items can.
    let mapped = |max_items: usize| Expr::Map {
        collection: Box::new(r("input", &["ranks"])),
        item: "n".into(),
        value: Box::new(lookup(
            r("n", &[]),
            (0..256).map(|i| case(json!(i), lit(json!(i)))).collect(),
            lit(json!(0)),
        )),
        max_items,
    };
    let ranks = Schema::Array {
        items: Box::new(Schema::Integer {
            minimum: 0,
            maximum: 255,
        }),
        max_items: 30,
    };
    let mut c = content(valid_route_input(), mapped(100), ranks.clone());
    let Schema::Object { properties, .. } = &mut c.playbook.entries.get_mut("main").unwrap().input
    else {
        unreachable!()
    };
    properties.insert("ranks".into(), ranks.clone());
    let issues = c.validate_structure();
    assert!(
        has_issue(
            &issues,
            "expression_limit",
            "steps.finish",
            "steps.finish.output: may charge up to 25902 elements; evaluation stops past 10000"
        ),
        "{issues:?}"
    );
    let hundred = input_context(json!({"ranks": (0..100).collect::<Vec<_>>()}));
    assert!(eval(&mapped(100), &hundred, &limits).is_err());
    if let Operation::Return { output, .. } = &mut c.playbook.steps.get_mut("finish").unwrap().op {
        *output = mapped(30);
    }
    assert_eq!(c.validate_structure(), vec![]);
    let thirty = input_context(json!({"ranks": (0..30).collect::<Vec<_>>()}));
    // The bound is exact for a full collection: map, collection, and per item
    // the lookup, its key, its chosen branch and its 256 cases.
    let exact = 2 + 30 * (3 + 256);
    let budget = |max_elements| EvalLimits {
        max_elements,
        ..EvalLimits::default()
    };
    assert!(eval(&mapped(30), &thirty, &budget(exact)).is_ok());
    assert!(eval(&mapped(30), &thirty, &budget(exact - 1)).is_err());
    assert_eq!(
        ev(&mapped(30), &thirty),
        known(json!((0..30).collect::<Vec<_>>()))
    );
}

#[test]
fn empty_array_branches_meet_any_array_target() {
    let harnesses = || {
        lookup(
            r("input", &["kind"]),
            vec![
                case(json!("chore"), lit(json!(["codex"]))),
                case(json!("design"), lit(json!([]))),
            ],
            lit(json!([])),
        )
    };
    let without_claude = |collection: Expr| Expr::Filter {
        collection: Box::new(collection),
        item: "h".into(),
        predicate: Box::new(Expr::Not {
            value: Box::new(Expr::Eq {
                left: Box::new(r("h", &[])),
                right: Box::new(lit(json!("claude"))),
            }),
        }),
        max_items: 4,
    };
    let strings = Schema::Array {
        items: Box::new(Schema::string(16)),
        max_items: 4,
    };
    let shapes = [
        harnesses(),
        without_claude(harnesses()),
        let_(vec![bind("hs", harnesses())], r("hs", &[])),
        let_(vec![bind("hs", harnesses())], without_claude(r("hs", &[]))),
        let_(vec![bind("none", lit(json!([])))], r("none", &[])),
        Expr::Array { items: vec![] },
    ];
    for e in shapes {
        let issues = returning(e.clone(), strings.clone());
        assert!(issues.is_empty(), "{e:?}: {issues:?}");
    }
    let e = without_claude(harnesses());
    for (kind, expected) in [
        ("chore", json!(["codex"])),
        ("design", json!([])),
        ("bugfix", json!([])),
    ] {
        assert_eq!(
            ev(&e, &input_context(json!({"kind": kind}))),
            known(expected)
        );
    }
    // Non-empty arrays are checked as before: an integer item is refused.
    let integers = lookup(
        r("input", &["kind"]),
        vec![case(json!("chore"), lit(json!([1])))],
        lit(json!([])),
    );
    let issues = returning(without_claude(integers), strings);
    assert!(
        has_issue(
            &issues,
            "schema_compatibility",
            "steps.finish",
            "steps.finish.output: expression type/bounds not assignable to target schema: it has type array of at most 1 (the integer 1)"
        ),
        "{issues:?}"
    );
}

#[test]
fn diagnostics_name_the_place_the_key_type_and_the_branch_once() {
    let s = |v: &str| lit(json!(v));
    let strings = Schema::object(BTreeMap::from([
        ("first".into(), Schema::string(16)),
        ("second".into(), Schema::string(16)),
    ]));
    // Two tables in one output: the message names the one that is wrong.
    let two = Expr::Object {
        fields: BTreeMap::from([
            (
                "first".into(),
                lookup(r("input", &["rank"]), vec![case(json!(1), s("a"))], s("b")),
            ),
            (
                "second".into(),
                lookup(
                    r("input", &["kind"]),
                    vec![case(json!("Chore"), s("a"))],
                    s("b"),
                ),
            ),
        ]),
    };
    let issues = returning(two, strings);
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(
        issues[0].message,
        "steps.finish.output.second.lookup.cases[0].when: `when` \"Chore\" can never match: \
         the key (output.second.lookup.key) has type string, one of \"bugfix\", \"chore\", \
         \"design\", \"research\". Change the `when` to a value of the key's type, or change the key."
    );
    // An integer key may hold any integer as a case, whatever its declared range.
    let wide = lookup(
        r("input", &["rank"]),
        vec![case(json!(5000), s("a"))],
        s("b"),
    );
    assert!(returning(wide, Schema::string(16)).is_empty());
    // A refused assignment names the branch and its type.
    let deep = lookup(
        r("input", &["kind"]),
        vec![case(json!("research"), s("deep-high"))],
        s("standard-high"),
    );
    let issues = returning(deep, level_schema(&LEVELS));
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(
        issues[0].message,
        "steps.finish.output.lookup.cases[0].then: literal \"deep-high\" is not a value of the \
         target type string, one of \"complex-high\", \"simple-high\", \"standard-high\""
    );
    // Inside a let, and nested in an inferred position, the branch is named too.
    let nested = Expr::Contains {
        collection: Box::new(lit(json!(["a"]))),
        value: Box::new(let_(
            vec![bind(
                "x",
                lookup(r("input", &["kind"]), vec![case(json!(2), s("a"))], s("b")),
            )],
            r("x", &[]),
        )),
    };
    let issues = returning(nested, Schema::Boolean);
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert!(
        issues[0].message.starts_with(
            "steps.finish.output.contains.value.let.bindings[0].value.lookup.cases[0].when: `when` 2 can never match"
        ),
        "{issues:?}"
    );
    // A let name is refused once, by the structural check, with its reason.
    let issues = returning(
        let_(vec![bind("input", lit(json!(1)))], r("input", &[])),
        Schema::Integer {
            minimum: 0,
            maximum: 9,
        },
    );
    assert_eq!(issues.len(), 1, "{issues:?}");
    assert_eq!(
        issues[0].message,
        "steps.finish.output.let.bindings[0].name: `input` is a reserved root (input, event, \
         definitions, vars); choose another name"
    );
}

/// `map` or `filter` over `[0, 1]` whose body is Missing for item `missing`
/// and Unknown for item `unknown`.
fn collection_over(filter: bool, missing: i64, unknown: i64) -> Expr {
    let body = lookup(
        r("i", &[]),
        vec![
            case(json!(missing), r("input", &["absent"])),
            case(json!(unknown), r("stale", &[])),
        ],
        lit(json!(true)),
    );
    let collection = Box::new(lit(json!([0, 1])));
    if filter {
        Expr::Filter {
            collection,
            item: "i".into(),
            predicate: Box::new(body),
            max_items: 2,
        }
    } else {
        Expr::Map {
            collection,
            item: "i".into(),
            value: Box::new(body),
            max_items: 2,
        }
    }
}

#[test]
fn map_and_filter_never_let_a_missing_item_hide_an_unknown_one() {
    let ctx = BTreeMap::from([
        ("input".into(), known(json!({}))),
        ("stale".into(), EvalValue::Unknown("revision".into())),
    ]);
    let stale = EvalValue::Unknown("revision".into());
    for filter in [false, true] {
        // Missing then Unknown, and Unknown then Missing: Unknown either way.
        assert_eq!(ev(&collection_over(filter, 0, 1), &ctx), stale, "{filter}");
        assert_eq!(ev(&collection_over(filter, 1, 0), &ctx), stale, "{filter}");
        // The reviewer's probe: a lookup keyed on it never takes its default.
        let keyed = lookup(
            collection_over(filter, 0, 1),
            vec![case(json!([]), lit(json!("EMPTY")))],
            lit(json!("DEFAULT")),
        );
        assert_eq!(ev(&keyed, &ctx), stale, "{filter}");
        // Missing alone is still Missing (and so takes a lookup's default).
        let only_missing = collection_over(filter, 0, 7);
        assert_eq!(ev(&only_missing, &ctx), EvalValue::Missing, "{filter}");
        let keyed = lookup(only_missing, vec![], lit(json!("DEFAULT")));
        assert_eq!(ev(&keyed, &ctx), known(json!("DEFAULT")), "{filter}");
        // Known items are unchanged.
        let all_known = collection_over(filter, 7, 8);
        let expected = if filter {
            json!([0, 1])
        } else {
            json!([true, true])
        };
        assert_eq!(ev(&all_known, &ctx), known(expected), "{filter}");
    }
}

#[test]
fn an_empty_collection_item_satisfies_a_boolean_position() {
    let none = || lit(json!([]));
    let item = || r("h", &[]);
    let filtered = |predicate: Expr| Expr::Filter {
        collection: Box::new(none()),
        item: "h".into(),
        predicate: Box::new(predicate),
        max_items: 4,
    };
    let strings = Schema::Array {
        items: Box::new(Schema::string(16)),
        max_items: 4,
    };
    for predicate in [
        item(),
        Expr::Not {
            value: Box::new(item()),
        },
        Expr::And {
            values: vec![item(), lit(json!(true))],
        },
        Expr::Or {
            values: vec![item(), lit(json!(false))],
        },
    ] {
        let issues = returning(filtered(predicate.clone()), strings.clone());
        assert!(issues.is_empty(), "{predicate:?}: {issues:?}");
    }
    // A non-boolean item is still refused.
    let numbers = Expr::Filter {
        collection: Box::new(lit(json!([1]))),
        item: "h".into(),
        predicate: Box::new(item()),
        max_items: 4,
    };
    assert!(has_issue(
        &returning(numbers, strings),
        "expression_type",
        "steps.finish",
        "filter predicate requires boolean"
    ));
}

/// Canonical bytes of pilot artifacts written by `aq-playbooks examples` at
/// 92fcc42, before `lookup` and `let` existed. Adding expression variants must
/// not change the bytes, digest or executable fingerprint of an artifact that
/// does not use them.
#[test]
fn existing_artifacts_keep_their_bytes_digest_and_fingerprint() {
    for (bytes, digest, fingerprint) in [
        (
            &include_bytes!("fixtures/coding.artifact.json")[..],
            "sha256:4a91ea931ca19ff66c5fa4d37a24350613a1d484dd65a1a5d299025a53f8bbdf",
            "sha256:c16cb61c26284a2b63c12b0b92882adcb3502272e277d260319b3401d6ac1765",
        ),
        (
            &include_bytes!("fixtures/repair.artifact.json")[..],
            "sha256:809a461b365c4f0f0a5b41d8e00891f5db49dda71cede56b12bfbc0c43e31b92",
            "sha256:4c5465ba9f51e79eed9cb47c22748e0e83c881930f027b7b781d5e479e6ba16e",
        ),
    ] {
        // Round trip reproduces the exact bytes and re-verifies both hashes.
        let artifact = Artifact::from_canonical_bytes(bytes).unwrap();
        assert_eq!(artifact.digest, digest);
        assert_eq!(artifact.executable_fingerprint, fingerprint);
        let resealed = Artifact::seal(artifact.content.clone()).unwrap();
        assert_eq!(resealed.digest, digest);
        assert_eq!(resealed.executable_fingerprint, fingerprint);
        assert_eq!(canonical_bytes(&resealed).unwrap(), bytes);
    }
}
