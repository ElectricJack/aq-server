//! A generated soundness test of the draft read check.
//!
//! Base reads that must be refused (a value some way of reaching the step
//! lacks) and base reads that must be accepted are wrapped in every
//! composition, up to three deep, of combinators that cover every `Expr`
//! variant and every construct that puts a value in scope. An oracle that
//! knows only what each combinator does to a Missing value (carries it,
//! needs it, or tests it with `exists`) says whether the draft must be
//! refused. Every wrapped unsound read must be refused with the reasons of
//! the direct read; no wrapped sound read may be refused.
use aq_playbooks_model::*;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn lit(v: Value) -> Expr {
    Expr::literal(v)
}
fn r(root: &str, path: &[&str]) -> Expr {
    Expr::Ref {
        root: root.into(),
        path: path.iter().map(|s| s.to_string()).collect(),
    }
}
fn b(e: Expr) -> Box<Expr> {
    Box::new(e)
}
fn edges(xs: &[(&str, &str)]) -> BTreeMap<String, String> {
    xs.iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}
fn strings(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}
fn let_(name: &str, value: Expr, body: Expr) -> Expr {
    Expr::Let {
        bindings: vec![LetBinding {
            name: name.into(),
            value,
        }],
        body: b(body),
    }
}
fn eq(left: Expr, right: Expr) -> Expr {
    Expr::Eq {
        left: b(left),
        right: b(right),
    }
}
fn array(items: Vec<Expr>) -> Expr {
    Expr::Array { items }
}
fn object(key: &str, value: Expr) -> Expr {
    Expr::Object {
        fields: BTreeMap::from([(key.to_string(), value)]),
    }
}
fn lookup(key: Expr, cases: Vec<(Value, Expr)>, default: Expr) -> Expr {
    Expr::Lookup {
        key: b(key),
        cases: cases
            .into_iter()
            .map(|(when, then)| LookupCase { when, then })
            .collect(),
        default: b(default),
    }
}

fn fields(names: &[&str]) -> Schema {
    Schema::object(
        names
            .iter()
            .map(|name| (name.to_string(), Schema::string(64)))
            .collect(),
    )
}
fn command(name: &str, outcomes: &[(&str, Schema)]) -> CommandContract {
    CommandContract {
        name: name.into(),
        version: "1.0.0".into(),
        input: Schema::object(BTreeMap::new()),
        outcomes: outcomes
            .iter()
            .map(|(k, v)| ((*k).to_string(), v.clone()))
            .collect(),
        capabilities: strings(&[name]),
        scopes: strings(&["project"]),
        effect: Effect::Observation,
        recovery: Recovery::Observation,
        asynchronous: None,
        cancellation: false,
        public_result_fields: BTreeSet::new(),
        sensitive_input_fields: BTreeSet::new(),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 4096,
        deadline_ms: 1000,
    }
}
fn event(name: &str, payload: Schema, correlation: &[&str]) -> EventContract {
    EventContract {
        sensitive_payload_fields: BTreeSet::new(),
        name: name.into(),
        version: "1.0.0".into(),
        payload,
        trusted_producers: strings(&["host"]),
        scopes: strings(&["project"]),
        correlation_fields: strings(correlation),
        subject_field: None,
        revision_field: None,
        max_payload_bytes: 4096,
        retention_ms: 86_400_000,
    }
}
/// `ask.judge`, asynchronous and joined by its handle, whose outcomes differ
/// in shape as the host's judgment does (`assessed` alone has `choice`);
/// `work.look`, synchronous, whose outcomes differ too (`found` alone has
/// `kind`, and its `hint` is optional); and an event to wait for.
fn registry() -> Registry {
    let assessed = fields(&["choice", "reference"]);
    let declined = fields(&["reason", "reference"]);
    let completion = Schema::object(BTreeMap::from([
        ("operation".to_string(), Schema::string(64)),
        (
            "outcome".to_string(),
            Schema::String {
                max_length: 16,
                values: strings(&["assessed", "failed", "refused"]),
            },
        ),
        (
            "output".to_string(),
            Schema::Union {
                variants: vec![assessed.clone(), declined.clone()],
            },
        ),
    ]));
    let mut judge = command(
        "ask.judge",
        &[
            ("accepted", fields(&["handle"])),
            ("assessed", assessed),
            ("failed", declined.clone()),
            ("refused", declined),
        ],
    );
    judge.asynchronous = Some(AsyncContract {
        completion_event: "ask.judged".into(),
        completion: completion.clone(),
        correlation_field: "operation".into(),
        reconciliation_command: "ask.get".into(),
    });
    let found = Schema::Object {
        properties: BTreeMap::from([
            ("id".into(), Schema::string(64)),
            ("kind".into(), Schema::string(32)),
            ("hint".into(), Schema::string(32)),
        ]),
        required: strings(&["id", "kind"]),
    };
    let look = command("work.look", &[("found", found), ("none", fields(&["id"]))]);
    let get = command("ask.get", &[("known", fields(&["status"]))]);
    // A command whose input a generated read can be written into.
    let mut flag = command("work.flag", &[("done", fields(&[]))]);
    flag.input = Schema::object(BTreeMap::from([("flag".into(), Schema::Boolean)]));
    Registry {
        commands: [judge, look, get, flag]
            .into_iter()
            .map(|c| (c.name.clone(), c))
            .collect(),
        events: [
            event("ask.judged", completion, &["operation"]),
            event("aq.work.seen", fields(&["note"]), &[]),
        ]
        .into_iter()
        .map(|e| (e.name.clone(), e))
        .collect(),
    }
}
fn decide(predicate: Expr, next: &str) -> Step {
    Step {
        op: Operation::Decide { predicate },
        edges: edges(&[("true", next), ("false", next), ("unknown", next)]),
        on_error: None,
    }
}

/// The step every generated chain starts at.
const SLOT: &str = "slot";

/// A read: the expression that carries the value, and the path read in it.
struct Base {
    name: &'static str,
    carrier: Expr,
    path: &'static [&'static str],
    /// The read is there on every way to `SLOT`.
    sound: bool,
    /// The carrier itself is Missing on some way (rule R-B, or a read given
    /// whole that is Missing); otherwise the carrier is there and the path
    /// leads through a field some way lacks.
    carrier_missing: bool,
    /// The value is needed inside the carrier itself (as a lookup key), so
    /// nothing around it, not even `exists`, makes the draft sound.
    needed_inside: bool,
    /// Ordinary typing gives a `for_each` item the type its collection has
    /// anywhere in the playbook, not at the loop, and for this carrier that
    /// type cannot be read: typing itself refuses the read through an item.
    loose_item: bool,
    /// The carrier may be Missing and, where it is there, may lack the
    /// field: a wrapped read is refused for some of the direct read's
    /// reasons, not always all (`exists` can absorb one kind).
    two_causes: bool,
    /// How deep the combinators are composed around it.
    depth: usize,
    /// Where the read can be consumed.
    sites: &'static [Site],
    content: ArtifactContent,
}
/// Where the value of the generated expression is needed.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Site {
    /// A `decide` predicate.
    Decide,
    /// A command's input.
    Input,
    /// A `return` output.
    Output,
    /// A trigger's `input`, where only `event` is in scope.
    Trigger,
}
const IN_STEPS: &[Site] = &[Site::Decide, Site::Input, Site::Output];
const ANYWHERE: &[Site] = &[Site::Decide, Site::Input, Site::Output, Site::Trigger];

fn playbook(registry: &Registry, start: &str, steps: Vec<(&str, Step)>) -> ArtifactContent {
    let mut steps: BTreeMap<String, Step> =
        steps.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    steps.insert(
        "end".into(),
        Step {
            op: Operation::Return {
                outcome: "done".into(),
                output: lit(Value::Null),
            },
            edges: BTreeMap::new(),
            on_error: None,
        },
    );
    ArtifactContent {
        format_version: FORMAT_VERSION,
        compiler: "test".into(),
        source: SourceBinding {
            digest: hash_bytes(b"source"),
            header: SourceHeader {
                format_version: FORMAT_VERSION,
                id: "generated".into(),
                scope: "project".into(),
                interfaces: strings(&["main"]),
                definitions: strings(&["levels"]),
            },
        },
        playbook: Playbook {
            id: "generated".into(),
            purpose: "a generated read".into(),
            scope: "project".into(),
            entries: BTreeMap::from([(
                "main".into(),
                EntryPoint {
                    input: Schema::Object {
                        properties: BTreeMap::from([
                            ("task".into(), Schema::string(64)),
                            ("flag".into(), Schema::Boolean),
                        ]),
                        required: strings(&["task"]),
                    },
                    outcomes: BTreeMap::from([
                        ("done".into(), Schema::Null),
                        ("told".into(), Schema::Boolean),
                    ]),
                    start: start.into(),
                },
            )]),
            steps,
            triggers: vec![],
            limits: Limits::default(),
            required_capabilities: strings(&["ask.judge", "work.look", "work.flag"]),
        },
        registry: registry.clone(),
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::from([(
            "levels".to_string(),
            CapturedDefinition {
                version: "1".into(),
                digest: digest(&json!(["low", "high"])).unwrap(),
                value: json!(["low", "high"]),
            },
        )]),
        source_map: BTreeMap::new(),
    }
}
fn bases() -> Vec<Base> {
    let registry = registry();
    let command_ref = |name: &str| ContractRef {
        name: name.into(),
        fingerprint: registry.commands[name].fingerprint().unwrap(),
    };
    // `ask` binds `j`; each of its exits goes where `to` says.
    let ask = |to: &[(&str, &str)], on_error: &str| Step {
        op: Operation::Invoke {
            command: command_ref("ask.judge"),
            input: Expr::Object {
                fields: BTreeMap::new(),
            },
            bind: "j".into(),
            retry: None,
        },
        edges: edges(to),
        on_error: Some(on_error.into()),
    };
    let look = |to: &[(&str, &str)]| Step {
        op: Operation::Invoke {
            command: command_ref("work.look"),
            input: Expr::Object {
                fields: BTreeMap::new(),
            },
            bind: "seen".into(),
            retry: None,
        },
        edges: edges(to),
        on_error: Some("end".into()),
    };
    let ask_as = |bind: &str, to: &[(&str, &str)]| Step {
        op: Operation::Invoke {
            command: command_ref("ask.judge"),
            input: Expr::Object {
                fields: BTreeMap::new(),
            },
            bind: bind.into(),
            retry: None,
        },
        edges: edges(to),
        on_error: Some("end".into()),
    };
    let join_of = |members: &[&str], mode: JoinMode, accepted: &[&str], to: &[(&str, &str)]| Step {
        op: Operation::Join {
            members: members.iter().map(|m| r(m, &[])).collect(),
            member_keys: ["c", "d"][..members.len()]
                .iter()
                .map(|k| k.to_string())
                .collect(),
            mode,
            accepted_outcomes: strings(accepted),
            deadline_ms: 60_000,
            remaining: RemainingPolicy::Track,
            bind: "answer".into(),
        },
        edges: edges(to),
        on_error: Some("end".into()),
    };
    let join = |accepted: &[&str], to: &[(&str, &str)]| Step {
        op: Operation::Join {
            members: vec![r("j", &[])],
            member_keys: vec!["c".into()],
            mode: JoinMode::All,
            accepted_outcomes: strings(accepted),
            deadline_ms: 60_000,
            remaining: RemainingPolicy::Track,
            bind: "answer".into(),
        },
        edges: edges(to),
        on_error: Some("end".into()),
    };
    let to_join = [
        ("accepted", "read"),
        ("assessed", "end"),
        ("failed", "end"),
        ("refused", "end"),
    ];
    let all_end = [
        ("accepted", "end"),
        ("assessed", "end"),
        ("failed", "end"),
        ("refused", "end"),
    ];
    let base = |name, carrier: &str, path, sound, carrier_missing, start: &str, steps| Base {
        name,
        carrier: r(carrier, &[]),
        path,
        sound,
        carrier_missing,
        needed_inside: false,
        two_causes: false,
        loose_item: carrier == "answer" || carrier == "vars",
        depth: 3,
        sites: IN_STEPS,
        content: playbook(&registry, start, steps),
    };
    // A base composed two deep: the scope sources and sites beyond the first
    // bases, which are composed three deep.
    let shallow = |mut base: Base| {
        base.depth = 2;
        base
    };
    // A literal bound to a name: the value an author writes as a table.
    let literal = |name, value: Value, path, sound| Base {
        carrier: let_("lit", lit(value), r("lit", &[])),
        sites: ANYWHERE,
        ..base(name, "lit", path, sound, false, SLOT, vec![])
    };
    // A read given whole, over literal rows of which the second lacks `k`.
    let rows = |name, read: Expr, needed_inside| Base {
        carrier: let_("rows", lit(json!([{"k": "chore"}, {}])), read),
        needed_inside,
        depth: 2,
        sites: ANYWHERE,
        ..base(name, "rows", &[], false, true, SLOT, vec![])
    };
    let two_asked = [
        ("accepted", "ask2"),
        ("assessed", "end"),
        ("failed", "end"),
        ("refused", "end"),
    ];
    // `look` runs before the loop and again in its body; its `none` outcome
    // leaves the loop by `noted`, whose failure inside the loop is the
    // loop's to handle.
    let looped = |name, on_failure: IterationFailure, sound: bool| {
        let mut noted = look(&[("found", "end"), ("none", "end")]);
        let Operation::Invoke { bind, .. } = &mut noted.op else {
            unreachable!()
        };
        *bind = "seen2".into();
        // The check follows a failure of any step in the body, `end` too,
        // which `look`'s own `on_error` reaches without `seen`: where the
        // loop goes on after a failure, `seen` itself may not be bound.
        let two_causes = !sound;
        let mut base = shallow(base(
            name,
            "seen",
            &["kind"],
            sound,
            !sound,
            "look",
            vec![
                ("look", look(&[("found", "each"), ("none", "noted")])),
                (
                    "each",
                    Step {
                        op: Operation::ForEach {
                            collection: lit(json!(["a"])),
                            item: "it0".into(),
                            body: "again".into(),
                            next: SLOT.into(),
                            max_items: 1,
                            concurrency: 1,
                            on_failure,
                        },
                        edges: BTreeMap::new(),
                        on_error: Some("end".into()),
                    },
                ),
                ("again", decide(lit(json!(true)), "look")),
                ("noted", noted),
            ],
        ));
        base.two_causes = two_causes;
        base
    };
    let always_asked = || {
        vec![(
            "ask",
            ask(
                &[
                    ("accepted", SLOT),
                    ("assessed", "end"),
                    ("failed", "end"),
                    ("refused", "end"),
                ],
                "end",
            ),
        )]
    };
    let joined = |name, members: &[&str], mode: JoinMode, accepted: &[&str], edge: &str, sound| {
        let mut to = vec![
            ("completed", "end"),
            ("no_match", "end"),
            ("timeout", "end"),
        ];
        for exit in &mut to {
            if exit.0 == edge {
                exit.1 = SLOT;
            }
        }
        let mut steps = vec![("read", join_of(members, mode, accepted, &to))];
        if members.len() == 2 {
            steps.push(("ask", ask(&two_asked, "end")));
            steps.push(("ask2", ask_as("j2", &to_join)));
        } else {
            steps.push(("ask", ask(&to_join, "end")));
        }
        shallow(base(
            name,
            "answer",
            &["c", "participant"],
            sound,
            false,
            "ask",
            steps,
        ))
    };
    let mut all = vec![
        // Literals: a table's rows keep their own fields, an array its length.
        literal(
            "a field of a literal row that lacks it",
            json!([{"k": "chore"}, {}]),
            &["1", "k"],
            false,
        ),
        literal(
            "a position past the end of a literal array",
            json!(["chore", "x"]),
            &["5"],
            false,
        ),
        literal(
            "a field of a literal row that has it",
            json!([{"k": "chore"}, {}]),
            &["0", "k"],
            true,
        ),
        // The reviewer's probes L1 and L1b.
        rows(
            "a lookup keyed on a field of every literal row, one of which lacks it",
            Expr::Map {
                collection: b(r("rows", &[])),
                item: "row".into(),
                value: b(lookup(r("row", &["k"]), vec![], lit(json!(true)))),
                max_items: 2,
            },
            true,
        ),
        rows(
            "a filter on a field of every literal row, one of which lacks it",
            Expr::Filter {
                collection: b(r("rows", &[])),
                item: "row".into(),
                predicate: b(eq(r("row", &["k"]), lit(json!("chore")))),
                max_items: 2,
            },
            false,
        ),
        // The other roots.
        shallow(base(
            "a binding not bound on some way, read through `vars`",
            "vars",
            &["j", "handle"],
            false,
            false,
            "ask",
            vec![("ask", ask(&all_end, SLOT))],
        )),
        shallow(base(
            "a field every way has, read through `vars`",
            "vars",
            &["j", "handle"],
            true,
            false,
            "ask",
            always_asked(),
        )),
        shallow(base(
            "a captured definition",
            "definitions",
            &["levels", "0"],
            true,
            false,
            SLOT,
            vec![],
        )),
        Base {
            sites: &[Site::Trigger],
            ..shallow(base(
                "a field of the trigger's event",
                "event",
                &["note"],
                true,
                false,
                SLOT,
                vec![],
            ))
        },
        // Joins with several members, and `no_match`.
        joined(
            "a member another member can qualify before, in `any_accepted`",
            &["j", "j2"],
            JoinMode::AnyAccepted,
            &["assessed"],
            "completed",
            false,
        ),
        joined(
            "a member another member can complete before, in `any_completion`",
            &["j", "j2"],
            JoinMode::AnyCompletion,
            &[],
            "completed",
            false,
        ),
        joined(
            "a member on `no_match` of an `all` join",
            &["j"],
            JoinMode::All,
            &["assessed"],
            "no_match",
            true,
        ),
        joined(
            "a member on `no_match` of an `any_accepted` join",
            &["j"],
            JoinMode::AnyAccepted,
            &["assessed"],
            "no_match",
            false,
        ),
        // A failure in a loop body.
        looped(
            "an outcome that reaches the end of a loop only by a failure in its body",
            IterationFailure::Continue,
            false,
        ),
        looped(
            "an outcome a failure in a loop body takes out of the loop",
            IterationFailure::Stop,
            true,
        ),
    ];
    all.extend([
        base(
            "an outcome-only field where two outcome edges meet",
            "j",
            &["choice"],
            false,
            false,
            "ask",
            vec![(
                "ask",
                ask(
                    &[
                        ("accepted", "end"),
                        ("assessed", SLOT),
                        ("failed", SLOT),
                        ("refused", "end"),
                    ],
                    "end",
                ),
            )],
        ),
        base(
            "a join member's field on the timeout edge",
            "answer",
            &["c", "participant"],
            false,
            false,
            "ask",
            vec![
                ("ask", ask(&to_join, "end")),
                (
                    "read",
                    join(
                        &["assessed"],
                        &[("completed", "end"), ("no_match", "end"), ("timeout", SLOT)],
                    ),
                ),
            ],
        ),
        base(
            "a field only one accepted outcome of a join has",
            "answer",
            &["c", "output", "choice"],
            false,
            false,
            "ask",
            vec![
                ("ask", ask(&to_join, "end")),
                (
                    "read",
                    join(
                        &[],
                        &[("completed", SLOT), ("no_match", "end"), ("timeout", "end")],
                    ),
                ),
            ],
        ),
        base(
            "a wait's binding on its timeout edge",
            "w",
            &["note"],
            false,
            true,
            "hold",
            vec![(
                "hold",
                Step {
                    op: Operation::Wait {
                        event: Some(ContractRef {
                            name: "aq.work.seen".into(),
                            fingerprint: registry.events["aq.work.seen"].fingerprint().unwrap(),
                        }),
                        correlation: BTreeMap::new(),
                        minimum_revision: None,
                        deadline_ms: 1000,
                        bind: "w".into(),
                    },
                    edges: edges(&[("received", "end"), ("timeout", SLOT)]),
                    on_error: Some("end".into()),
                },
            )],
        ),
        base(
            "a binding not bound on its step's on_error exit",
            "j",
            &["handle"],
            false,
            true,
            "ask",
            vec![("ask", ask(&all_end, SLOT))],
        ),
        base(
            "a binding not bound on a way around its step",
            "j",
            &["handle"],
            false,
            true,
            "gate",
            vec![
                (
                    "gate",
                    Step {
                        op: Operation::Decide {
                            predicate: lit(json!(true)),
                        },
                        edges: edges(&[("true", "ask"), ("false", SLOT), ("unknown", "end")]),
                        on_error: None,
                    },
                ),
                (
                    "ask",
                    ask(
                        &[
                            ("accepted", SLOT),
                            ("assessed", "end"),
                            ("failed", "end"),
                            ("refused", "end"),
                        ],
                        "end",
                    ),
                ),
            ],
        ),
        base(
            "a for_each body's binding read after the loop",
            "seen",
            &["id"],
            false,
            true,
            "each",
            vec![
                (
                    "each",
                    Step {
                        op: Operation::ForEach {
                            collection: lit(json!(["a"])),
                            item: "it0".into(),
                            body: "look".into(),
                            next: SLOT.into(),
                            max_items: 1,
                            concurrency: 1,
                            on_failure: IterationFailure::Stop,
                        },
                        edges: BTreeMap::new(),
                        on_error: Some("end".into()),
                    },
                ),
                ("look", look(&[("found", "each"), ("none", "each")])),
            ],
        ),
        base(
            "a field every way has",
            "j",
            &["handle"],
            true,
            false,
            "ask",
            vec![(
                "ask",
                ask(
                    &[
                        ("accepted", SLOT),
                        ("assessed", "end"),
                        ("failed", "end"),
                        ("refused", "end"),
                    ],
                    "end",
                ),
            )],
        ),
        base(
            "a field optional in its one schema",
            "seen",
            &["hint"],
            true,
            false,
            "look",
            vec![("look", look(&[("found", SLOT), ("none", "end")]))],
        ),
    ]);
    all
}

/// A combinator: wraps the expression built so far.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Wrap {
    // Give back the same value: a value read through them is the same read.
    LetAlias,
    AliasOfAlias,
    ObjectField,
    ArrayPosition,
    LookupBoth,
    MapResultIndex,
    FilterResultIndex,
    DifferenceResult,
    /// Read in a later step than the one control arrives at.
    LaterStep,
    /// The value is the item of a `for_each`, read in its body.
    ForEachItem,
    // Give back the value or null: only around the finished read.
    LookupCase,
    LookupDefault,
    // Compute something else from the value.
    Object,
    Array,
    MapBody,
    FilterPredicate,
    Contains,
    Eq,
    Lt,
    Not,
    And,
    Or,
    /// The value is a lookup key.
    LookupKey,
    Exists,
}
const WRAPS: [Wrap; 24] = [
    Wrap::LetAlias,
    Wrap::AliasOfAlias,
    Wrap::ObjectField,
    Wrap::ArrayPosition,
    Wrap::LookupBoth,
    Wrap::MapResultIndex,
    Wrap::FilterResultIndex,
    Wrap::DifferenceResult,
    Wrap::LaterStep,
    Wrap::ForEachItem,
    Wrap::LookupCase,
    Wrap::LookupDefault,
    Wrap::Object,
    Wrap::Array,
    Wrap::MapBody,
    Wrap::FilterPredicate,
    Wrap::Contains,
    Wrap::Eq,
    Wrap::Lt,
    Wrap::Not,
    Wrap::And,
    Wrap::Or,
    Wrap::LookupKey,
    Wrap::Exists,
];
/// What a combinator does with a Missing value.
#[derive(PartialEq)]
enum Missing {
    /// Its result is Missing too.
    Carries,
    /// It needs the value: the run stops, or a default is taken silently.
    Needs,
    /// `exists`: its result is `false`.
    Tests,
}
impl Wrap {
    fn missing(self) -> Missing {
        match self {
            Wrap::LetAlias
            | Wrap::AliasOfAlias
            | Wrap::ObjectField
            | Wrap::ArrayPosition
            | Wrap::LookupBoth
            | Wrap::MapResultIndex
            | Wrap::FilterResultIndex
            | Wrap::DifferenceResult
            | Wrap::LaterStep
            | Wrap::LookupCase
            | Wrap::LookupDefault
            | Wrap::Object
            | Wrap::Array
            | Wrap::MapBody
            | Wrap::FilterPredicate
            | Wrap::Contains
            | Wrap::Eq
            | Wrap::Lt
            | Wrap::Not
            | Wrap::And
            | Wrap::Or => Missing::Carries,
            Wrap::ForEachItem | Wrap::LookupKey => Missing::Needs,
            Wrap::Exists => Missing::Tests,
        }
    }
    /// Gives back the very value it wraps, so a path can be read through it.
    fn same_value(self) -> bool {
        matches!(
            self,
            Wrap::LetAlias
                | Wrap::AliasOfAlias
                | Wrap::ObjectField
                | Wrap::ArrayPosition
                | Wrap::LookupBoth
                | Wrap::MapResultIndex
                | Wrap::FilterResultIndex
                | Wrap::DifferenceResult
                | Wrap::LaterStep
                | Wrap::ForEachItem
        )
    }
}
/// A step in front of the one that reads.
enum Layer {
    Later,
    ForEach { collection: Expr, item: String },
}
/// The expression built so far and the steps in front of it.
struct Built {
    expr: Expr,
    /// The expression is a string (`lt` compares two of them).
    string: bool,
    layers: Vec<Layer>,
    /// A string every way has, for the key of a table: `input.task` in a
    /// step, `event.note` in a trigger.
    key: Expr,
}
impl Built {
    fn new(expr: Expr, string: bool, site: Site) -> Self {
        let key = match site {
            Site::Decide | Site::Input | Site::Output => r("input", &["task"]),
            Site::Trigger => r("event", &["note"]),
        };
        Self {
            expr,
            string,
            layers: vec![],
            key,
        }
    }
}
impl Built {
    fn wrap(&mut self, wrap: Wrap, n: usize) -> bool {
        let e = self.expr.clone();
        let name = |prefix: &str| format!("{prefix}{n}");
        let item = name("x");
        let both = |e: &Expr| eq(e.clone(), e.clone());
        let mut string = false;
        self.expr = match wrap {
            Wrap::LetAlias => {
                string = self.string;
                let_(&name("a"), e, r(&name("a"), &[]))
            }
            Wrap::AliasOfAlias => {
                string = self.string;
                let_(
                    &name("a"),
                    e,
                    let_(&name("b"), r(&name("a"), &[]), r(&name("b"), &[])),
                )
            }
            Wrap::ObjectField => {
                string = self.string;
                let_(&name("o"), object("k", e), r(&name("o"), &["k"]))
            }
            Wrap::ArrayPosition => {
                string = self.string;
                let_(&name("r"), array(vec![e]), r(&name("r"), &["0"]))
            }
            Wrap::LookupBoth => {
                string = self.string;
                lookup(self.key.clone(), vec![(json!("a"), e.clone())], e)
            }
            Wrap::MapResultIndex => {
                string = self.string;
                let mapped = Expr::Map {
                    collection: b(array(vec![e])),
                    item: item.clone(),
                    value: b(r(&item, &[])),
                    max_items: 1,
                };
                let_(&name("m"), mapped, r(&name("m"), &["0"]))
            }
            Wrap::FilterResultIndex => {
                string = self.string;
                let filtered = Expr::Filter {
                    collection: b(array(vec![e])),
                    item: item.clone(),
                    predicate: b(lit(json!(true))),
                    max_items: 1,
                };
                let_(&name("f"), filtered, r(&name("f"), &["0"]))
            }
            Wrap::DifferenceResult => {
                string = self.string;
                let left = array(vec![Expr::Object {
                    fields: BTreeMap::from([("id".to_string(), lit(json!("a"))), ("v".into(), e)]),
                }]);
                let kept = Expr::Difference {
                    left: b(left),
                    right: b(lit(json!([]))),
                    identity: vec!["id".into()],
                    max_items: 1,
                };
                let_(&name("d"), kept, r(&name("d"), &["0", "v"]))
            }
            Wrap::LaterStep => {
                string = self.string;
                self.layers.push(Layer::Later);
                e
            }
            Wrap::ForEachItem => {
                string = self.string;
                self.layers.push(Layer::ForEach {
                    collection: array(vec![e]),
                    item: name("t"),
                });
                r(&name("t"), &[])
            }
            Wrap::LookupCase => lookup(self.key.clone(), vec![(json!("a"), e)], lit(Value::Null)),
            Wrap::LookupDefault => {
                lookup(self.key.clone(), vec![(json!("a"), lit(Value::Null))], e)
            }
            Wrap::Object => object("k", e),
            Wrap::Array => array(vec![e]),
            Wrap::MapBody => Expr::Map {
                collection: b(lit(json!([1]))),
                item,
                value: b(e),
                max_items: 1,
            },
            Wrap::FilterPredicate => Expr::Filter {
                collection: b(lit(json!([1]))),
                item,
                predicate: b(both(&e)),
                max_items: 1,
            },
            Wrap::Contains => Expr::Contains {
                collection: b(array(vec![e.clone()])),
                value: b(e),
            },
            Wrap::Eq => both(&e),
            Wrap::Lt => {
                if !self.string {
                    return false;
                }
                Expr::Lt {
                    left: b(e.clone()),
                    right: b(e),
                }
            }
            Wrap::Not => Expr::Not { value: b(both(&e)) },
            Wrap::And => Expr::And {
                values: vec![both(&e), lit(json!(true))],
            },
            Wrap::Or => Expr::Or {
                values: vec![lit(json!(false)), both(&e)],
            },
            Wrap::LookupKey => lookup(e, vec![], lit(json!(true))),
            Wrap::Exists => Expr::Exists { value: b(e) },
        };
        self.string = string;
        true
    }
    /// Reads `path` in the expression built so far.
    fn project(&mut self, path: &[&str]) {
        // The expression is a reference (to the carrier, a name or an item),
        // or a lookup of two references: read the path in each.
        fn read(e: &mut Expr, path: &[&str]) {
            match e {
                Expr::Ref { path: p, .. } => p.extend(path.iter().map(|s| s.to_string())),
                Expr::Let { body, .. } => read(body, path),
                Expr::Lookup { cases, default, .. } => {
                    for c in cases {
                        read(&mut c.then, path);
                    }
                    read(default, path);
                }
                other => panic!("not a value to read through: {other:?}"),
            }
        }
        if !path.is_empty() {
            read(&mut self.expr, path);
            self.string = true;
        }
    }
    /// Puts the generated read into `content`: the layers as steps from
    /// `SLOT` on, then the expression where `site` needs its value.
    fn install(self, site: Site, content: &mut ArtifactContent) {
        let steps = &mut content.playbook.steps;
        steps.retain(|name, _| {
            name != SLOT && !(name.starts_with('g') && name[1..].parse::<usize>().is_ok())
        });
        content.playbook.triggers.clear();
        let names: Vec<String> = (0..=self.layers.len())
            .map(|i| if i == 0 { SLOT.into() } else { format!("g{i}") })
            .collect();
        // Where a step leads when it is done: the innermost loop around it.
        let mut after = "end".to_string();
        for (i, layer) in self.layers.into_iter().enumerate() {
            let next = &names[i + 1];
            match layer {
                Layer::Later => {
                    steps.insert(names[i].clone(), decide(lit(json!(true)), next));
                }
                Layer::ForEach { collection, item } => {
                    steps.insert(
                        names[i].clone(),
                        Step {
                            op: Operation::ForEach {
                                collection,
                                item,
                                body: next.clone(),
                                next: after.clone(),
                                max_items: 1,
                                concurrency: 1,
                                on_failure: IterationFailure::Stop,
                            },
                            edges: BTreeMap::new(),
                            on_error: Some("end".into()),
                        },
                    );
                    after = names[i].clone();
                }
            }
        }
        // A test of any value that is Missing where the value is.
        let reads = let_("v", self.expr, eq(r("v", &[]), r("v", &[])));
        let last = names.last().unwrap().clone();
        let flag = |reads: Expr| Expr::Object {
            fields: BTreeMap::from([("flag".to_string(), reads)]),
        };
        let reader = match site {
            Site::Decide => decide(reads, &after),
            Site::Input => {
                let work = &content.registry.commands["work.flag"];
                Step {
                    op: Operation::Invoke {
                        command: ContractRef {
                            name: work.name.clone(),
                            fingerprint: work.fingerprint().unwrap(),
                        },
                        input: flag(reads),
                        bind: "flagged".into(),
                        retry: None,
                    },
                    edges: edges(&[("done", &after)]),
                    on_error: Some("end".into()),
                }
            }
            // A step that returns it, beside the way on to `end`.
            Site::Output => {
                steps.insert(
                    "g99".into(),
                    Step {
                        op: Operation::Return {
                            outcome: "told".into(),
                            output: reads,
                        },
                        edges: BTreeMap::new(),
                        on_error: None,
                    },
                );
                Step {
                    op: Operation::Decide {
                        predicate: lit(json!(true)),
                    },
                    edges: edges(&[("true", "g99"), ("false", &after), ("unknown", &after)]),
                    on_error: None,
                }
            }
            Site::Trigger => {
                let seen = &content.registry.events["aq.work.seen"];
                let mut input = flag(reads);
                let Expr::Object { fields } = &mut input else {
                    unreachable!()
                };
                fields.insert("task".into(), r("event", &["note"]));
                content.playbook.triggers.push(Trigger {
                    id: "t".into(),
                    event: ContractRef {
                        name: seen.name.clone(),
                        fingerprint: seen.fingerprint().unwrap(),
                    },
                    entry: "main".into(),
                    input,
                });
                decide(lit(json!(true)), "end")
            }
        };
        steps.insert(last, reader);
    }
}

/// Whether the draft must be refused: the oracle. It follows the one fact
/// that matters, whether the value is Missing, through the combinators.
fn must_refuse(base: &Base, wraps: &[Wrap], split: usize) -> bool {
    if base.sound {
        return false;
    }
    if base.needed_inside {
        return true;
    }
    let mut missing = base.carrier_missing;
    for (i, wrap) in wraps.iter().enumerate() {
        if i == split {
            missing = true;
        }
        match wrap.missing() {
            Missing::Carries => {}
            Missing::Needs if missing => return true,
            Missing::Needs => {}
            Missing::Tests => return false,
        }
    }
    // Read at last where its value is needed.
    true
}
/// The reasons of every refused read: "…this step: <reasons>. Read it only…".
fn reasons(issues: &[ValidationIssue]) -> BTreeSet<String> {
    issues
        .iter()
        .filter(|i| i.code == "expression_type")
        .filter_map(|i| i.message.split_once("this step: "))
        .filter_map(|(_, rest)| rest.split_once(". Read it only"))
        .flat_map(|(reasons, _)| reasons.split("; ").map(str::to_string))
        .collect()
}

const ACCEPTED: &str = ": an unsound read is accepted";
const OTHER_REASONS: &str = ": refused for ";
const SOUND_REFUSED: &str = ": a sound read is refused: ";
const TYPING_REFUSES: &str = ": ordinary typing refuses: ";

#[derive(Default)]
struct Tally {
    cases: usize,
    must_refuse: usize,
    must_accept: usize,
    typed: usize,
    typing_stricter: usize,
    failures: Vec<String>,
}
impl Tally {
    fn add(&mut self, other: Tally) {
        self.cases += other.cases;
        self.must_refuse += other.must_refuse;
        self.must_accept += other.must_accept;
        self.typed += other.typed;
        self.typing_stricter += other.typing_stricter;
        self.failures.extend(other.failures);
    }
}

/// Every composition of up to `depth` combinators around `base` that starts
/// with `first` (the direct read alone for `None`), with the path read after
/// each possible prefix of value-preserving ones, consumed at `site`.
fn check_base(base: &Base, depth: usize, first: Option<Wrap>, site: Site) -> Tally {
    let mut tally = Tally::default();
    let mut content = base.content.clone();
    let direct = {
        let mut built = Built::new(base.carrier.clone(), true, site);
        built.project(base.path);
        built.install(site, &mut content);
        let issues = content.draft_issues(&[]);
        assert_eq!(
            content.validate_structure(),
            vec![],
            "{}: ordinary typing accepts the direct read",
            base.name
        );
        assert_eq!(
            issues.is_empty(),
            base.sound,
            "{}: the direct read: {issues:?}",
            base.name
        );
        reasons(&issues)
    };
    let mut sequences: Vec<Vec<Wrap>> = vec![];
    let mut last: Vec<Vec<Wrap>> = match first {
        None => {
            sequences.push(vec![]);
            vec![]
        }
        Some(first) => vec![vec![first]],
    };
    sequences.extend(last.iter().cloned());
    for _ in 1..depth {
        let mut next = vec![];
        for sequence in &last {
            for wrap in WRAPS {
                let mut longer: Vec<Wrap> = sequence.clone();
                longer.push(wrap);
                next.push(longer);
            }
        }
        sequences.extend(next.iter().cloned());
        last = next;
    }
    for wraps in &sequences {
        // A trigger's input is one expression: no step comes before it.
        if site == Site::Trigger
            && wraps
                .iter()
                .any(|w| matches!(w, Wrap::LaterStep | Wrap::ForEachItem))
        {
            continue;
        }
        // A read given whole has no path left to read after a combinator.
        let prefix = if base.path.is_empty() {
            0
        } else {
            wraps.iter().take_while(|w| w.same_value()).count()
        };
        for split in 0..=prefix {
            let mut built = Built::new(base.carrier.clone(), false, site);
            let mut applies = true;
            for (i, wrap) in wraps.iter().enumerate() {
                if i == split {
                    built.project(base.path);
                }
                applies &= built.wrap(*wrap, i);
            }
            if !applies {
                continue;
            }
            if split == wraps.len() {
                built.project(base.path);
            }
            built.install(site, &mut content);
            tally.cases += 1;
            let case = || {
                format!(
                    "{} ({site:?}): {wraps:?}, path read after {split}",
                    base.name
                )
            };
            // Ordinary typing accepts the draft, so only the draft check can
            // refuse it: checked for every composition of one combinator and
            // a sample of the deeper ones (it costs ten times the check).
            let sampled = match wraps.len() {
                0 | 1 => true,
                2 => tally.cases % 11 == 0,
                _ => tally.cases % 397 == 0,
            };
            if sampled {
                tally.typed += 1;
                let ordinary = content.validate_structure();
                // Ordinary typing gives a `for_each` item the type its
                // collection has anywhere in the playbook, not at the loop:
                // a join's answer carried as an item types as nullable, and
                // typing itself refuses the read.
                let loose_item = base.loose_item && wraps.contains(&Wrap::ForEachItem);
                if !ordinary.is_empty() && loose_item {
                    tally.typing_stricter += 1;
                } else if !ordinary.is_empty() {
                    tally
                        .failures
                        .push(format!("{}{TYPING_REFUSES}{ordinary:?}", case()));
                    continue;
                }
            }
            let issues = content.draft_issues(&[]);
            if must_refuse(base, wraps, split) {
                tally.must_refuse += 1;
                let found = reasons(&issues);
                if issues.iter().all(|i| i.code != "expression_type") {
                    tally.failures.push(format!("{}{ACCEPTED}", case()));
                } else if found != direct && !(base.two_causes && found.is_subset(&direct)) {
                    tally.failures.push(format!(
                        "{}{OTHER_REASONS}{found:?}, the direct read for {direct:?}",
                        case()
                    ));
                }
            } else {
                tally.must_accept += 1;
                if !issues.is_empty() {
                    tally
                        .failures
                        .push(format!("{}{SOUND_REFUSED}{issues:?}", case()));
                }
            }
        }
    }
    tally
}

#[test]
fn every_wrapped_read_is_refused_or_accepted_as_the_direct_read_is() {
    let bases = bases();
    let mut tally = Tally::default();
    // One task per base, site and first combinator, shared out over the
    // cores. A `decide` consumes the read at the base's full depth; the
    // other sites at depth two at most.
    let mut tasks: Vec<(&Base, Option<Wrap>, Site, usize)> = vec![];
    for base in &bases {
        for site in base.sites {
            let depth = match site {
                Site::Decide => base.depth,
                Site::Input | Site::Output | Site::Trigger => base.depth.min(2),
            };
            tasks.push((base, None, *site, depth));
            tasks.extend(WRAPS.into_iter().map(|w| (base, Some(w), *site, depth)));
        }
    }
    let next = std::sync::atomic::AtomicUsize::new(0);
    let workers = std::thread::available_parallelism().map_or(4, |n| n.get());
    let results: Vec<Tally> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..workers)
            .map(|_| {
                scope.spawn(|| {
                    let mut tally = Tally::default();
                    while let Some((base, first, site, depth)) =
                        tasks.get(next.fetch_add(1, std::sync::atomic::Ordering::Relaxed))
                    {
                        tally.add(check_base(base, *depth, *first, *site));
                    }
                    tally
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for result in results {
        tally.add(result);
    }
    println!(
        "soundness: {} bases, {} combinators, 4 sites, {} compositions ({} must be refused, {} \
         must be accepted; {} also checked against ordinary typing, which refused {} itself), {} \
         failures",
        bases.len(),
        WRAPS.len(),
        tally.cases,
        tally.must_refuse,
        tally.must_accept,
        tally.typed,
        tally.typing_stricter,
        tally.failures.len()
    );
    let count = |what: &str| tally.failures.iter().filter(|f| f.contains(what)).count();
    let shown: Vec<_> = tally.failures.iter().take(12).collect();
    assert!(
        tally.failures.is_empty(),
        "{} of {} generated compositions fail ({} unsound reads accepted, {} refused for other \
         reasons than the direct read, {} sound reads refused, {} that ordinary typing refuses); \
         the first: {shown:#?}",
        tally.failures.len(),
        tally.cases,
        count(ACCEPTED),
        count(OTHER_REASONS),
        count(SOUND_REFUSED),
        count(TYPING_REFUSES),
    );
}

/// The combinators cover every `Expr` variant: adding a variant fails to
/// compile here until a combinator uses it.
#[test]
fn the_combinators_cover_every_expression() {
    fn seen(e: &Expr, used: &mut BTreeSet<&'static str>) {
        let mut all = |es: &mut dyn Iterator<Item = &Expr>| {
            for e in es {
                seen(e, used);
            }
        };
        match e {
            Expr::Literal { .. } => {}
            Expr::Ref { .. } => {}
            Expr::Object { fields } => all(&mut fields.values()),
            Expr::Array { items } => all(&mut items.iter()),
            Expr::Exists { value } | Expr::Not { value } => all(&mut [&**value].into_iter()),
            Expr::And { values } | Expr::Or { values } => all(&mut values.iter()),
            Expr::Eq { left, right }
            | Expr::Lt { left, right }
            | Expr::Difference { left, right, .. } => all(&mut [&**left, &**right].into_iter()),
            Expr::Contains { collection, value } => all(&mut [&**collection, &**value].into_iter()),
            Expr::Map {
                collection, value, ..
            } => all(&mut [&**collection, &**value].into_iter()),
            Expr::Filter {
                collection,
                predicate,
                ..
            } => all(&mut [&**collection, &**predicate].into_iter()),
            Expr::Lookup {
                key,
                cases,
                default,
            } => all(&mut [&**key, &**default]
                .into_iter()
                .chain(cases.iter().map(|c| &c.then))),
            Expr::Let { bindings, body } => {
                all(&mut bindings.iter().map(|b| &b.value).chain([&**body]))
            }
        }
        used.insert(match e {
            Expr::Literal { .. } => "literal",
            Expr::Ref { .. } => "ref",
            Expr::Object { .. } => "object",
            Expr::Array { .. } => "array",
            Expr::Exists { .. } => "exists",
            Expr::Not { .. } => "not",
            Expr::And { .. } => "and",
            Expr::Or { .. } => "or",
            Expr::Eq { .. } => "eq",
            Expr::Lt { .. } => "lt",
            Expr::Contains { .. } => "contains",
            Expr::Map { .. } => "map",
            Expr::Filter { .. } => "filter",
            Expr::Difference { .. } => "difference",
            Expr::Lookup { .. } => "lookup",
            Expr::Let { .. } => "let",
        });
    }
    let mut used = BTreeSet::new();
    for (i, wrap) in WRAPS.into_iter().enumerate() {
        let mut built = Built::new(r("input", &["task"]), true, Site::Decide);
        assert!(built.wrap(wrap, i), "{wrap:?}");
        for layer in &built.layers {
            if let Layer::ForEach { collection, .. } = layer {
                seen(collection, &mut used);
            }
        }
        seen(&built.expr, &mut used);
    }
    assert_eq!(used.len(), 16, "{used:?}");
}
