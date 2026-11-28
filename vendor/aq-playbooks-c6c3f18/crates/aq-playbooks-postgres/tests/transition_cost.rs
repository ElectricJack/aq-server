//! D10 (PB-04) deterministic transition-cost proof. The five-artifact PB-02
//! fixture is seeded at histories 0/32/128/512 into the record backend (the
//! shared `transition_v1` planner over keyed records) and a full-State
//! MemoryStorage reference. Since spec 0003 a finished run leaves the State,
//! so "history" is run history (settled cohorts written as history rows)
//! next to the events still in their window. Every timed operation runs on
//! both; the complete canonical State, revision, maintained record counts
//! and inspection (live plus history) are compared after each step. The
//! record backend's work counters must show zero full materialization and
//! history-flat record work; admission and event ingress also take zero
//! fallbacks, and the settling `dispatch` is a full commit exactly where the
//! backend makes cohort removal one (format 3, SC-13).
use aq_playbooks_model::*;
use aq_playbooks_runner::{storage::*, transition::*, *};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::atomic::Ordering,
};

const HISTORY: [usize; 4] = [0, 32, 128, 512];
/// Deterministic counters need few samples; the PG timing matrix uses 24.
const SAMPLES: usize = 8;
const NOW: i64 = 10_000;

fn authority(scope: &str) -> Authority {
    Authority {
        actor: "pb02-synthetic".into(),
        scope: scope.into(),
        capabilities: BTreeSet::new(),
    }
}
fn event_contract() -> EventContract {
    EventContract {
        sensitive_payload_fields: BTreeSet::new(),
        name: "pb02.signal".into(),
        version: "1".into(),
        payload: Schema::string(512),
        trusted_producers: BTreeSet::from(["pb02".into()]),
        scopes: BTreeSet::from(["a".into(), "b".into(), "c".into()]),
        correlation_fields: BTreeSet::new(),
        subject_field: None,
        revision_field: None,
        max_payload_bytes: 1024,
        retention_ms: 86400000,
    }
}
fn event(id: &str) -> EventEnvelope {
    let c = event_contract();
    EventEnvelope {
        producer: "pb02".into(),
        event_id: id.into(),
        contract: ContractRef {
            name: c.name.clone(),
            fingerprint: c.fingerprint().unwrap(),
        },
        scope: "a".into(),
        correlation: BTreeMap::new(),
        subject: None,
        revision: None,
        causation: None,
        occurred_at_ms: Some(0),
        observed_at_ms: None,
        payload: json!("p".repeat(128)),
    }
}
fn ret() -> Step {
    Step {
        op: Operation::Return {
            outcome: "ok".into(),
            output: Expr::literal(Value::Null),
        },
        edges: BTreeMap::new(),
        on_error: None,
    }
}
fn artifact(id: &str, scope: &str, mut first: Step) -> Artifact {
    if !matches!(&first.op, Operation::Return { .. }) {
        first.on_error = Some("return".into());
    }
    Artifact::seal(ArtifactContent {
        format_version: 1,
        compiler: "pb02-benchmark-v1".into(),
        source: SourceBinding {
            digest: hash_bytes(id.as_bytes()),
            header: SourceHeader {
                format_version: 1,
                id: id.into(),
                scope: scope.into(),
                interfaces: BTreeSet::from(["main".into()]),
                definitions: BTreeSet::new(),
            },
        },
        playbook: Playbook {
            id: id.into(),
            purpose: "Finite synthetic storage/scheduling experiment".into(),
            scope: scope.into(),
            entries: BTreeMap::from([(
                "main".into(),
                EntryPoint {
                    input: Schema::Null,
                    outcomes: BTreeMap::from([("ok".into(), Schema::Null)]),
                    start: "first".into(),
                },
            )]),
            steps: if matches!(&first.op, Operation::Return { .. }) {
                BTreeMap::from([("first".into(), first)])
            } else {
                BTreeMap::from([("first".into(), first), ("return".into(), ret())])
            },
            triggers: vec![],
            limits: Limits::default(),
            required_capabilities: BTreeSet::new(),
        },
        registry: Registry {
            commands: BTreeMap::new(),
            events: BTreeMap::from([("pb02.signal".into(), event_contract())]),
        },
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::new(),
        source_map: BTreeMap::new(),
    })
    .unwrap()
}
fn install<S: Storage>(e: &Engine<S>, binding: &str, a: Artifact) {
    let scope = a.content.playbook.scope.clone();
    e.install(a.clone()).unwrap();
    e.activate_at(binding, &a.digest, authority(&scope), 0)
        .unwrap();
}
/// The PB-02 five-artifact fixture: return-a/b/c, child-wait, and parent
/// (an owned asynchronous call to child-wait).
fn base<S: Storage>(store: S) -> Engine<S> {
    let e = Engine::new(store, "seed", "seed-owner").unwrap();
    for scope in ["a", "b", "c"] {
        install(
            &e,
            scope,
            artifact(&format!("return-{scope}"), scope, ret()),
        );
    }
    let child = artifact(
        "child-wait",
        "a",
        Step {
            op: Operation::Wait {
                event: None,
                correlation: BTreeMap::new(),
                minimum_revision: None,
                deadline_ms: 1_000_000,
                bind: "wake".into(),
            },
            edges: BTreeMap::from([("timeout".into(), "return".into())]),
            on_error: None,
        },
    );
    install(&e, "child", child.clone());
    let mut parent = artifact(
        "parent",
        "a",
        Step {
            op: Operation::Call {
                dependency: "child".into(),
                entry: "main".into(),
                input: Expr::literal(Value::Null),
                mode: CallMode::OwnedAsync,
                bind: "handle".into(),
            },
            edges: BTreeMap::from([("started".into(), "return".into())]),
            on_error: None,
        },
    );
    parent.content.dependencies.insert(
        "child".into(),
        CallableDependency {
            binding: "child".into(),
            selection: DependencySelection::Exact,
            artifact: child.digest.clone(),
            interfaces: child.content.playbook.entries.clone(),
            capabilities: BTreeSet::new(),
            scope: "a".into(),
            dependencies: BTreeSet::new(),
        },
    );
    parent = Artifact::seal(parent.content).unwrap();
    install(&e, "parent", parent);
    e
}
/// Seed `state` and its run history (`cohorts`, written as history rows by
/// the seeding commit).
fn seed<S: Storage>(
    store: S,
    namespace: &str,
    state: &State,
    cohorts: &[SettledCohort],
) -> Engine<S> {
    store.acquire(namespace, "pb02-owner").unwrap();
    let revision = store.load(namespace).unwrap().revision;
    let mut seeded = state.clone();
    seeded.settled = cohorts.to_vec();
    store
        .compare_and_swap(namespace, "pb02-owner", revision, &seeded)
        .unwrap();
    Engine::new(store, namespace, "pb02-owner").unwrap()
}

#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Work {
    full_loads: usize,
    /// Full-State commits, including PostgreSQL's complete post-plan path.
    full_commits: usize,
    /// Bounded record commits/transitions.
    transitions: usize,
    record_reads: usize,
}
/// A record-addressed backend under test, with its value-free counters.
trait Candidate: Storage {
    fn work(&self) -> Work;
    fn fallbacks(&self) -> Vec<String>;
    /// Maintained record counts and header bytes, where the backend exposes
    /// them.
    fn accounting(&self, namespace: &str) -> Option<(BTreeMap<&'static str, usize>, usize)>;
    /// Whether removing a settled cohort is a full (heavy) commit here.
    fn settling_commit_is_full(&self) -> bool;
}
impl Candidate for RecordStorage {
    fn work(&self) -> Work {
        let s = &self.stats;
        Work {
            full_loads: s.full_loads.load(Ordering::Relaxed),
            full_commits: s.full_commits.load(Ordering::Relaxed),
            transitions: s.transitions.load(Ordering::Relaxed),
            record_reads: s.record_reads.load(Ordering::Relaxed),
        }
    }
    fn fallbacks(&self) -> Vec<String> {
        self.stats.fallbacks()
    }
    fn accounting(&self, namespace: &str) -> Option<(BTreeMap<&'static str, usize>, usize)> {
        let a = RecordStorage::accounting(self, namespace).unwrap();
        let counts = a.fields.into_iter().filter(|(_, n)| *n > 0).collect();
        Some((counts, a.header_len))
    }
    fn settling_commit_is_full(&self) -> bool {
        false
    }
}

/// Per-operation record work at one history size.
#[derive(Debug, Default)]
struct Cell {
    reads: Vec<usize>,
    header_len: Vec<usize>,
}

/// Full-State comparison against the reference after every step: revision
/// and the complete serialized State (every container, record backend fully
/// assembled). `exact` also checks canonical bytes and record counts.
fn compare<C: Candidate>(
    ns: &str,
    m: &Engine<MemoryStorage>,
    r: &Engine<C>,
    step: &str,
    exact: bool,
) {
    let mv = m.store.load(ns).unwrap();
    let rv = r.store.load(ns).unwrap();
    assert_eq!(mv.revision, rv.revision, "revision after {step}");
    if serde_json::to_vec(&mv.state).unwrap() != serde_json::to_vec(&rv.state).unwrap() {
        let (a, b) = (
            serde_json::to_value(&mv.state).unwrap(),
            serde_json::to_value(&rv.state).unwrap(),
        );
        for (k, v) in a.as_object().unwrap() {
            assert_eq!(Some(v), b.get(k), "state.{k} after {step}");
        }
        panic!("State differs after {step}");
    }
    for kind in [
        InspectionKind::Runs,
        InspectionKind::Effects,
        InspectionKind::Receipts,
    ] {
        let query = InspectionQuery {
            kind,
            run: None,
            after: None,
            limit: 1000,
        };
        assert_eq!(
            m.store.inspect(ns, &query).unwrap().items,
            r.store.inspect(ns, &query).unwrap().items,
            "{kind:?} inspection after {step}"
        );
    }
    if exact {
        assert_eq!(
            canonical_bytes(&mv.state).unwrap(),
            canonical_bytes(&rv.state).unwrap(),
            "canonical State after {step}"
        );
        if let Some((counts, _)) = r.store.accounting(ns) {
            let mut plain = mv.state.clone();
            let expected: BTreeMap<&'static str, usize> = take_patch(&mut plain)
                .unwrap()
                .changes
                .into_iter()
                .filter_map(|(path, change)| match change {
                    aq_playbooks_runner::lazy::ContainerChange::Replace(rows)
                        if !rows.is_empty() =>
                    {
                        Some((path.field, rows.len()))
                    }
                    _ => None,
                })
                .collect();
            assert_eq!(counts, expected, "maintained record counts after {step}");
        }
    }
}

fn measure<C: Candidate>(
    (state, cohorts): &(State, Vec<SettledCohort>),
    ns: &str,
    candidate: C,
    samples: usize,
) -> BTreeMap<&'static str, Cell> {
    let m = seed(MemoryStorage::retaining_settled(), ns, state, cohorts);
    let r = seed(candidate, ns, state, cohorts);
    compare(ns, &m, &r, "seed", true);
    let full_settling = r.store.settling_commit_is_full();
    let mut cells: BTreeMap<&'static str, Cell> = BTreeMap::new();
    for i in 0..samples {
        for kind in ["admit", "event", "dispatch"] {
            let at = NOW + i as i64;
            let step = format!("{kind} {i} in {ns}");
            let settled = m.store.settled_cohorts(ns).len();
            let reference = format!(
                "{:?}",
                match kind {
                    "admit" => m
                        .admit("a", "main", Value::Null, authority("a"), at)
                        .map(|_| 0),
                    "event" => m
                        .admit_event("pb02", event(&format!("measure-{i}")), at)
                        .map(|_| 0),
                    _ => m.drive(&mut FakeHost::default(), at + 100, 1),
                }
            );
            let before = r.store.work();
            let fallbacks = r.store.fallbacks().len();
            let candidate = format!(
                "{:?}",
                match kind {
                    "admit" => r
                        .admit("a", "main", Value::Null, authority("a"), at)
                        .map(|_| 0),
                    "event" => r
                        .admit_event("pb02", event(&format!("measure-{i}")), at)
                        .map(|_| 0),
                    _ => r.drive(&mut FakeHost::default(), at + 100, 1),
                }
            );
            let after = r.store.work();
            assert_eq!(reference, candidate, "result of {step}");
            assert!(reference.starts_with("Ok"), "{step}: {reference}");
            // The settling commits of this step (the drive finishes one run).
            let settling = m.store.settled_cohorts(ns).len() - settled;
            assert_eq!(settling, usize::from(kind == "dispatch"), "{step} settles");
            // Zero full materialization; full commits only where cohort
            // removal is one; zero fallbacks except when a cohort leaves
            // (receipt compaction rewrites the positional sequence, SC-13).
            assert_eq!(after.full_loads, before.full_loads, "full load in {step}");
            let full = if full_settling { settling } else { 0 };
            assert_eq!(
                after.full_commits - before.full_commits,
                full,
                "full commits in {step}"
            );
            if kind == "dispatch" {
                assert!(
                    after.transitions + after.full_commits
                        > before.transitions + before.full_commits,
                    "{step} committed"
                );
            } else {
                assert!(
                    after.transitions > before.transitions,
                    "{step} used the bounded transition path"
                );
                assert_eq!(
                    r.store.fallbacks()[fallbacks..].to_vec(),
                    Vec::<String>::new(),
                    "{step}"
                );
            }
            let cell = cells.entry(kind).or_default();
            cell.reads.push(after.record_reads - before.record_reads);
            if let Some((_, header_len)) = r.store.accounting(ns) {
                cell.header_len.push(header_len);
            }
            compare(ns, &m, &r, &step, i + 1 == samples);
        }
    }
    cells
}

/// History snapshots of the five-artifact fixture: `h` finished runs (now
/// run history: their settled cohorts) and `h` events still in their window.
/// Grown on the reference backend, which keeps the settled cohorts.
fn histories(sizes: &[usize]) -> Vec<(usize, (State, Vec<SettledCohort>))> {
    let e = base(MemoryStorage::retaining_settled());
    // Task 8b: above the largest history, so every event stays remembered
    // (none is evicted by count) and record work is measured against all.
    e.set_retention_policy(RetentionPolicy {
        replay_keep: 1_000,
        ..RetentionPolicy::default()
    })
    .unwrap();
    let mut previous = 0;
    let mut out = vec![];
    for &h in sizes {
        for i in previous..h {
            let id = e
                .admit("a", "main", Value::Null, authority("a"), i as i64)
                .unwrap();
            e.advance(&id, i as i64).unwrap();
            e.admit_event("pb02", event(&format!("history-{i}")), i as i64)
                .unwrap();
        }
        previous = h;
        let state = e.snapshot().unwrap();
        let cohorts = e.store.settled_cohorts("seed");
        assert_eq!(state.artifacts.len(), 5);
        assert!(state.events.len() >= h && state.runs.is_empty());
        assert_eq!(cohorts.len(), h);
        out.push((h, (state, cohorts)));
    }
    out
}

fn assert_flat(results: &BTreeMap<usize, BTreeMap<&'static str, Cell>>) {
    let summary: BTreeMap<_, _> = results
        .iter()
        .map(|(h, cells)| {
            (
                *h,
                cells
                    .iter()
                    .map(|(k, c)| {
                        (
                            *k,
                            (
                                c.reads.iter().max().copied().unwrap(),
                                c.reads.iter().sum::<usize>(),
                                c.header_len.iter().max().copied(),
                            ),
                        )
                    })
                    .collect::<BTreeMap<_, _>>(),
            )
        })
        .collect();
    eprintln!("(max record reads/op, total record reads, max header bytes) = {summary:#?}");
    let flat = &results[&0];
    for (h, cells) in results {
        for (kind, cell) in cells {
            let reference = &flat[kind];
            // Record work per operation is independent of retained history:
            // the same operation sequence touches the same number of records.
            assert_eq!(
                cell.reads, reference.reads,
                "{kind} record reads at history {h} differ from history 0"
            );
            // The header (the only per-transition full encode) does not grow
            // with retained history; only decimal digits of counters vary.
            for (n, (a, b)) in cell
                .header_len
                .iter()
                .zip(&reference.header_len)
                .enumerate()
            {
                assert!(
                    a.abs_diff(*b) <= 16,
                    "{kind} {n} header bytes {a} at history {h} vs {b} at history 0"
                );
            }
        }
    }
}

#[test]
fn transition_cost_flat_with_history() {
    let results = histories(&HISTORY)
        .into_iter()
        .map(|(h, seeded)| {
            (
                h,
                measure(&seeded, &format!("pb04-{h}"), RecordStorage::new(), SAMPLES),
            )
        })
        .collect();
    assert_flat(&results);
}

/// The same proof against PostgreSQL writer format 3 (`connect_records`).
#[cfg(feature = "postgres-tests")]
mod postgres {
    use super::*;
    use aq_playbooks_postgres::PostgresStorage;
    impl Candidate for PostgresStorage {
        fn work(&self) -> Work {
            let s = self.transition_stats();
            Work {
                full_loads: s.full_loads as usize,
                full_commits: (s.full_commits + s.heavy) as usize,
                transitions: s.incremental as usize,
                record_reads: s.record_reads as usize,
            }
        }
        fn fallbacks(&self) -> Vec<String> {
            self.transition_stats().fallbacks
        }
        fn accounting(&self, _: &str) -> Option<(BTreeMap<&'static str, usize>, usize)> {
            None
        }
        fn settling_commit_is_full(&self) -> bool {
            true
        }
    }
    #[test]
    fn transition_cost_flat_with_history_postgres() {
        let config = std::env::var("AQ_PLAYBOOKS_TEST_DATABASE_URL")
            .expect("set AQ_PLAYBOOKS_TEST_DATABASE_URL to a disposable PostgreSQL database");
        let run = format!("{}_{}", std::process::id(), NOW);
        let results = histories(&[0, 32])
            .into_iter()
            .map(|(h, seeded)| {
                let store = PostgresStorage::connect_records(&config).unwrap();
                (h, measure(&seeded, &format!("pb04_{run}_{h}"), store, 4))
            })
            .collect();
        assert_flat(&results);
    }
}
