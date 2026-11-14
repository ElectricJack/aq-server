//! Checks a new draft is held to that `Artifact::verify` does not run, so an
//! artifact stored before them keeps loading (FORMAT.md, "What a draft is
//! held to"):
//! - a join member that is an operation handle is named by its binding alone;
//! - strict reads: a read through a field that some outcome lacks is refused;
//! - bound reads: so is a read of a binding some way to the step never bound;
//! - aliasing does not launder: so is a read through any expression that
//!   carries such a value;
//! - bounded cost: the types an expression reads, and the types in scope
//!   over the steps, are bounded in size.
//!
//! All of it rests on ONE derivation, `Pass::derive`: what every expression
//! carries (`Carried`: the forms its value can have on each way of reaching
//! the step, the ways it is Missing, and its expanded size). It is one match
//! over every `Expr` variant; `ArtifactContent::parts` is one match over every
//! `Operation` and says how each step puts a value in scope (`Held`); and
//! `Pass::reserved` is one match over the reserved roots. None has a wildcard
//! arm, and the lints below refuse one, so a new variant does not compile
//! until it is decided here. A read is refused where a Missing value is
//! consumed (`Pass::sink`): there is no second walk over expressions.
//!
//! Ordinary typing merges the outcomes a binding can hold into one object, in
//! which a field only one outcome has is merely optional, and an optional
//! field types as present. Here each binding keeps its outcomes apart.
#![deny(
    clippy::wildcard_enum_match_arm,
    clippy::match_wildcard_for_single_variants
)]
use super::*;
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;

/// The most nodes one expression of a new draft may count, and all of its
/// expressions together. An expression counts its own nodes, a literal the
/// nodes of its value, and every reference the nodes of the type it reads
/// (whatever it refers to: a root, a binding, a `let` name, an item). What a
/// name stands for is counted once, where it is written; a name whose type
/// doubles at every level still counts double at every level.
pub const MAX_DRAFT_EXPANSION: u64 = 200_000;
pub const MAX_DRAFT_EXPANSION_TOTAL: u64 = 1_000_000;
/// The most type nodes all the steps of a draft may have in scope together:
/// for each step, the types of its entry's `input`, of the definitions and of
/// every binding and item of the playbook. Ordinary typing copies them for
/// every step, several times, when a draft is checked and on every load.
pub const MAX_DRAFT_SCOPE: u64 = 250_000;
/// The most times the flow over the steps may revisit them before the draft
/// is refused as too entangled to check.
const MAX_FLOW_VISITS_PER_STEP: usize = 64;
/// A value with more shapes than this counts toward the work of following
/// values of many shapes, which is bounded over the whole draft.
const MANY_SHAPES: u64 = 16;
const MAX_SHAPE_WORK: u64 = 1_000_000;
/// The most reasons one diagnostic lists.
const MAX_REASONS: usize = 12;
/// The deepest a `for_each` item's derived value may nest.
const MAX_ITEM_DEPTH: u32 = 256;

// ---------------------------------------------------------------------------
// What a value is.
// ---------------------------------------------------------------------------

/// What a value can be, over every way of reaching the step that reads it.
#[derive(Clone)]
struct Carried<'a> {
    /// Ways on which the value is not there, not yet read by any reference:
    /// only the values steps put in scope have these.
    lacks: Vec<Rc<str>>,
    /// Ways on which the value is Missing because a reference inside the
    /// expression that computed it read something that was not there.
    gaps: Gaps,
    /// What it is where it is there: one form per way.
    forms: Rc<[Form<'a>]>,
    /// Nodes of its type, as ordinary typing would build it.
    nodes: u64,
    /// How deep its built forms nest.
    depth: u32,
}
/// One form a present value has.
#[derive(Clone)]
enum Form<'a> {
    /// A value of this schema, arising on `way`; `name` is how a direct
    /// reference writes it. A field optional in the schema is readable.
    Shape {
        schema: &'a Schema,
        way: Rc<str>,
        name: Rc<str>,
    },
    /// A present value whose inside this check does not follow (a literal, a
    /// boolean, a scalar, a result ordinary typing gives no shape); the
    /// number is the node count of its type.
    Opaque(u64),
    /// An object or array an expression builds, a named join's result, or
    /// `vars`: each key or position with what it carries.
    Built(Rc<Built<'a>>),
    /// An array whose every item is this (the result of `map`, `filter` or
    /// `difference`).
    Items(Rc<Carried<'a>>),
}
struct Built<'a> {
    /// The field names of an object, sorted; none for an array.
    fields: Vec<String>,
    /// What each field, or each position, carries.
    values: Vec<Carried<'a>>,
    array: bool,
    nodes: u64,
    depth: u32,
}
impl<'a> Built<'a> {
    fn get(&self, key: &str) -> Option<&Carried<'a>> {
        let at = if self.array {
            key.parse::<usize>().ok()?
        } else {
            self.fields.binary_search_by(|f| f.as_str().cmp(key)).ok()?
        };
        self.values.get(at)
    }
}
/// A set of gaps, shared: joining two sets never copies either.
type Gaps = Option<Rc<GapNode>>;
enum GapNode {
    /// `read` found the value missing for `reason`.
    Leaf {
        reason: Rc<str>,
        read: Rc<Read>,
    },
    All(Vec<Rc<GapNode>>),
    /// The gaps of the value a lexical name holds, used through that name.
    Via(Rc<str>, Rc<GapNode>),
}
/// A reference, as written, that read a value some way lacks.
struct Read {
    at: String,
    written: String,
    note: Option<String>,
    /// Read as the identity of a `difference` item: `exists` cannot test it.
    identity: bool,
}
fn join_gaps(parts: impl IntoIterator<Item = Gaps>) -> Gaps {
    let mut nodes: Vec<Rc<GapNode>> = parts.into_iter().flatten().collect();
    if nodes.len() > 1 {
        Some(Rc::new(GapNode::All(nodes)))
    } else {
        nodes.pop()
    }
}

/// Identity of a form, for keeping each once.
#[derive(PartialEq, Eq, Hash)]
enum FormKey {
    Shape(*const Schema, Rc<str>),
    Opaque(u64),
    Built(*const ()),
    Items(*const ()),
}
impl Form<'_> {
    fn key(&self) -> FormKey {
        match self {
            Form::Shape { schema, way, .. } => {
                FormKey::Shape(std::ptr::from_ref(*schema), way.clone())
            }
            Form::Opaque(nodes) => FormKey::Opaque(*nodes),
            Form::Built(built) => FormKey::Built(Rc::as_ptr(built).cast()),
            Form::Items(items) => FormKey::Items(Rc::as_ptr(items).cast()),
        }
    }
}
/// Identity of a built value: equal ones are built once and shared, so that
/// deriving the same value again gives the very same form.
#[derive(PartialEq, Eq, Hash)]
enum BuiltKey {
    Parts(bool, Vec<(String, PartKey)>),
    Items(PartKey),
}
type PartKey = (Vec<Rc<str>>, Vec<FormKey>);
/// `None` where the part has gaps of its own: such a value is not shared.
fn part_key(part: &Carried<'_>) -> Option<PartKey> {
    part.gaps.is_none().then(|| {
        (
            part.lacks.clone(),
            part.forms.iter().map(Form::key).collect(),
        )
    })
}
/// Collects what a value carries, each form and lack once.
#[derive(Default)]
struct Build<'a> {
    lacks: Vec<Rc<str>>,
    lacking: HashSet<Rc<str>>,
    gaps: Vec<Gaps>,
    forms: Vec<Form<'a>>,
    seen: HashSet<FormKey>,
}
impl<'a> Build<'a> {
    fn form(&mut self, form: Form<'a>) {
        if self.seen.insert(form.key()) {
            self.forms.push(form);
        }
    }
    fn lack(&mut self, text: &str) {
        if !self.lacking.contains(text) {
            let text: Rc<str> = text.into();
            self.lacking.insert(text.clone());
            self.lacks.push(text);
        }
    }
    /// Everything `other` carries.
    fn all(&mut self, other: &Carried<'a>) {
        for lack in &other.lacks {
            self.lack(lack);
        }
        self.gaps.push(other.gaps.clone());
        for form in other.forms.iter() {
            self.form(form.clone());
        }
    }
}

// ---------------------------------------------------------------------------
// How a value enters scope.
// ---------------------------------------------------------------------------

/// What a step leaves in its binding (or a `for_each` in its item) on one
/// exit. Every kind of step binding is one of these.
enum Held<'a> {
    /// A value of this schema: an invoke or call outcome, a received event.
    Shape(&'a Schema),
    /// Nothing: the name is not bound on this exit.
    Unbound,
    /// A value ordinary typing gives no shape, so no read of it types.
    Unfollowed,
    /// A named join's result: what each key holds.
    Members(BTreeMap<&'a str, Vec<Member<'a>>>),
    /// An item of this collection, evaluated where the step runs.
    ItemOf(&'a Expr),
}
/// What one key of a named join's result holds on one edge.
enum Member<'a> {
    /// The member answered with this outcome and output.
    Answer(&'a str, &'a Schema),
    /// The member's answer, any outcome, perhaps without outcome and output.
    Loose(&'a Schema),
    /// The key is not in the result, for this reason.
    Absent(&'static str),
}
/// One way out of a step: where it leads, and what it leaves bound.
struct Exit<'a> {
    to: &'a str,
    /// Names the way, for a diagnostic.
    way: String,
    holds: Option<(&'a str, Held<'a>)>,
}
/// Everything about one step this check uses: the expressions it evaluates
/// and its exits.
struct Parts<'a> {
    exprs: Vec<(String, &'a Expr)>,
    exits: Vec<Exit<'a>>,
    /// A `for_each`'s own item: not bound while the step's own expression is
    /// evaluated, which is when the loop is entered.
    own_item: Option<&'a str>,
}
/// A reserved root.
#[derive(Clone, Copy)]
enum Reserved {
    Input,
    Event,
    Definitions,
    Vars,
}
impl Reserved {
    /// In the order of `RESERVED_ROOTS`.
    const ALL: [Reserved; 4] = [
        Reserved::Input,
        Reserved::Event,
        Reserved::Definitions,
        Reserved::Vars,
    ];
    fn name(self) -> &'static str {
        match self {
            Reserved::Input => "input",
            Reserved::Event => "event",
            Reserved::Definitions => "definitions",
            Reserved::Vars => "vars",
        }
    }
    fn named(root: &str) -> Option<Reserved> {
        Self::ALL.into_iter().find(|r| r.name() == root)
    }
}
/// Where an expression is evaluated.
#[derive(Clone, Copy)]
enum Place<'a, 'c> {
    /// In a step of `entry`, with these bindings.
    Step {
        entry_name: &'a str,
        entry: &'a EntryPoint,
        context: &'c Context<'a>,
    },
    /// In a trigger's `input`, with this event payload.
    Trigger { payload: Option<&'a Schema> },
}
/// Every binding and `for_each` item of the playbook at one step.
type Context<'a> = BTreeMap<&'a str, Rc<Carried<'a>>>;

/// Schemas this check reads that the artifact does not hold as such.
struct Sources {
    definitions: Option<Schema>,
    child: Schema,
    /// Per named join step: each key's answer of any outcome.
    loose: BTreeMap<String, BTreeMap<String, Schema>>,
    /// Per named join step: each member's outcomes.
    outcomes: BTreeMap<String, Vec<BTreeMap<String, Schema>>>,
}
impl Sources {
    fn new(content: &ArtifactContent) -> Self {
        let definitions = literal_schema(&Value::Object(
            content
                .definitions
                .iter()
                .map(|(k, d)| (k.clone(), d.value.clone()))
                .collect(),
        ))
        .ok();
        let mut loose = BTreeMap::new();
        let mut outcomes = BTreeMap::new();
        for (name, step) in &content.playbook.steps {
            let Operation::Join {
                members,
                member_keys,
                ..
            } = &step.op
            else {
                continue;
            };
            if member_keys.is_empty() || member_keys.len() != members.len() {
                continue;
            }
            let Ok(Some(Schema::Object { properties, .. })) = content.join_schema(step, false)
            else {
                continue;
            };
            let Ok(each) = members
                .iter()
                .map(|m| content.member_outcomes(m))
                .collect::<Result<Vec<_>, _>>()
            else {
                continue;
            };
            loose.insert(name.clone(), properties);
            outcomes.insert(name.clone(), each);
        }
        Self {
            definitions,
            child: Schema::object(BTreeMap::from([("child".into(), Schema::string(128))])),
            loose,
            outcomes,
        }
    }
}

impl ArtifactContent {
    /// The expressions and exits of the step `name`: the one place that says,
    /// for every kind of operation, how it puts a value in scope.
    fn parts<'a>(&'a self, sources: &'a Sources, name: &'a str, step: &'a Step) -> Parts<'a> {
        let edges = |holds: &dyn Fn(&'a str) -> (String, Option<(&'a str, Held<'a>)>)| {
            step.edges
                .iter()
                .map(|(edge, to)| {
                    let (way, holds) = holds(edge);
                    Exit { to, way, holds }
                })
                .collect::<Vec<_>>()
        };
        let failed = |bind: Option<&'a str>| {
            step.on_error.as_deref().map(|to| Exit {
                to,
                way: format!("the `on_error` exit of step `{name}`"),
                holds: bind.map(|b| (b, Held::Unbound)),
            })
        };
        let outcome = |edge: &str| format!("outcome `{edge}` of step `{name}`");
        let (exprs, mut exits, bind): (Vec<(String, &Expr)>, Vec<Exit>, Option<&str>) = match &step
            .op
        {
            Operation::Invoke {
                command,
                input,
                bind,
                retry: _,
            } => {
                let outcomes = self.registry.commands.get(&command.name);
                let exits = edges(&|edge| {
                    let held = match outcomes.and_then(|c| c.outcomes.get(edge)) {
                        Some(schema) => Held::Shape(schema),
                        None => Held::Unfollowed,
                    };
                    (outcome(edge), Some((bind.as_str(), held)))
                });
                (vec![("input".into(), input)], exits, Some(bind))
            }
            Operation::Decide { predicate } => (
                vec![("predicate".into(), predicate)],
                edges(&|edge| (format!("edge `{edge}` of step `{name}`"), None)),
                None,
            ),
            Operation::Wait {
                event,
                correlation,
                minimum_revision,
                deadline_ms: _,
                bind,
            } => {
                let payload = event
                    .as_ref()
                    .and_then(|e| self.registry.events.get(&e.name))
                    .map(|e| &e.payload);
                // Only a received event binds; a timeout, and a timer,
                // bind nothing.
                let exits = edges(&|edge| {
                    let held = match (event, edge) {
                        (Some(_), "received") => match payload {
                            Some(schema) => Held::Shape(schema),
                            None => Held::Unfollowed,
                        },
                        (Some(_) | None, _) => Held::Unbound,
                    };
                    (
                        format!("edge `{edge}` of wait `{name}`"),
                        Some((bind.as_str(), held)),
                    )
                });
                let exprs = correlation
                    .iter()
                    .map(|(k, e)| (format!("correlation.{k}"), e))
                    .chain(
                        minimum_revision
                            .iter()
                            .map(|e| ("minimum_revision".into(), e)),
                    )
                    .collect();
                (exprs, exits, Some(bind))
            }
            Operation::ForEach {
                collection,
                item,
                body,
                next,
                max_items: _,
                concurrency: _,
                on_failure: _,
            } => {
                // The item is bound for the body only: the runner removes
                // it when the loop ends or fails.
                let mut exits = vec![
                    Exit {
                        to: body,
                        way: format!("the body of for_each `{name}`"),
                        holds: Some((item, Held::ItemOf(collection))),
                    },
                    Exit {
                        to: next,
                        way: format!("the `next` exit of for_each `{name}`"),
                        holds: Some((item, Held::Unbound)),
                    },
                ];
                exits.extend(edges(&|edge| {
                    (
                        format!("edge `{edge}` of for_each `{name}`"),
                        Some((item.as_str(), Held::Unbound)),
                    )
                }));
                (vec![("collection".into(), collection)], exits, Some(item))
            }
            Operation::Call {
                dependency,
                entry,
                input,
                mode,
                bind,
            } => {
                let interface = self
                    .dependencies
                    .get(dependency)
                    .and_then(|d| d.interfaces.get(entry));
                let exits = edges(&|edge| {
                    let held = match mode {
                        CallMode::Wait => match interface.and_then(|i| i.outcomes.get(edge)) {
                            Some(schema) => Held::Shape(schema),
                            None => Held::Unfollowed,
                        },
                        // The child's identity: `{child}`.
                        CallMode::OwnedAsync | CallMode::Independent => Held::Shape(&sources.child),
                    };
                    (outcome(edge), Some((bind.as_str(), held)))
                });
                (vec![("input".into(), input)], exits, Some(bind))
            }
            Operation::Join {
                members,
                member_keys,
                mode,
                accepted_outcomes,
                deadline_ms: _,
                remaining: _,
                bind,
            } => {
                let exits = edges(&|edge| {
                    let held = match (sources.loose.get(name), sources.outcomes.get(name)) {
                        (Some(loose), Some(outcomes)) => join_members(
                            edge,
                            mode,
                            accepted_outcomes,
                            member_keys,
                            loose,
                            outcomes,
                        ),
                        // Without keys the result is keyed by run-time
                        // identities, and has no type.
                        (Some(_) | None, Some(_) | None) => Held::Unfollowed,
                    };
                    (
                        format!("edge `{edge}` of join `{name}`"),
                        Some((bind.as_str(), held)),
                    )
                });
                let exprs = members
                    .iter()
                    .enumerate()
                    .map(|(i, e)| (format!("members[{i}]"), e))
                    .collect();
                (exprs, exits, Some(bind))
            }
            Operation::Return { outcome: _, output } => (
                vec![("output".into(), output)],
                edges(&|edge| (format!("edge `{edge}` of step `{name}`"), None)),
                None,
            ),
        };
        exits.extend(failed(bind));
        let own_item = match &step.op {
            Operation::ForEach { item, .. } => Some(item.as_str()),
            Operation::Invoke { .. }
            | Operation::Decide { .. }
            | Operation::Wait { .. }
            | Operation::Call { .. }
            | Operation::Join { .. }
            | Operation::Return { .. } => None,
        };
        Parts {
            exprs,
            exits,
            own_item,
        }
    }
}
/// What each key of a named join's result holds on `edge`: one answer per
/// outcome the member can have there, and `Absent` where the key can be
/// missing.
fn join_members<'a>(
    edge: &str,
    mode: &JoinMode,
    accepted: &BTreeSet<String>,
    keys: &'a [String],
    loose: &'a BTreeMap<String, Schema>,
    outcomes: &'a [BTreeMap<String, Schema>],
) -> Held<'a> {
    let several = keys.len() > 1;
    let mut held = BTreeMap::new();
    for (key, outcomes) in keys.iter().zip(outcomes) {
        let Some(any) = loose.get(key) else {
            return Held::Unfollowed;
        };
        let qualified = || {
            outcomes
                .iter()
                .filter(|(o, _)| accepted.is_empty() || accepted.contains(*o))
                .map(|(o, schema)| Member::Answer(o, schema))
                .collect::<Vec<_>>()
        };
        let unanswered = Member::Absent("when that member has not answered");
        let none = vec![Member::Absent("where no member is kept")];
        let members = match (mode, edge) {
            (JoinMode::All, "completed") => qualified(),
            (JoinMode::All, "no_match") => vec![Member::Loose(any)],
            (JoinMode::AnyAccepted, "completed") => {
                let mut members = qualified();
                // With one member, that member is the one that qualified.
                if several {
                    members.push(Member::Absent("when another member qualified first"));
                }
                members
            }
            (JoinMode::AnyCompletion, "completed") => {
                let mut members = vec![Member::Loose(any)];
                // With one member, that member is the one that completed.
                if several {
                    members.push(Member::Absent("when another member completed first"));
                }
                members
            }
            (JoinMode::All | JoinMode::AnyAccepted | JoinMode::AnyCompletion, "timeout") => {
                vec![Member::Loose(any), unanswered]
            }
            (JoinMode::All | JoinMode::AnyAccepted | JoinMode::AnyCompletion, _) => none,
        };
        held.insert(key.as_str(), members);
    }
    Held::Members(held)
}

// ---------------------------------------------------------------------------
// The derivation.
// ---------------------------------------------------------------------------

/// A lexical name: a `let` name or a `map` or `filter` item.
#[derive(Clone)]
struct Named<'a> {
    carried: Carried<'a>,
    /// The reference it stands for, as a direct read writes it.
    origin: Option<Rc<str>>,
    /// What it is, for a diagnostic: "is `work`", "is an item of …".
    note: Rc<str>,
}
type Scope<'a> = BTreeMap<&'a str, Named<'a>>;
/// What `derive` says of one expression.
struct Out<'a> {
    carried: Carried<'a>,
    /// Its size with every name replaced by what it stands for. Saturating.
    size: u64,
    /// Set for a reference: what it reads, as a direct read writes it.
    origin: Option<Rc<str>>,
}
/// A read some way cannot satisfy, consumed where its value is needed.
struct Refusal {
    read: Rc<Read>,
    reasons: Vec<Rc<str>>,
    /// Reasons beyond the ones listed.
    more: usize,
    /// Consumed as a lookup key.
    key: bool,
    /// The lexical names its value was used through.
    through: Vec<Rc<str>>,
}
/// What one name contributes to an expression's expanded size.
#[derive(Clone, Default)]
struct Charge {
    each: u64,
    times: u64,
    total: u64,
}
/// The size of one expression of the draft.
struct ExprSize {
    path: String,
    at: String,
    size: u64,
    /// The name that contributes most.
    most: Option<(String, Charge)>,
}

/// What stays the same over the whole draft.
struct Check<'a> {
    content: &'a ArtifactContent,
    sources: &'a Sources,
    /// Nodes of each schema, by address.
    schema_nodes: RefCell<HashMap<*const Schema, u64>>,
    /// Every built value, by what it is made of.
    built: RefCell<HashMap<BuiltKey, Form<'a>>>,
    /// What each literal is, by where it is written.
    literals: RefCell<HashMap<*const Value, Carried<'a>>>,
    /// A present scalar.
    scalar: Carried<'a>,
    /// Shapes followed so far, of values with many.
    work: Cell<u64>,
    /// What each `for_each` item is, for a diagnostic.
    item_notes: BTreeMap<&'a str, Rc<str>>,
}
/// Evaluates expressions, collecting the reads it refuses.
struct Pass<'a, 'k> {
    check: &'k Check<'a>,
    refusals: Vec<Refusal>,
    by_read: HashMap<*const Read, usize>,
    /// Gap sets already consumed, kept alive so an address is never reused.
    sunk: HashSet<(*const GapNode, bool)>,
    kept: Vec<Rc<GapNode>>,
    charges: BTreeMap<String, Charge>,
}

fn value_nodes(value: &Value) -> u64 {
    match value {
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => 1,
        Value::Array(items) => items
            .iter()
            .fold(1u64, |n, v| n.saturating_add(value_nodes(v))),
        Value::Object(map) => map
            .values()
            .fold(1u64, |n, v| n.saturating_add(value_nodes(v))),
    }
}
/// "<way> has no `key` in `at` (it has a, b)", or "(it has no fields)".
fn no_field(way: &str, key: &str, at: &str, properties: &BTreeMap<String, Schema>) -> String {
    let has = if properties.is_empty() {
        "no fields".to_string()
    } else {
        properties.keys().cloned().collect::<Vec<_>>().join(", ")
    };
    format!("{way} has no `{key}` in `{at}` (it has {has})")
}

impl<'a> Check<'a> {
    fn nodes(&self, schema: &Schema) -> u64 {
        if let Some(n) = self.schema_nodes.borrow().get(&std::ptr::from_ref(schema)) {
            return *n;
        }
        let sum = |parts: &mut dyn Iterator<Item = &Schema>| {
            parts.fold(1u64, |n, s| n.saturating_add(self.nodes(s)))
        };
        let n = match schema {
            Schema::Null | Schema::Boolean | Schema::Integer { .. } | Schema::String { .. } => 1,
            Schema::Array {
                items,
                max_items: _,
            } => sum(&mut [&**items].into_iter()),
            Schema::Object {
                properties,
                required: _,
            } => sum(&mut properties.values()),
            Schema::Nullable { inner } => sum(&mut [&**inner].into_iter()),
            Schema::Union { variants } => sum(&mut variants.iter()),
        };
        self.schema_nodes
            .borrow_mut()
            .insert(std::ptr::from_ref(schema), n);
        n
    }
    fn finish(&self, build: Build<'a>) -> Carried<'a> {
        let mut nodes = 0u64;
        let mut depth = 0;
        for form in &build.forms {
            let (n, d) = match form {
                Form::Shape { schema, .. } => (self.nodes(schema), 0),
                Form::Opaque(n) => (*n, 0),
                Form::Built(built) => (built.nodes, built.depth),
                Form::Items(items) => (items.nodes.saturating_add(1), items.depth + 1),
            };
            nodes = nodes.saturating_add(n);
            depth = depth.max(d);
        }
        Carried {
            lacks: build.lacks,
            gaps: join_gaps(build.gaps),
            forms: build.forms.into(),
            nodes,
            depth,
        }
    }
    fn of(&self, form: Form<'a>) -> Carried<'a> {
        let mut build = Build::default();
        build.form(form);
        self.finish(build)
    }
    fn opaque(&self, nodes: u64) -> Carried<'a> {
        self.of(Form::Opaque(nodes.max(1)))
    }
    fn shape(&self, schema: &'a Schema, way: &str, name: &str) -> Carried<'a> {
        self.of(Form::Shape {
            schema,
            way: way.into(),
            name: name.into(),
        })
    }
    fn lacking(&self, text: &str) -> Carried<'a> {
        let mut build = Build::default();
        build.lack(text);
        self.finish(build)
    }
    /// An object of `parts` by field, or an array of them in their order.
    fn built(&self, parts: Vec<(String, Carried<'a>)>, array: bool) -> Carried<'a> {
        let key = parts
            .iter()
            .map(|(k, part)| Some((k.clone(), part_key(part)?)))
            .collect::<Option<Vec<_>>>()
            .map(|parts| BuiltKey::Parts(array, parts));
        match key {
            Some(key) => self.shared(Some(key), || self.assembled_form(parts, array)),
            None => self.assembled(parts, array),
        }
    }
    /// An object (or array) of `parts`, not looked up among those built
    /// before (a literal is found again by where it is written).
    fn assembled(&self, parts: Vec<(String, Carried<'a>)>, array: bool) -> Carried<'a> {
        self.of(self.assembled_form(parts, array))
    }
    fn assembled_form(&self, mut parts: Vec<(String, Carried<'a>)>, array: bool) -> Form<'a> {
        if !array {
            parts.sort_by(|a, b| a.0.cmp(&b.0));
        }
        let nodes = parts
            .iter()
            .fold(1u64, |n, (_, c)| n.saturating_add(c.nodes.max(1)));
        let depth = parts.iter().map(|(_, c)| c.depth).max().unwrap_or(0) + 1;
        let (fields, values): (Vec<String>, Vec<Carried>) = parts.into_iter().unzip();
        Form::Built(Rc::new(Built {
            fields: if array { vec![] } else { fields },
            values,
            array,
            nodes,
            depth,
        }))
    }
    /// An array whose every item is `item`.
    fn items(&self, item: Carried<'a>) -> Carried<'a> {
        let key = part_key(&item).map(BuiltKey::Items);
        self.shared(key, || Form::Items(Rc::new(item)))
    }
    /// The form `make` builds, or the equal one built before.
    fn shared(&self, key: Option<BuiltKey>, make: impl FnOnce() -> Form<'a>) -> Carried<'a> {
        let Some(key) = key else {
            return self.of(make());
        };
        if let Some(form) = self.built.borrow().get(&key) {
            return self.of(form.clone());
        }
        let form = make();
        self.built.borrow_mut().insert(key, form.clone());
        self.of(form)
    }
    /// What a literal value is: an object or array as it is written, a
    /// scalar as a present value. The same literal gives the same forms
    /// every time it is derived.
    fn literal(&self, value: &'a Value) -> Carried<'a> {
        if let Some(known) = self.literals.borrow().get(&std::ptr::from_ref(value)) {
            return known.clone();
        }
        let carried = self.literal_value(value);
        self.literals
            .borrow_mut()
            .insert(std::ptr::from_ref(value), carried.clone());
        carried
    }
    fn literal_value(&self, value: &Value) -> Carried<'a> {
        match value {
            Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {
                self.scalar.clone()
            }
            Value::Array(items) => self.assembled(
                items
                    .iter()
                    .map(|v| (String::new(), self.literal_value(v)))
                    .collect(),
                true,
            ),
            Value::Object(map) => self.assembled(
                map.iter()
                    .map(|(k, v)| (k.clone(), self.literal_value(v)))
                    .collect(),
                false,
            ),
        }
    }
    /// What a step's exit leaves in `bind`. `item` derives an item's value.
    fn held(
        &self,
        bind: &str,
        way: &str,
        held: &Held<'a>,
        item: &mut dyn FnMut(&'a Expr) -> Carried<'a>,
    ) -> Carried<'a> {
        match held {
            Held::Shape(schema) => self.shape(schema, way, bind),
            Held::Unbound => self.lacking(&format!("on {way}, `{bind}` is not bound")),
            Held::Unfollowed => self.opaque(1),
            Held::Members(keys) => {
                let mut parts = vec![];
                for (key, members) in keys {
                    let mut build = Build::default();
                    for member in members {
                        match member {
                            Member::Answer(outcome, output) => {
                                let answered =
                                    format!("outcome `{outcome}` of member `{key}` on {way}");
                                let output =
                                    self.shape(output, &answered, &format!("{bind}.{key}.output"));
                                build.all(&self.built(
                                    vec![
                                        ("participant".to_string(), self.opaque(1)),
                                        ("outcome".to_string(), self.opaque(1)),
                                        ("output".to_string(), output),
                                    ],
                                    false,
                                ));
                            }
                            Member::Loose(schema) => build.form(Form::Shape {
                                schema,
                                way: way.into(),
                                name: format!("{bind}.{key}").into(),
                            }),
                            Member::Absent(why) => {
                                build.lack(&format!("on {way}, {why}, `{bind}.{key}` is absent"));
                            }
                        }
                    }
                    parts.push(((*key).to_string(), self.finish(build)));
                }
                self.built(parts, false)
            }
            Held::ItemOf(collection) => item(collection),
        }
    }
    /// `from`, followed along `path[depth..]`, into `out`.
    fn follow(&self, from: &Carried<'a>, path: &[String], depth: usize, out: &mut Build<'a>) {
        let Some(key) = path.get(depth) else {
            out.all(from);
            return;
        };
        for lack in &from.lacks {
            out.lack(lack);
        }
        out.gaps.push(from.gaps.clone());
        for form in from.forms.iter() {
            match form {
                Form::Shape { schema, way, name } => {
                    self.follow_schema(schema, way, name, &path[depth..], 0, out);
                }
                Form::Opaque(nodes) => out.form(Form::Opaque(*nodes)),
                Form::Built(built) => {
                    match built.get(key) {
                        Some(part) => self.follow(part, path, depth + 1, out),
                        // Said without the name it is read through, so that
                        // a read through another name gives the same reason.
                        None => out.lack(&if built.array {
                            format!(
                                "position `{key}` is past the end of an array written with {} \
                                 item{}",
                                built.values.len(),
                                if built.values.len() == 1 { "" } else { "s" }
                            )
                        } else if built.fields.is_empty() {
                            format!("`{key}` is not in an object written with no fields")
                        } else {
                            format!(
                                "`{key}` is not in an object written with the fields {}",
                                built.fields.join(", ")
                            )
                        }),
                    }
                }
                Form::Items(item) => {
                    if key.parse::<usize>().is_ok() {
                        self.follow(item, path, depth + 1, out);
                    } else {
                        out.form(Form::Opaque(1));
                    }
                }
            }
        }
    }
    fn follow_schema(
        &self,
        schema: &'a Schema,
        way: &Rc<str>,
        name: &Rc<str>,
        path: &[String],
        depth: usize,
        out: &mut Build<'a>,
    ) {
        let Some(key) = path.get(depth) else {
            out.form(Form::Shape {
                schema,
                way: way.clone(),
                name: dotted(name, path).into(),
            });
            return;
        };
        match schema {
            Schema::Union { variants } => {
                for v in variants {
                    self.follow_schema(v, way, name, path, depth, out);
                }
            }
            Schema::Object {
                properties,
                required: _,
            } => match properties.get(key) {
                // A field merely optional in one schema is readable.
                Some(next) => self.follow_schema(next, way, name, path, depth + 1, out),
                None => out.lack(&no_field(
                    way,
                    key,
                    &dotted(name, &path[..depth]),
                    properties,
                )),
            },
            Schema::Array {
                items,
                max_items: _,
            } => self.follow_schema(items, way, name, path, depth + 1, out),
            // Ordinary typing refuses to read inside these.
            Schema::Nullable { inner: _ }
            | Schema::Null
            | Schema::Boolean
            | Schema::Integer { .. }
            | Schema::String { .. } => out.form(Form::Opaque(1)),
        }
    }
    /// What an item of the array `collection` is.
    fn items_of(&self, collection: &Carried<'a>) -> Carried<'a> {
        let mut out = Build::default();
        for form in collection.forms.iter() {
            match form {
                Form::Shape { schema, way, name } => {
                    self.items_of_schema(schema, way, &format!("{name}[]"), &mut out);
                }
                Form::Opaque(nodes) => out.form(Form::Opaque(*nodes)),
                Form::Built(built) => {
                    if built.array {
                        for part in &built.values {
                            out.all(part);
                        }
                    } else {
                        out.form(Form::Opaque(1));
                    }
                }
                Form::Items(item) => out.all(item),
            }
        }
        // Where the collection is Missing nothing has an item; its own read
        // carries that.
        out.gaps.clear();
        self.finish(out)
    }
    fn items_of_schema(&self, schema: &'a Schema, way: &Rc<str>, name: &str, out: &mut Build<'a>) {
        match schema {
            Schema::Array {
                items,
                max_items: _,
            } => out.form(Form::Shape {
                schema: items,
                way: way.clone(),
                name: name.into(),
            }),
            Schema::Union { variants } => {
                for v in variants {
                    self.items_of_schema(v, way, name, out);
                }
            }
            Schema::Object { .. }
            | Schema::Nullable { .. }
            | Schema::Null
            | Schema::Boolean
            | Schema::Integer { .. }
            | Schema::String { .. } => out.form(Form::Opaque(1)),
        }
    }
}

impl<'a, 'k> Pass<'a, 'k> {
    fn new(check: &'k Check<'a>) -> Self {
        Self {
            check,
            refusals: vec![],
            by_read: HashMap::new(),
            sunk: HashSet::new(),
            kept: vec![],
            charges: BTreeMap::new(),
        }
    }
    /// What `expr`, at location `at`, carries: the one derivation.
    fn derive(
        &mut self,
        expr: &'a Expr,
        at: &str,
        scope: &Scope<'a>,
        place: Place<'a, '_>,
    ) -> Out<'a> {
        let check = self.check;
        let under = |field: &str| operand(at, expr, field);
        // A test of its operands: a boolean, Missing where any operand is.
        let test = |parts: Vec<Out<'a>>| {
            let mut carried = check.opaque(1);
            carried.gaps = join_gaps(parts.iter().map(|o| o.carried.gaps.clone()));
            Out {
                carried,
                size: parts.iter().fold(1u64, |n, o| n.saturating_add(o.size)),
                origin: None,
            }
        };
        // A value built from parts: Missing where any part is.
        let build = |parts: Vec<(String, Out<'a>)>, array: bool| {
            let size = parts
                .iter()
                .fold(1u64, |n, (_, o)| n.saturating_add(o.size));
            let gaps = join_gaps(parts.iter().map(|(_, o)| o.carried.gaps.clone()));
            let mut carried = check.built(
                parts
                    .into_iter()
                    .map(|(k, o)| {
                        let mut part = o.carried;
                        part.gaps = None;
                        (k, part)
                    })
                    .collect(),
                array,
            );
            carried.gaps = gaps;
            Out {
                carried,
                size,
                origin: None,
            }
        };
        match expr {
            // A literal is the value an `object` or `array` expression of the
            // same shape would build: each row keeps its own fields, and an
            // array its own length.
            Expr::Literal { value } => {
                let carried = check.literal(value);
                Out {
                    size: carried.nodes.max(1),
                    carried,
                    origin: None,
                }
            }
            Expr::Ref { root, path } => self.read(root, path, at, scope, place),
            Expr::Object { fields } => {
                let parts = fields
                    .iter()
                    .map(|(k, e)| {
                        (
                            k.clone(),
                            self.derive(e, &under(&format!(".{k}")), scope, place),
                        )
                    })
                    .collect();
                build(parts, false)
            }
            Expr::Array { items } => {
                let parts = items
                    .iter()
                    .enumerate()
                    .map(|(i, e)| {
                        (
                            i.to_string(),
                            self.derive(e, &under(&format!("[{i}]")), scope, place),
                        )
                    })
                    .collect();
                build(parts, true)
            }
            // The one place a Missing value stops: `exists` makes it `false`.
            Expr::Exists { value } => {
                let tested = self.derive(value, &under(".value"), scope, place);
                Out {
                    carried: check.opaque(1),
                    size: tested.size.saturating_add(1),
                    origin: None,
                }
            }
            Expr::Not { value } => test(vec![self.derive(value, &under(".value"), scope, place)]),
            Expr::And { values } | Expr::Or { values } => test(
                values
                    .iter()
                    .enumerate()
                    .map(|(i, e)| self.derive(e, &under(&format!(".values[{i}]")), scope, place))
                    .collect(),
            ),
            Expr::Eq { left, right } | Expr::Lt { left, right } => test(vec![
                self.derive(left, &under(".left"), scope, place),
                self.derive(right, &under(".right"), scope, place),
            ]),
            Expr::Contains { collection, value } => test(vec![
                self.derive(collection, &under(".collection"), scope, place),
                self.derive(value, &under(".value"), scope, place),
            ]),
            Expr::Map {
                collection,
                item,
                value,
                max_items: _,
            } => {
                let over = self.derive(collection, &under(".collection"), scope, place);
                let inner = self.with_item(scope, item, &over);
                let each = self.derive(value, &under(".value"), &inner, place);
                // Missing where the collection is, or where the body is for
                // any item; otherwise every item is what the body carries.
                let gaps = join_gaps([over.carried.gaps.clone(), each.carried.gaps.clone()]);
                let mut mapped = each.carried;
                mapped.gaps = None;
                let mut carried = check.items(mapped);
                carried.gaps = gaps;
                Out {
                    carried,
                    size: over.size.saturating_add(each.size).saturating_add(1),
                    origin: None,
                }
            }
            Expr::Filter {
                collection,
                item,
                predicate,
                max_items: _,
            } => {
                let over = self.derive(collection, &under(".collection"), scope, place);
                let inner = self.with_item(scope, item, &over);
                let kept = self.derive(predicate, &under(".predicate"), &inner, place);
                // The items it keeps are items of the collection.
                let mut carried = check.items(check.items_of(&over.carried));
                carried.gaps = join_gaps([over.carried.gaps.clone(), kept.carried.gaps.clone()]);
                Out {
                    carried,
                    size: over.size.saturating_add(kept.size).saturating_add(1),
                    origin: None,
                }
            }
            Expr::Difference {
                left,
                right,
                identity,
                max_items: _,
            } => {
                let sides = [
                    ("left", self.derive(left, &under(".left"), scope, place)),
                    ("right", self.derive(right, &under(".right"), scope, place)),
                ];
                // The identity is read in every item of both sides, and an
                // item without it stops the run: no `exists` can test it.
                for (side, out) in &sides {
                    let mut found = Build::default();
                    check.follow(&check.items_of(&out.carried), identity, 0, &mut found);
                    if !found.lacks.is_empty() {
                        let read = Rc::new(Read {
                            at: under(".identity"),
                            written: dotted(side, identity),
                            note: Some(format!(
                                "`{side}` is an item of the {side} collection, and `difference` \
                                 needs the identity of every item"
                            )),
                            identity: true,
                        });
                        for reason in found.lacks {
                            self.refuse(&read, &reason, false, None);
                        }
                    }
                }
                let [(_, kept), (_, removed)] = sides;
                // The items it keeps are items of `left`.
                let mut carried = check.items(check.items_of(&kept.carried));
                carried.gaps = join_gaps([kept.carried.gaps.clone(), removed.carried.gaps.clone()]);
                Out {
                    carried,
                    size: kept.size.saturating_add(removed.size).saturating_add(1),
                    origin: None,
                }
            }
            Expr::Lookup {
                key,
                cases,
                default,
            } => {
                // A Missing key takes the default without a trace, so the
                // key is consumed here, inside `exists` or not.
                let keyed = self.derive(key, &under(".key"), scope, place);
                self.sink(&keyed.carried.gaps, true);
                let mut size = keyed.size.saturating_add(1);
                let mut result = Build::default();
                let branches = cases
                    .iter()
                    .enumerate()
                    .map(|(i, c)| (format!(".cases[{i}].then"), &c.then, value_nodes(&c.when)))
                    .chain([(".default".to_string(), &**default, 0)]);
                // It is whichever branch is chosen.
                for (field, branch, when) in branches {
                    let out = self.derive(branch, &under(&field), scope, place);
                    size = size.saturating_add(when).saturating_add(out.size);
                    result.all(&out.carried);
                }
                Out {
                    carried: check.finish(result),
                    size,
                    origin: None,
                }
            }
            Expr::Let { bindings, body } => {
                let mut inner = scope.clone();
                let mut size = 1u64;
                for (i, b) in bindings.iter().enumerate() {
                    let at = under(&format!(".bindings[{i}].value"));
                    let value = self.derive(&b.value, &at, &inner, place);
                    size = size.saturating_add(value.size);
                    // A Missing value is bound as it is: it is refused where
                    // the name is consumed, and `exists` may test the name.
                    let note = match &value.origin {
                        Some(origin) => format!("is `{origin}`"),
                        None => "is computed from other values".to_string(),
                    };
                    inner.insert(
                        &b.name,
                        Named {
                            carried: value.carried,
                            origin: value.origin,
                            note: note.into(),
                        },
                    );
                }
                let out = self.derive(body, &under(".body"), &inner, place);
                Out {
                    carried: out.carried,
                    size: size.saturating_add(out.size),
                    origin: out.origin,
                }
            }
        }
    }
    /// `scope` with the item of a `map` or `filter` over `collection`.
    fn with_item(&self, scope: &Scope<'a>, item: &'a str, collection: &Out<'a>) -> Scope<'a> {
        let note = match &collection.origin {
            Some(origin) => format!("is an item of `{origin}`"),
            None => "is an item of its collection".to_string(),
        };
        let mut inner = scope.clone();
        // The item shadows any binding of the same name.
        inner.insert(
            item,
            Named {
                carried: self.check.items_of(&collection.carried),
                origin: collection.origin.as_ref().map(|o| format!("{o}[]").into()),
                note: note.into(),
            },
        );
        inner
    }
    /// What the reserved root `root` holds at `place`.
    fn reserved(&self, root: Reserved, place: Place<'a, '_>) -> Carried<'a> {
        let check = self.check;
        let elsewhere =
            |only: &str| check.lacking(&format!("`{}` is there only in {only}", root.name()));
        match (root, place) {
            (
                Reserved::Input,
                Place::Step {
                    entry_name, entry, ..
                },
            ) => check.shape(
                &entry.input,
                &format!("the input of entry `{entry_name}`"),
                root.name(),
            ),
            (Reserved::Input, Place::Trigger { .. }) => elsewhere("a step"),
            (Reserved::Event, Place::Trigger { payload }) => match payload {
                Some(schema) => check.shape(schema, "the trigger's event", root.name()),
                None => check.opaque(1),
            },
            (Reserved::Event, Place::Step { .. }) => elsewhere("a trigger's `input`"),
            (Reserved::Definitions, Place::Step { .. } | Place::Trigger { .. }) => {
                match &check.sources.definitions {
                    Some(schema) => check.shape(schema, "the captured definitions", root.name()),
                    None => check.opaque(1),
                }
            }
            // Every binding and item, each as it is at this step.
            (Reserved::Vars, Place::Step { context, .. }) => check.built(
                context
                    .iter()
                    .map(|(name, carried)| ((*name).to_string(), (**carried).clone()))
                    .collect(),
                false,
            ),
            (Reserved::Vars, Place::Trigger { .. }) => elsewhere("a step"),
        }
    }
    /// A reference: what `root.path` reads.
    fn read(
        &mut self,
        root: &'a str,
        path: &'a [String],
        at: &str,
        scope: &Scope<'a>,
        place: Place<'a, '_>,
    ) -> Out<'a> {
        let check = self.check;
        let written = dotted(root, path);
        let mut found = Build::default();
        // A lexical name first: it shadows; then a reserved root; then a
        // binding or item of the step's context.
        let lexical = scope.get(root);
        let (source, note, origin): (Option<Carried>, Option<String>, Option<Rc<str>>) =
            match lexical {
                Some(named) => (
                    Some(named.carried.clone()),
                    Some(format!("`{root}` {}", named.note)),
                    named.origin.as_ref().map(|o| dotted(o, path).into()),
                ),
                None => {
                    let (source, shown) = match (Reserved::named(root), place) {
                        (Some(reserved), _) => (Some(self.reserved(reserved, place)), None),
                        (None, Place::Step { context, .. }) => (
                            context.get(root).map(|c| (**c).clone()),
                            check.item_notes.get(root),
                        ),
                        (None, Place::Trigger { .. }) => (None, None),
                    };
                    let origin = match (root, path.split_first()) {
                        ("vars", Some((binding, rest))) => dotted(binding, rest),
                        _ => written.clone(),
                    };
                    (
                        source,
                        shown.map(|n| format!("`{root}` {n}")),
                        Some(origin.into()),
                    )
                }
            };
        // Following a value that can have many shapes costs one step for
        // each: that work is counted, and past a bound the draft is refused
        // without doing it.
        let shapes = source.as_ref().map_or(1, |s| s.forms.len()) as u64;
        if shapes > MANY_SHAPES {
            check.work.set(check.work.get().saturating_add(shapes));
        }
        if check.work.get() > MAX_SHAPE_WORK {
            self.charge(root, shapes);
            return Out {
                carried: check.scalar.clone(),
                size: shapes,
                origin: None,
            };
        }
        match &source {
            Some(source) => check.follow(source, path, 0, &mut found),
            // No such root: ordinary typing says so.
            None => found.form(Form::Opaque(1)),
        }
        let mut carried = check.finish(found);
        if lexical.is_some()
            && let Some(gaps) = carried.gaps.take()
        {
            carried.gaps = Some(Rc::new(GapNode::Via(root.into(), gaps)));
        }
        // What is not there on some way is Missing here, read by this
        // reference: from now on it travels with the value.
        if !carried.lacks.is_empty() {
            let read = Rc::new(Read {
                at: at.to_string(),
                written: written.clone(),
                note,
                identity: false,
            });
            let leaves = std::mem::take(&mut carried.lacks)
                .into_iter()
                .map(|reason| {
                    Some(Rc::new(GapNode::Leaf {
                        reason,
                        read: read.clone(),
                    }))
                })
                .collect::<Vec<_>>();
            carried.gaps = join_gaps(std::iter::once(carried.gaps.take()).chain(leaves));
        }
        // Every reference counts the type it reads (and no less than the
        // shapes it had to follow): what a name stands for is counted once,
        // where it is written.
        let size = carried.nodes.max(shapes).max(1);
        self.charge(root, size);
        Out {
            carried,
            size,
            origin,
        }
    }
    fn charge(&mut self, root: &str, size: u64) {
        let charge = self.charges.entry(root.to_string()).or_default();
        charge.each = charge.each.max(size);
        charge.times += 1;
        charge.total = charge.total.saturating_add(size);
    }
    /// The value with these gaps is needed here: every read behind them is
    /// refused. `key`: needed as a lookup key.
    fn sink(&mut self, gaps: &Gaps, key: bool) {
        let Some(top) = gaps else {
            return;
        };
        let mut pending: Vec<(Rc<GapNode>, Option<Rc<str>>)> = vec![(top.clone(), None)];
        while let Some((node, through)) = pending.pop() {
            if !self.sunk.insert((Rc::as_ptr(&node), key)) {
                continue;
            }
            self.kept.push(node.clone());
            match &*node {
                GapNode::Leaf { reason, read } => {
                    self.refuse(read, reason, key, through);
                }
                GapNode::All(parts) => {
                    pending.extend(parts.iter().map(|p| (p.clone(), through.clone())));
                }
                GapNode::Via(name, inner) => pending.push((inner.clone(), Some(name.clone()))),
            }
        }
    }
    fn refuse(&mut self, read: &Rc<Read>, reason: &Rc<str>, key: bool, through: Option<Rc<str>>) {
        let index = *self.by_read.entry(Rc::as_ptr(read)).or_insert_with(|| {
            self.refusals.push(Refusal {
                read: read.clone(),
                reasons: vec![],
                more: 0,
                key: false,
                through: vec![],
            });
            self.refusals.len() - 1
        });
        let refusal = &mut self.refusals[index];
        if refusal.reasons.contains(reason) {
        } else if refusal.reasons.len() >= MAX_REASONS {
            refusal.more += 1;
        } else {
            refusal.reasons.push(reason.clone());
        }
        refusal.key |= key;
        if let Some(name) = through
            && !refusal.through.contains(&name)
        {
            refusal.through.push(name);
        }
    }
    /// One expression a step (or trigger) evaluates and needs the value of:
    /// its refused reads as (location, message), and its size.
    fn evaluate(
        &mut self,
        path: &str,
        at: &str,
        expr: &'a Expr,
        place: Place<'a, '_>,
    ) -> (Vec<(String, String)>, ExprSize) {
        self.charges.clear();
        let out = self.derive(expr, at, &Scope::new(), place);
        self.sink(&out.carried.gaps, false);
        let reads = std::mem::take(&mut self.refusals)
            .into_iter()
            .map(|refusal| (refusal.read.at.clone(), refusal.message()))
            .collect();
        self.by_read.clear();
        self.sunk.clear();
        self.kept.clear();
        let most = self
            .charges
            .iter()
            .max_by_key(|(_, c)| c.total)
            .map(|(name, c)| (name.clone(), c.clone()));
        let sized = ExprSize {
            path: path.to_string(),
            at: at.to_string(),
            size: out.size,
            most,
        };
        (reads, sized)
    }
}
impl Refusal {
    fn message(&self) -> String {
        let read = &self.read;
        let mut notes: Vec<String> = read.note.iter().cloned().collect();
        if !self.through.is_empty() {
            let names: Vec<_> = self.through.iter().map(|n| format!("`{n}`")).collect();
            notes.push(format!(
                "its value is used through {}, outside any `exists`",
                names.join(", ")
            ));
        }
        let note = if notes.is_empty() {
            String::new()
        } else {
            format!(" ({})", notes.join("; "))
        };
        let mut reasons: Vec<String> = self.reasons.iter().map(|r| r.to_string()).collect();
        if self.more > 0 {
            reasons.push(format!("and {} more ways", self.more));
        }
        let remedy = if read.identity {
            "Build both collections only from values that always have it: an item without its \
             identity stops the run, and `exists` cannot test it"
        } else {
            "Read it only where it is always present: after the outcome edge of the step that \
             returns it, or on the `completed` edge of a join whose `accepted_outcomes` lists \
             only outcomes that have it (a `decide` on `outcome` does not narrow it). `exists` \
             only tells you whether it is there: testing it does not make a read of it allowed, \
             in the same expression or in a later step. So branch on the edge first and read \
             the value in the step that edge leads to; use `exists` on its own, as a test \
             that routes to a step that does not read the value"
        };
        let mut message = format!(
            "`{}`{note} is not there on every way to this step: {}. {remedy}",
            read.written,
            reasons.join("; ")
        );
        if self.key {
            message.push_str(
                ". A lookup whose key is missing takes its default without a trace, so a \
                 lookup key is held to this too",
            );
        }
        message
    }
}

// ---------------------------------------------------------------------------
// The flow over the steps, and the whole draft.
// ---------------------------------------------------------------------------

/// Per step inside a `for_each` body, where a failure there leads: the target,
/// the items that are unbound there, and the name of the way.
type LoopFailures<'a> = BTreeMap<&'a str, Vec<(&'a str, Vec<&'a str>, String)>>;
/// What the derivation says of the whole draft.
#[derive(Default)]
struct Derived {
    /// Refused reads: (step or trigger path, location, message).
    reads: Vec<(String, String, String)>,
    sizes: Vec<ExprSize>,
    /// Per entry: the steps it reaches and the type nodes in scope at each.
    scope: Vec<(String, u64, u64)>,
    /// The expanded size of every expression, once for each entry that
    /// reaches its step: ordinary typing types it once for each.
    total: u64,
    /// Faults that stop the check itself.
    limits: Vec<ValidationIssue>,
}

fn merge<'a>(check: &Check<'a>, into: &mut Context<'a>, from: &Context<'a>) -> bool {
    let mut changed = false;
    for (name, carried) in from {
        match into.get_mut(name) {
            None => {
                into.insert(name, carried.clone());
                changed = true;
            }
            Some(existing) => {
                if Rc::ptr_eq(existing, carried) {
                    continue;
                }
                let mut both = Build::default();
                both.all(existing);
                both.all(carried);
                if both.forms.len() > existing.forms.len()
                    || both.lacks.len() > existing.lacks.len()
                {
                    *existing = Rc::new(check.finish(both));
                    changed = true;
                }
            }
        }
    }
    changed
}

impl<'a> Check<'a> {
    fn new(content: &'a ArtifactContent, sources: &'a Sources) -> Self {
        let item_notes = content
            .playbook
            .steps
            .iter()
            .filter_map(|(name, step)| match &step.op {
                Operation::ForEach { item, .. } => Some((
                    item.as_str(),
                    format!("is an item of the collection of for_each `{name}`").into(),
                )),
                Operation::Invoke { .. }
                | Operation::Decide { .. }
                | Operation::Wait { .. }
                | Operation::Call { .. }
                | Operation::Join { .. }
                | Operation::Return { .. } => None,
            })
            .collect();
        Self {
            content,
            sources,
            schema_nodes: RefCell::new(HashMap::new()),
            built: RefCell::new(HashMap::new()),
            literals: RefCell::new(HashMap::new()),
            work: Cell::new(0),
            scalar: Carried {
                lacks: vec![],
                gaps: None,
                forms: vec![Form::Opaque(1)].into(),
                nodes: 1,
                depth: 0,
            },
            item_notes,
        }
    }
    /// A `for_each`'s own item where the loop is entered: its collection is
    /// evaluated then, before any item is bound.
    fn entering(&self, site: &str, item: &str) -> Carried<'a> {
        self.lacking(&format!(
            "where for_each `{site}` is entered, `{item}` is not bound yet"
        ))
    }
    /// For each step inside a `for_each` body, where a failure there leads:
    /// the runner hands it to the innermost loop, not to the step's own
    /// `on_error`. `(target, item unbound there, way)`.
    fn loop_failures(&self, parts: &BTreeMap<&'a str, Parts<'a>>) -> LoopFailures<'a> {
        let mut failures: BTreeMap<&str, Vec<_>> = BTreeMap::new();
        for (site, step) in &self.content.playbook.steps {
            let Operation::ForEach {
                item,
                body,
                on_failure,
                ..
            } = &step.op
            else {
                continue;
            };
            let target = match on_failure {
                // The next item, or the end of the loop.
                IterationFailure::Continue => Some((site.as_str(), None)),
                IterationFailure::Stop => {
                    step.on_error.as_deref().map(|to| (to, Some(item.as_str())))
                }
            };
            let Some((to, own)) = target else {
                continue;
            };
            let mut inside = BTreeSet::new();
            let mut pending = vec![body.as_str()];
            while let Some(name) = pending.pop() {
                if name == site || !inside.insert(name) {
                    continue;
                }
                if let Some(p) = parts.get(name) {
                    pending.extend(p.exits.iter().map(|e| e.to));
                }
            }
            // The failure is this loop's when no loop inside it is running,
            // so the items of those loops are not bound either.
            let unbinds: Vec<&str> = inside
                .iter()
                .filter_map(|name| parts.get(name)?.own_item)
                .chain(own)
                .collect();
            for name in inside {
                failures.entry(name).or_default().push((
                    to,
                    unbinds.clone(),
                    format!("a failure in the body of for_each `{site}`"),
                ));
            }
        }
        failures
    }
    /// What every binding and item is at every step reachable from `entry`.
    /// Each is in every context: not bound on a way that never bound it.
    fn contexts(
        &self,
        entry_name: &'a str,
        entry: &'a EntryPoint,
        parts: &BTreeMap<&'a str, Parts<'a>>,
        failures: &LoopFailures<'a>,
        limits: &mut Vec<ValidationIssue>,
    ) -> BTreeMap<&'a str, Context<'a>> {
        let mut start = Context::new();
        for (name, p) in parts {
            for exit in &p.exits {
                if let Some((bind, _)) = &exit.holds {
                    let way = format!(
                        "the way from the start of entry `{entry_name}` that does not pass step `{name}`"
                    );
                    start.insert(
                        bind,
                        Rc::new(self.lacking(&format!("on {way}, `{bind}` is not bound"))),
                    );
                }
            }
        }
        let mut contexts = BTreeMap::from([(entry.start.as_str(), start)]);
        let mut pending = VecDeque::from([entry.start.as_str()]);
        let mut queued = BTreeSet::from([entry.start.as_str()]);
        let mut visits = 0usize;
        let budget = MAX_FLOW_VISITS_PER_STEP * parts.len().max(1);
        let mut frozen = BTreeSet::new();
        while let Some(name) = pending.pop_front() {
            queued.remove(name);
            let Some(p) = parts.get(name) else {
                continue;
            };
            visits += 1;
            if visits > budget {
                issue(
                    limits,
                    "draft_limit",
                    "playbook",
                    format!(
                        "the steps of entry `{entry_name}` reach each other in too many ways to \
                         check (more than {budget} visits for {} steps). Shorten the draft: fewer \
                         steps that lead back to earlier ones.",
                        parts.len()
                    ),
                );
                break;
            }
            let mut current = contexts[name].clone();
            if let Some(item) = p.own_item {
                current.insert(item, Rc::new(self.entering(name, item)));
            }
            let mut targets = vec![];
            for exit in &p.exits {
                let mut next = current.clone();
                if let Some((bind, held)) = &exit.holds {
                    let mut item = |collection: &'a Expr| {
                        if frozen.contains(name) {
                            return self.opaque(1);
                        }
                        let mut pass = Pass::new(self);
                        let place = Place::Step {
                            entry_name,
                            entry,
                            context: &current,
                        };
                        let out = pass.derive(collection, "collection", &Scope::new(), place);
                        let item = self.items_of(&out.carried);
                        if item.depth > MAX_ITEM_DEPTH {
                            frozen.insert(name);
                            issue(
                                limits,
                                "draft_limit",
                                &format!("steps.{name}"),
                                format!(
                                    "steps.{name}.collection: the items nest more than \
                                     {MAX_ITEM_DEPTH} levels deep. A `for_each` collection may \
                                     not be built from the items of a loop that is itself built \
                                     from this one's."
                                ),
                            );
                            return self.opaque(1);
                        }
                        item
                    };
                    next.insert(bind, Rc::new(self.held(bind, &exit.way, held, &mut item)));
                }
                targets.push((exit.to, next));
            }
            for (to, unbinds, way) in failures.get(name).into_iter().flatten() {
                let mut next = current.clone();
                for item in unbinds {
                    next.insert(
                        item,
                        Rc::new(self.lacking(&format!("on {way}, `{item}` is not bound"))),
                    );
                }
                targets.push((to, next));
            }
            for (target, next) in targets {
                let changed = match contexts.get_mut(target) {
                    Some(existing) => merge(self, existing, &next),
                    None => {
                        contexts.insert(target, next);
                        true
                    }
                };
                if changed && queued.insert(target) {
                    pending.push_back(target);
                }
            }
        }
        contexts
    }
    /// The derivation over the whole draft.
    fn derived(&self) -> Derived {
        let playbook = &self.content.playbook;
        let parts: BTreeMap<&str, Parts> = playbook
            .steps
            .iter()
            .map(|(name, step)| (name.as_str(), self.content.parts(self.sources, name, step)))
            .collect();
        let failures = self.loop_failures(&parts);
        let mut derived = Derived::default();
        let mut sizes: BTreeMap<(String, String), ExprSize> = BTreeMap::new();
        let mut pass = Pass::new(self);
        for (entry_name, entry) in &playbook.entries {
            let contexts = self.contexts(entry_name, entry, &parts, &failures, &mut derived.limits);
            // Ordinary typing holds, at every step, the type of `input`, of
            // the definitions and of every binding and item, in every form
            // it has anywhere.
            let mut anywhere: BTreeMap<&str, Build> = BTreeMap::new();
            for context in contexts.values() {
                for (name, carried) in context {
                    anywhere.entry(name).or_default().all(carried);
                }
            }
            let fixed = self.nodes(&entry.input).saturating_add(
                self.sources
                    .definitions
                    .as_ref()
                    .map_or(0, |d| self.nodes(d)),
            );
            let in_scope = anywhere
                .into_values()
                .fold(fixed, |n, forms| n.saturating_add(self.finish(forms).nodes));
            let reached = contexts
                .keys()
                .filter(|name| parts.contains_key(*name))
                .count();
            derived
                .scope
                .push((entry_name.clone(), reached as u64, in_scope));
            for (name, context) in &contexts {
                let Some(p) = parts.get(name) else {
                    continue;
                };
                let path = format!("steps.{name}");
                let mut entered;
                let context = match p.own_item {
                    Some(item) => {
                        entered = context.clone();
                        entered.insert(item, Rc::new(self.entering(name, item)));
                        &entered
                    }
                    None => context,
                };
                let place = Place::Step {
                    entry_name,
                    entry,
                    context,
                };
                for (at, expr) in &p.exprs {
                    let (reads, sized) = pass.evaluate(&path, at, expr, place);
                    derived
                        .reads
                        .extend(reads.into_iter().map(|(at, m)| (path.clone(), at, m)));
                    derived.total = derived.total.saturating_add(sized.size);
                    let key = (path.clone(), at.clone());
                    if sizes.get(&key).is_none_or(|old| old.size < sized.size) {
                        sizes.insert(key, sized);
                    }
                }
            }
        }
        for t in &playbook.triggers {
            let path = format!("triggers.{}", t.id);
            let payload = self
                .content
                .registry
                .events
                .get(&t.event.name)
                .map(|e| &e.payload);
            let (reads, sized) =
                pass.evaluate(&path, "input", &t.input, Place::Trigger { payload });
            derived
                .reads
                .extend(reads.into_iter().map(|(at, m)| (path.clone(), at, m)));
            derived.total = derived.total.saturating_add(sized.size);
            sizes.insert((path, "input".into()), sized);
        }
        derived.sizes = sizes.into_values().collect();
        if self.work.get() > MAX_SHAPE_WORK {
            issue(
                &mut derived.limits,
                "draft_limit",
                "playbook",
                format!(
                    "the draft reads values that can have many different shapes too often to \
                     check: more than {MAX_SHAPE_WORK} shapes to follow in all. A literal \
                     table counts one shape for each row, whatever its fields, and a lookup one \
                     for each result; each read through a name or an item follows all of them. \
                     Read the table in fewer places (bind what you need from it once with \
                     `let`), or use a smaller table."
                ),
            );
        }
        derived
    }
}

fn count(size: u64) -> String {
    if size == u64::MAX {
        "too many".into()
    } else {
        size.to_string()
    }
}
impl ExprSize {
    /// "`R` stands for 4100 nodes and is read 40 times".
    fn most(&self) -> String {
        let Some((name, charge)) = &self.most else {
            return String::new();
        };
        format!(
            " Most of it is `{name}` (counted at the size of the type it reads): up to {} nodes, \
             read {} time{}, {} in all.",
            count(charge.each),
            charge.times,
            if charge.times == 1 { "" } else { "s" },
            count(charge.total)
        )
    }
}

/// How much of the cost bounds a draft uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DraftSize {
    /// The largest expression's count, against `MAX_DRAFT_EXPANSION`.
    pub largest_expression: u64,
    /// Where that expression is, as a diagnostic names it.
    pub largest_at: String,
    /// All expressions together, against `MAX_DRAFT_EXPANSION_TOTAL`.
    pub expressions: u64,
    /// Type nodes in scope over all steps, against `MAX_DRAFT_SCOPE`.
    pub scope: u64,
}
fn scope_total(derived: &Derived) -> u64 {
    derived.scope.iter().fold(0u64, |n, (_, steps, each)| {
        n.saturating_add(steps.saturating_mul(*each))
    })
}

impl ArtifactContent {
    /// What the draft counts against each cost bound.
    pub fn draft_size(&self) -> DraftSize {
        let sources = Sources::new(self);
        let derived = Check::new(self, &sources).derived();
        let largest = derived.sizes.iter().max_by_key(|s| s.size);
        DraftSize {
            largest_expression: largest.map_or(0, |s| s.size),
            largest_at: largest
                .map(|s| format!("{}.{}", s.path, s.at))
                .unwrap_or_default(),
            expressions: derived.total,
            scope: scope_total(&derived),
        }
    }
    /// Bounds a new draft is held to before anything costly runs on it: the
    /// expanded size of each expression and of all together, and the types
    /// in scope over all steps. Typing and the read check follow every copy
    /// of a value, so such a draft is refused before either runs.
    pub fn draft_expansion_issues(&self) -> Vec<ValidationIssue> {
        let sources = Sources::new(self);
        let derived = Check::new(self, &sources).derived();
        let mut issues = derived.limits.clone();
        if !issues.is_empty() {
            return issues;
        }
        const COUNTED: &str = "with every reference counted at the size of the type it reads";
        let total = derived.total;
        for sized in &derived.sizes {
            if sized.size > MAX_DRAFT_EXPANSION {
                issue(
                    &mut issues,
                    "expression_limit",
                    &sized.path,
                    format!(
                        "{}.{}: expands to {} nodes {COUNTED}; an expression may expand to at \
                         most {MAX_DRAFT_EXPANSION}.{} Build each value from a name once, not \
                         twice at every level, and read the one field you need rather than a \
                         whole binding or table.",
                        sized.path,
                        sized.at,
                        count(sized.size),
                        sized.most()
                    ),
                );
            }
        }
        if issues.is_empty() && total > MAX_DRAFT_EXPANSION_TOTAL {
            let largest = derived.sizes.iter().max_by_key(|s| s.size);
            let largest = largest
                .map(|s| {
                    format!(
                        " The largest is {}.{}, {} nodes.{}",
                        s.path,
                        s.at,
                        count(s.size),
                        s.most()
                    )
                })
                .unwrap_or_default();
            issue(
                &mut issues,
                "expression_limit",
                "playbook",
                format!(
                    "the draft's expressions expand to {} nodes in all {COUNTED} (an expression \
                     counts once for each entry that reaches its step); a draft may expand to at \
                     most {MAX_DRAFT_EXPANSION_TOTAL}.{largest} Build each value from a name \
                     once, or shorten the draft.",
                    count(total)
                ),
            );
        }
        let in_scope = scope_total(&derived);
        if issues.is_empty() && in_scope > MAX_DRAFT_SCOPE {
            let entries: Vec<String> = derived
                .scope
                .iter()
                .map(|(entry, steps, each)| {
                    format!("entry `{entry}` reaches {steps} steps with {each} at each")
                })
                .take(8)
                .collect();
            issue(
                &mut issues,
                "expression_limit",
                "playbook",
                format!(
                    "the draft's steps have {} type nodes in scope in all (for each step an \
                     entry reaches: the type of the entry's `input`, the captured definitions, \
                     and the result type of every step that binds one); a draft may have at most \
                     {MAX_DRAFT_SCOPE}: {}. Use fewer steps, fewer steps that bind a result, a \
                     smaller `input` type, or fewer entries over the same steps.",
                    count(in_scope),
                    entries.join("; ")
                ),
            );
        }
        issues
    }
    /// Issues only a new draft is held to. `reported` are the issues
    /// `validate_structure` already found; a read it already refused is not
    /// reported again.
    pub fn draft_issues(&self, reported: &[ValidationIssue]) -> Vec<ValidationIssue> {
        let mut issues = vec![];
        for (name, step) in &self.playbook.steps {
            let Operation::Join {
                members,
                member_keys,
                ..
            } = &step.op
            else {
                continue;
            };
            for (i, member) in members.iter().enumerate() {
                if let Some(error) = self.handle_member_path(member) {
                    let key = member_keys
                        .get(i)
                        .map(|k| format!(" (key {k})"))
                        .unwrap_or_default();
                    issue(
                        &mut issues,
                        "join_member_type",
                        &format!("steps.{name}"),
                        format!("members[{i}]{key}: {error}"),
                    );
                }
            }
        }
        let sources = Sources::new(self);
        let derived = Check::new(self, &sources).derived();
        issues.extend(derived.limits);
        for (path, place, message) in derived.reads {
            let refused = reported
                .iter()
                .chain(&issues)
                .any(|i| i.path == path && i.message.starts_with(&format!("{path}.{place}:")));
            if !refused {
                issue(
                    &mut issues,
                    "expression_type",
                    &path,
                    format!("{path}.{place}: {message}"),
                );
            }
        }
        issues
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_reserved_root_is_decided() {
        let named: Vec<_> = Reserved::ALL.iter().map(|r| r.name()).collect();
        assert_eq!(named, RESERVED_ROOTS);
        for root in RESERVED_ROOTS {
            assert!(Reserved::named(root).is_some(), "{root}");
        }
    }
}
