//! A generated test of the cost bound: whatever a draft under the bounds
//! holds, checking it, and verifying what it seals into, stay within a fixed
//! budget of time and memory.
//!
//! Seeded, so every run checks the same drafts. Each draft is built to stress
//! one of the ways cost grows (a large type shared through many references,
//! names that double, large tables, many steps with much in scope, loops) at
//! a random size around the bounds: some fit and seal, the rest are refused.
use aq_playbooks_compiler::draft::*;
use aq_playbooks_model::*;
use serde_json::{Map, Value, json};
use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

/// Counts the bytes in use, to measure the peak of one call.
struct Counting;
static IN_USE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let now = IN_USE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
        PEAK.fetch_max(now, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        IN_USE.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: Counting = Counting;
/// One measurement at a time: the counts are the whole process's.
static MEASURING: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// What one call cost.
#[derive(Clone, Copy, Default, Debug)]
struct Cost {
    time: Duration,
    /// Peak bytes in use above what was in use when it started.
    bytes: usize,
}
impl Cost {
    fn max(self, other: Cost) -> Cost {
        Cost {
            time: self.time.max(other.time),
            bytes: self.bytes.max(other.bytes),
        }
    }
}
fn measured<T>(f: impl FnOnce() -> T) -> (T, Cost) {
    let before = IN_USE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let start = Instant::now();
    let result = f();
    let time = start.elapsed();
    let bytes = PEAK.load(Ordering::Relaxed).saturating_sub(before);
    (result, Cost { time, bytes })
}

/// The budget, in a release build: checking a draft under the bounds, and
/// verifying the artifact it seals into (as every load does).
const CHECK_BUDGET: Cost = Cost {
    time: Duration::from_millis(2_000),
    bytes: 256 << 20,
};
const VERIFY_BUDGET: Cost = Cost {
    time: Duration::from_millis(1_500),
    bytes: 256 << 20,
};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// A number in `low..=high`.
    fn within(&mut self, low: usize, high: usize) -> usize {
        low + (self.next() % (high - low + 1) as u64) as usize
    }
}

fn strings(items: &[&str]) -> BTreeSet<String> {
    items.iter().map(|s| (*s).to_string()).collect()
}
/// An object schema of `outer` objects of `inner` scalars: `outer * (inner +
/// 1) + 1` nodes.
fn wide(outer: usize, inner: usize) -> Value {
    let mut properties = Map::new();
    properties.insert("task".into(), json!({"type": "string", "max_length": 64}));
    for i in 0..outer {
        let mut q = Map::new();
        for j in 0..inner {
            q.insert(format!("q{j}"), json!({"type": "null"}));
        }
        properties.insert(
            format!("p{i}"),
            json!({"type": "object", "properties": q, "required": []}),
        );
    }
    json!({"type": "object", "properties": properties, "required": ["task"]})
}
/// `work.read`, whose `read` outcome is an object of `result` objects of
/// eight scalars, and `missing` is empty.
fn catalog(result: usize) -> Registry {
    let read = CommandContract {
        name: "work.read".into(),
        version: "1.0.0".into(),
        input: Schema::object(BTreeMap::new()),
        outcomes: BTreeMap::from([
            (
                "read".into(),
                serde_json::from_value(wide(result, 8)).unwrap(),
            ),
            ("missing".into(), Schema::object(BTreeMap::new())),
        ]),
        capabilities: strings(&["work.read"]),
        scopes: strings(&["project"]),
        effect: Effect::Observation,
        recovery: Recovery::Observation,
        asynchronous: None,
        cancellation: false,
        public_result_fields: BTreeSet::new(),
        sensitive_input_fields: BTreeSet::new(),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 65_536,
        deadline_ms: 1000,
    };
    Registry {
        commands: BTreeMap::from([(read.name.clone(), read)]),
        events: BTreeMap::new(),
    }
}
fn reference(root: &str) -> Value {
    json!({"op": "ref", "root": root, "path": []})
}
fn decide(value: Value, next: &str) -> Value {
    json!({"op": {"kind": "decide", "predicate": {"op": "exists", "value": value}},
           "edges": {"true": next, "false": next, "unknown": next}, "on_error": null})
}
fn read(bind: &str, next: &str) -> Value {
    json!({"op": {"kind": "invoke", "command": "work.read", "bind": bind, "retry": null,
                  "input": {"op": "object", "fields": {}}},
           "edges": {"read": next, "missing": next}, "on_error": "failed"})
}
fn ret(outcome: &str) -> Value {
    json!({"op": {"kind": "return", "outcome": outcome, "output": {"op": "literal", "value": null}},
           "edges": {}, "on_error": null})
}

/// One expression that grows in one of the ways cost grows, at a size drawn
/// around the bounds; `roots` are the roots in scope it may share.
fn hostile(rng: &mut Rng, roots: &[String]) -> Value {
    let root = roots[rng.within(0, roots.len() - 1)].clone();
    match rng.within(0, 6) {
        // An object of objects, each field a reference: every copy of the
        // root's type is kept in the type of the whole.
        0 => {
            let (g, h) = (rng.within(1, 44), rng.within(1, 44));
            let mut outer = Map::new();
            for i in 0..g {
                let mut inner = Map::new();
                for j in 0..h {
                    inner.insert(format!("g{j}"), reference(&root));
                }
                outer.insert(format!("f{i}"), json!({"op": "object", "fields": inner}));
            }
            json!({"op": "object", "fields": outer})
        }
        // A flat array of references.
        1 => {
            let n = rng.within(1, 2000);
            json!({"op": "array", "items": vec![reference(&root); n]})
        }
        // Names that double at every level.
        2 => {
            let depth = rng.within(2, 17);
            let mut bindings = vec![json!({"name": "b0", "value": {"op": "object",
                "fields": {"k": reference(&root)}}})];
            for j in 1..depth {
                let p = format!("b{}", j - 1);
                bindings.push(json!({"name": format!("b{j}"), "value": {"op": "object",
                    "fields": {"a": reference(&p), "b": reference(&p)}}}));
            }
            json!({"op": "let", "bindings": bindings,
                   "body": reference(&format!("b{}", depth - 1))})
        }
        // A table whose every result is the root.
        3 => {
            let cases: Vec<Value> = (0..rng.within(1, 256))
                .map(|i| json!({"when": format!("k{i}"), "then": reference(&root)}))
                .collect();
            json!({"op": "lookup", "key": {"op": "ref", "root": "input", "path": ["task"]},
                   "cases": cases, "default": reference(&root)})
        }
        // A map whose body builds an object of references.
        4 => {
            let n = rng.within(1, 200);
            let mut fields = Map::new();
            for j in 0..n {
                fields.insert(format!("g{j}"), reference(&root));
            }
            json!({"op": "map", "item": "it", "max_items": 4,
                   "collection": {"op": "literal", "value": [1, 2, 3, 4]},
                   "value": {"op": "object", "fields": fields}})
        }
        // A large literal table bound once and read many times.
        5 => {
            let rows: Vec<Value> = (0..rng.within(1, 400))
                .map(|i| json!({"k": i, "v": "x"}))
                .collect();
            let reads = rng.within(1, 400);
            json!({"op": "let", "bindings": [{"name": "t", "value": {"op": "literal", "value": rows}}],
                   "body": {"op": "array", "items": vec![reference("t"); reads]}})
        }
        // Every binding at once, many times.
        _ => {
            let n = rng.within(1, 300);
            json!({"op": "array", "items": vec![reference("vars"); n]})
        }
    }
}

/// A draft: `steps` steps in a chain (reads that bind, decides on a hostile
/// expression, some inside loops), an entry `input` of `outer * inner` nodes.
fn draft(rng: &mut Rng) -> Workspace {
    let steps = rng.within(4, MAX_DRAFT_STEPS - 2);
    let (outer, inner) = match rng.within(0, 3) {
        0 => (rng.within(1, 70), rng.within(1, 70)),
        1 => (rng.within(1, 20), rng.within(1, 20)),
        _ => (1, 1),
    };
    // How many of the steps carry a hostile expression (few large ones, or
    // many, so that both the per-expression and the whole-draft bounds bind),
    // and how many bind a result (so that the scope bound binds too).
    let hostile_every = [4, 16, 64, 300][rng.within(0, 3)];
    let reads_in_ten = rng.within(0, 3);
    let mut roots = vec!["input".to_string()];
    let mut map = Map::new();
    let name = |i: usize| {
        if i == steps {
            "done".to_string()
        } else {
            format!("s{i}")
        }
    };
    let mut loops: Vec<(usize, String)> = vec![];
    let mut i = 0;
    while i < steps {
        // Where this step leads: the next, or back to the loop it ends.
        let ends_loop = loops.last().is_some_and(|(end, _)| *end == i);
        let next = if ends_loop {
            // The innermost loop that ends here; those around it that end
            // here too are led back to by it.
            let site = loops.pop().unwrap().1;
            while loops.last().is_some_and(|(end, _)| *end == i) {
                loops.pop();
            }
            site
        } else {
            name(i + 1)
        };
        let kind = rng.within(0, 9);
        let step = match kind {
            _ if kind < reads_in_ten => {
                let bind = format!("w{i}");
                let step = read(&bind, &next);
                roots.push(bind);
                step
            }
            9 if loops.len() < 3 && !ends_loop && i + 3 < steps => {
                // A loop over the next few steps, inside the loop around it;
                // its last step returns here, and it leads on to the step
                // after, or back to the loop around it when both end there.
                let around = loops.last().cloned();
                let limit = around.as_ref().map_or(steps - 1, |(end, _)| *end);
                let end = rng.within(i + 1, (i + 6).min(limit));
                let after = match &around {
                    Some((outer, site)) if *outer == end => site.clone(),
                    Some(_) | None => name(end + 1),
                };
                loops.push((end, name(i)));
                json!({"op": {"kind": "for_each", "collection": {"op": "literal", "value": [1, 2]},
                    "item": format!("t{i}"), "body": name(i + 1), "next": after, "max_items": 2,
                    "concurrency": 1, "on_failure": "stop"},
                    "edges": {}, "on_error": "failed"})
            }
            _ => {
                let value = if rng.within(1, hostile_every) == 1 {
                    hostile(rng, &roots)
                } else {
                    reference(&roots[rng.within(0, roots.len() - 1)])
                };
                decide(value, &next)
            }
        };
        map.insert(name(i), step);
        i += 1;
    }
    // `failed` is there when some step can fail into it.
    if serde_json::to_string(&map).unwrap().contains("\"failed\"") {
        map.insert("failed".into(), ret("failed"));
    }
    map.insert("done".into(), ret("done"));
    Workspace {
        markdown: "# Generated\n\nA line.\n".into(),
        record: SourceRecord {
            id: "generated".into(),
            binding: "aq.generated".into(),
            scope: "project".into(),
            purpose: "A generated draft".into(),
            commands: strings(&["work.read"]),
            events: BTreeSet::new(),
            capabilities: strings(&["work.read"]),
            facts_version: "facts-1".into(),
        },
        catalog: catalog(rng.within(1, 12)),
        facts: BTreeMap::new(),
        draft: json!({
            "purpose": "A generated draft",
            "entries": {"main": {
                "input": wide(outer, inner),
                "outcomes": {"done": {"type": "null"}, "failed": {"type": "null"}},
                "start": "s0"}},
            "steps": map,
            "triggers": [],
            "source_map": {}
        }),
    }
}

/// The reviewer's draft (A2-R3-I2): a 4,972-node `input` type shared through
/// 1,936 references in one expression, 198 KB, under every earlier bound.
fn shared_input() -> Workspace {
    let mut rng = Rng(1);
    let mut ws = draft(&mut rng);
    let mut outer = Map::new();
    for i in 0..44 {
        let mut inner = Map::new();
        for j in 0..44 {
            inner.insert(format!("g{j}"), reference("input"));
        }
        outer.insert(format!("f{i}"), json!({"op": "object", "fields": inner}));
    }
    ws.draft["entries"]["main"]["input"] = wide(70, 70);
    ws.draft["steps"] = json!({
        "s0": decide(json!({"op": "object", "fields": outer}), "done"),
        "done": ret("done"),
        "failed": ret("failed"),
    });
    ws
}

#[test]
fn a_large_type_shared_through_many_references_is_refused() {
    let _alone = MEASURING.lock().unwrap_or_else(|e| e.into_inner());
    let ws = shared_input();
    let bytes = serde_json::to_string(&ws.draft).unwrap().len();
    assert!(bytes < MAX_DRAFT_JSON_BYTES, "{bytes}");
    let (diagnostics, cost) = measured(|| check(&ws));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    let d = &diagnostics[0];
    assert_eq!(
        (d.code.as_str(), d.path.as_str()),
        ("expression_limit", "draft.json:steps.s0")
    );
    // The message names what is responsible and the numbers: the type of
    // `input` is 70 objects of 70 fields, the object around them and `task`.
    let input = 70 * 71 + 2;
    let references = 44 * 44;
    let all = input * references;
    for text in [
        // The references, the 45 objects that hold them, and `exists`.
        format!("steps.s0.predicate: expands to {} nodes", all + 46),
        "an expression may expand to at most 200000".to_string(),
        "Most of it is `input`".to_string(),
        format!("up to {input} nodes, read {references} times, {all} in all"),
    ] {
        assert!(d.message.contains(&text), "{text}: {}", d.message);
    }
    println!("the shared-input draft ({bytes} bytes) is refused in {cost:?}");
    if !cfg!(debug_assertions) {
        assert!(cost.time < Duration::from_millis(100), "{cost:?}");
    }
}

/// Drafts built to sit just under one bound each; every one seals.
fn at_the_bounds() -> Vec<(&'static str, Workspace)> {
    let chain = |input: Value, steps: Vec<Value>| {
        let mut ws = draft(&mut Rng(7));
        let mut map = Map::new();
        let last = steps.len() - 1;
        for (i, value) in steps.into_iter().enumerate() {
            let next = if i == last {
                "done".to_string()
            } else {
                format!("s{}", i + 1)
            };
            map.insert(format!("s{i}"), decide(value, &next));
        }
        map.insert("done".into(), ret("done"));
        ws.draft["entries"]["main"]["input"] = input;
        ws.draft["steps"] = Value::Object(map);
        ws
    };
    // `refs` references to `root`, kept apart in an object of objects.
    let kept = |root: &str, refs: usize| {
        let mut outer = Map::new();
        for i in 0..refs.div_ceil(40) {
            let mut inner = Map::new();
            for j in 0..40.min(refs - i * 40) {
                inner.insert(format!("g{j}"), reference(root));
            }
            outer.insert(format!("f{i}"), json!({"op": "object", "fields": inner}));
        }
        json!({"op": "object", "fields": outer})
    };
    let doubling = |depth: usize| {
        let mut bindings = vec![json!({"name": "b0", "value": {"op": "object",
            "fields": {"k": reference("input")}}})];
        for j in 1..depth {
            let p = format!("b{}", j - 1);
            bindings.push(json!({"name": format!("b{j}"), "value": {"op": "object",
                "fields": {"a": reference(&p), "b": reference(&p)}}}));
        }
        json!({"op": "let", "bindings": bindings, "body": reference(&format!("b{}", depth - 1))})
    };
    vec![
        (
            // 254 steps, each with a 961-node input in scope: 244 094.
            "the scope bound",
            chain(wide(137, 6), vec![reference("input"); 253]),
        ),
        (
            // Five expressions of 390 references to a 502-node input: about
            // 196 000 each, 980 000 in all.
            "the whole-draft bound",
            chain(wide(50, 9), vec![kept("input", 390); 5]),
        ),
        (
            // One expression of 40 references to a 4 972-node input.
            "the expression bound, a large type",
            chain(wide(70, 70), vec![kept("input", 40)]),
        ),
        (
            // Near the byte cap: two literal arrays of 190 000 numbers, each
            // just under the expression bound.
            "the byte cap, long literal arrays",
            chain(
                wide(1, 1),
                vec![json!({"op": "literal", "value": vec![1; 190_000]}); 2],
            ),
        ),
        (
            // Near the byte cap: literal tables of 23 500 rows, each row an
            // object, 70 501 nodes a table, read through a name.
            "the byte cap, literal tables of rows",
            chain(
                wide(1, 1),
                vec![
                    json!({"op": "let",
                        "bindings": [{"name": "t", "value": {"op": "literal",
                            "value": vec![json!({"k": 1, "v": "x"}); 23_500]}}],
                        "body": {"op": "ref", "root": "t", "path": ["0", "k"]}});
                    2
                ],
            ),
        ),
        (
            // Near the byte cap: an input type of 32 582 nodes, in scope at
            // three steps and read whole twice.
            "the byte cap, a large input type",
            chain(wide(180, 180), vec![reference("input"); 2]),
        ),
        (
            // Near the byte cap: two literal arrays of 54 500 one-field rows,
            // the most rows with a field that fit. Serializing the artifact
            // to verify it costs about 2 KB a row.
            "the byte cap, one-field rows",
            chain(
                wide(1, 1),
                vec![json!({"op": "literal", "value": vec![json!({"": 1}); 54_500]}); 2],
            ),
        ),
        (
            // Near the byte cap: two literal arrays of 127 000 empty rows.
            "the byte cap, empty rows",
            chain(
                wide(1, 1),
                vec![json!({"op": "literal", "value": vec![json!({}); 127_000]}); 2],
            ),
        ),
        (
            // Forty entries over the same 100 steps, each with a 52-node
            // input in scope: 208 000.
            "the scope bound, many entries",
            {
                let mut ws = chain(wide(5, 9), vec![reference("input"); 99]);
                let main = ws.draft["entries"]["main"].clone();
                for i in 1..40 {
                    ws.draft["entries"][format!("e{i}")] = main.clone();
                }
                ws
            },
        ),
        (
            // Names that double fourteen times: about 147 000 nodes, six times.
            "the expression bound, doubling names",
            chain(wide(1, 1), vec![doubling(14); 6]),
        ),
    ]
}

/// Literal rows that each have a different field, read through a `map` item
/// in many places: every read has to follow every row's shape.
fn many_shapes(rows: usize, reads: usize) -> Workspace {
    let table: Vec<Value> = (0..rows).map(|i| json!({format!("a{i}"): 1})).collect();
    let read = json!({"op": "exists", "value": {"op": "ref", "root": "r", "path": ["a0"]}});
    let mut ws = draft(&mut Rng(7));
    ws.draft["steps"] = json!({
        "s0": decide(json!({"op": "map", "item": "r", "max_items": rows,
            "collection": {"op": "literal", "value": table},
            "value": {"op": "array", "items": vec![read; reads]}}), "done"),
        "done": ret("done"),
    });
    ws
}

#[test]
fn reading_a_value_of_many_shapes_many_times_is_refused_before_the_work_is_done() {
    let _alone = MEASURING.lock().unwrap_or_else(|e| e.into_inner());
    // 30 000 shapes read 6 000 times would be 180 million steps.
    let ws = many_shapes(30_000, 6_000);
    let bytes = serde_json::to_string(&ws.draft).unwrap().len();
    assert!(bytes < MAX_DRAFT_JSON_BYTES, "{bytes}");
    let (diagnostics, cost) = measured(|| check(&ws));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
    assert_eq!(diagnostics[0].code, "draft_limit", "{diagnostics:#?}");
    assert!(
        diagnostics[0].message.contains("many different shapes"),
        "{diagnostics:#?}"
    );
    println!("the many-shapes draft ({bytes} bytes) is refused in {cost:?}");
    assert!(cost.bytes < CHECK_BUDGET.bytes, "{cost:?}");
    if !cfg!(debug_assertions) {
        assert!(cost.time < CHECK_BUDGET.time, "{cost:?}");
    }
    // A table of a thousand such rows read in a hundred places is checked
    // as usual (the reads here only test with `exists`, so it checks clean).
    let diagnostics = check(&many_shapes(1_000, 100));
    assert!(diagnostics.is_empty(), "{diagnostics:#?}");
}

#[test]
fn checking_and_verifying_any_draft_under_the_bounds_stays_within_budget() {
    let _alone = MEASURING.lock().unwrap_or_else(|e| e.into_inner());
    // Fewer in a debug build, where only the memory budget is held.
    let drafts = if cfg!(debug_assertions) { 12 } else { 400 };
    let mut rng = Rng(0x00C0_FFEE_D00D_F00D);
    let (mut sealed, mut refused, mut too_long) = (0, 0, 0);
    let (mut worst_check, mut worst_refusal, mut worst_verify) =
        (Cost::default(), Cost::default(), Cost::default());
    let mut largest = 0;
    let directed = at_the_bounds();
    let aimed = directed.len();
    let names: Vec<&str> = directed.iter().map(|(name, _)| *name).collect();
    let generated = (0..drafts).map(|_| draft(&mut rng));
    let all = directed.into_iter().map(|(_, ws)| ws).chain(generated);
    for (n, ws) in all.enumerate() {
        let bytes = serde_json::to_string(&ws.draft).unwrap().len();
        let (diagnostics, cost) = measured(|| check(&ws));
        // A draft aimed just under a bound seals.
        assert!(
            n >= aimed || diagnostics.is_empty(),
            "{n}: {diagnostics:#?}"
        );
        // Refused for its size alone, never because the check gave up.
        if let Some(d) = diagnostics.iter().find(|d| {
            !["expression_limit", "draft_limit"].contains(&d.code.as_str())
                || d.message.contains("too many ways to check")
                || d.message.contains("levels deep")
        }) {
            panic!("draft {n}: the generator built a draft refused for another reason: {d:#?}");
        }
        if !diagnostics.is_empty() {
            refused += 1;
            if bytes > MAX_DRAFT_JSON_BYTES {
                too_long += 1;
            }
            worst_refusal = worst_refusal.max(cost);
            assert!(
                cost.bytes < CHECK_BUDGET.bytes,
                "draft {n} ({bytes} bytes): refusing it took {cost:?}"
            );
            continue;
        }
        sealed += 1;
        largest = largest.max(bytes);
        worst_check = worst_check.max(cost);
        assert!(
            cost.bytes < CHECK_BUDGET.bytes,
            "draft {n} ({bytes} bytes): checking it took {cost:?}"
        );
        let artifact = seal(&ws).unwrap_or_else(|d| panic!("draft {n}: {d:#?}"));
        let stored = canonical_bytes(&artifact).unwrap();
        let (loaded, verify) = measured(|| Artifact::from_canonical_bytes(&stored));
        loaded.unwrap_or_else(|e| panic!("draft {n}: {e}"));
        worst_verify = worst_verify.max(verify);
        if let Some(name) = names.get(n) {
            println!("at {name} ({bytes} bytes): check {cost:?}, verify {verify:?}");
        }
        assert!(
            verify.bytes < VERIFY_BUDGET.bytes,
            "draft {n} ({bytes} bytes): verifying it took {verify:?}"
        );
        if !cfg!(debug_assertions) {
            assert!(
                cost.time < CHECK_BUDGET.time,
                "draft {n} ({bytes} bytes): checking it took {cost:?}"
            );
            assert!(
                verify.time < VERIFY_BUDGET.time,
                "draft {n} ({bytes} bytes): verifying it took {verify:?}"
            );
        }
    }
    println!(
        "cost: {aimed} drafts at the bounds and {drafts} generated: {sealed} sealed (the largest {largest} bytes), {refused} refused \
         ({too_long} for their length); worst check {worst_check:?}, worst refusal \
         {worst_refusal:?}, worst verify {worst_verify:?}"
    );
    // Both sides of the bounds are exercised.
    assert!(
        (sealed - aimed) * 4 >= drafts && refused * 4 >= drafts,
        "{sealed} sealed, {refused} refused"
    );
}
