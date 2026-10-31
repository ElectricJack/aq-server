//! PB-02 finite operating-envelope experiment. Only the disposable wrapper may
//! supply the database marker; no engine behavior or production data is changed.
use aq_playbooks_model::*;
use aq_playbooks_postgres::PostgresStorage;
use aq_playbooks_runner::{storage::*, *};
use postgres::{Client, NoTls};
use serde_json::{Value, json};
use std::result::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Barrier, Mutex},
    time::{Instant, SystemTime, UNIX_EPOCH},
};
const SAMPLES: usize = 24;
const HISTORY: [usize; 4] = [0, 32, 128, 512];
const NOW: i64 = 10_000;
#[derive(Clone, Default)]
struct Meter {
    data: Arc<Mutex<BTreeMap<String, Vec<u64>>>>,
}
impl Meter {
    fn add(&self, key: &str, start: Instant) {
        self.data
            .lock()
            .unwrap()
            .entry(key.into())
            .or_default()
            .push(start.elapsed().as_micros() as u64);
    }
    fn clear(&self) {
        self.data.lock().unwrap().clear();
    }
    fn report(&self) -> Value {
        let g = self.data.lock().unwrap();
        Value::Object(g.iter().map(|(k, v)| (k.clone(), stats(v))).collect())
    }
}
struct Measured<S> {
    inner: S,
    meter: Meter,
}
impl<S: Storage> Storage for Measured<S> {
    fn protection_available(&self) -> bool {
        self.inner.protection_available()
    }
    fn load(&self, n: &str) -> Result<VersionedState, StorageError> {
        let t = Instant::now();
        let r = self.inner.load(n);
        self.meter.add("load_us", t);
        r
    }
    fn compare_and_swap(&self, n: &str, o: &str, r: u64, s: &State) -> Result<u64, StorageError> {
        let t = Instant::now();
        let r = self.inner.compare_and_swap(n, o, r, s);
        self.meter.add("commit_us", t);
        r
    }
    fn acquire(&self, n: &str, o: &str) -> Result<(), StorageError> {
        self.inner.acquire(n, o)
    }
    fn release(&self, n: &str, o: &str) -> Result<(), StorageError> {
        self.inner.release(n, o)
    }
    fn query(&self, n: &str, q: &IndexQuery) -> Result<IndexPage, StorageError> {
        let t = Instant::now();
        let r = self.inner.query(n, q);
        self.meter.add("indexed_query_us", t);
        r
    }
    fn inspect(&self, n: &str, q: &InspectionQuery) -> Result<InspectionPage, StorageError> {
        self.inner.inspect(n, q)
    }
}
fn stats(v: &[u64]) -> Value {
    if v.is_empty() {
        return json!({"samples":0});
    }
    let mut s = v.to_vec();
    s.sort_unstable();
    let p = |percent: usize| s[(s.len() * percent).div_ceil(100).saturating_sub(1)];
    json!({"samples":s.len(),"p50":p(50),"p95":p(95),"max":s[s.len()-1],"total":s.iter().sum::<u64>(),"values":v})
}
fn emit(v: Value) {
    println!("{}", serde_json::to_string(&v).unwrap());
}
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
fn base() -> Engine<MemoryStorage> {
    let e = Engine::new(MemoryStorage::default(), "seed", "seed-owner").unwrap();
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
fn counts(s: &State) -> Value {
    json!({"runs":s.runs.len(),"terminal_runs":s.runs.values().filter(|r|r.status.terminal()).count(),"pending_children":s.runs.values().filter(|r|r.parent.is_some()&&!r.status.terminal()).count(),"events":s.events.len(),"receipts":s.receipts.len(),"canonical_bytes":canonical_bytes(s).unwrap().len(),"queued_destinations":s.events.values().flat_map(|e|&e.destinations).filter(|d|d.run.is_none()).count()})
}
fn rss_kib() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .unwrap()
        .lines()
        .find_map(|s| s.strip_prefix("VmRSS:"))
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .parse()
        .unwrap()
}
fn seed<S: Storage>(store: S, namespace: &str, state: &State) -> Engine<S> {
    store.acquire(namespace, "pb02-owner").unwrap();
    let revision = store.load(namespace).unwrap().revision;
    store
        .compare_and_swap(namespace, "pb02-owner", revision, state)
        .unwrap();
    Engine::new(store, namespace, "pb02-owner").unwrap()
}
fn pg(config: &str, label: &str, state: &State) -> (Engine<Measured<PostgresStorage>>, Meter) {
    let meter = Meter::default();
    let e = seed(
        Measured {
            inner: PostgresStorage::connect(config).unwrap(),
            meter: meter.clone(),
        },
        label,
        state,
    );
    meter.clear();
    e.store
        .inner
        .set_profiling(std::env::var("AQ_PB03_STORAGE_PROFILE").as_deref() == Ok("1"));
    (e, meter)
}
fn phase<S: Storage>(e: &mut Engine<S>, name: &str) -> (Vec<u64>, Vec<u64>) {
    phase_n(e, name, SAMPLES)
}
fn phase_n<S: Storage>(e: &mut Engine<S>, name: &str, n: usize) -> (Vec<u64>, Vec<u64>) {
    e.metrics = Default::default();
    let mut wall = vec![];
    let mut pure = vec![];
    for i in 0..n {
        let before = e.metrics.snapshot().transition.total_us;
        let t = Instant::now();
        match name {
            "event" => {
                e.admit_event("pb02", event(&format!("measure-{i}")), NOW + i as i64)
                    .unwrap();
            }
            "admit" => {
                e.admit("a", "main", Value::Null, authority("a"), NOW + i as i64)
                    .unwrap();
            }
            _ => unreachable!(),
        }
        wall.push(t.elapsed().as_micros() as u64);
        pure.push(e.metrics.snapshot().transition.total_us - before);
    }
    (wall, pure)
}
fn report_phase(config: &str, state: &State, h: usize, kind: &str) {
    let (e, meter) = pg(config, &format!("history-{h}-{kind}"), state);
    let mut e = e;
    let before = counts(&e.snapshot().unwrap());
    meter.clear();
    e.store
        .inner
        .set_profiling(std::env::var("AQ_PB03_STORAGE_PROFILE").as_deref() == Ok("1"));
    let rss = rss_kib();
    let (wall, pure) = phase(&mut e, kind);
    let timing = meter.report();
    let storage_profile = e.store.inner.profiling_snapshot();
    let end = e.snapshot().unwrap();
    emit(
        json!({"scenario":kind,"history":h,"starting":before,"ending":counts(&end),"wall_us":stats(&wall),"transition_closure_us":stats(&pure),"store":timing,"storage_profile":storage_profile,"process_rss_kib_before":rss,"process_rss_kib_after":rss_kib(),"engine_metrics":e.metrics.snapshot(),"budget_us":if kind=="event"{25000}else{0},"event_budget_pass":kind!="event"||stats(&wall)["p95"].as_u64().unwrap()<25000,"pure_closure_budget_pass":stats(&pure)["p95"].as_u64().unwrap()<1000}),
    );
    e.release().unwrap();
}
fn memory_phase(state: &State, h: usize) {
    let mut e = seed(MemoryStorage::default(), "memory", state);
    let (wall, pure) = phase(&mut e, "event");
    emit(
        json!({"scenario":"memory_event","history":h,"wall_us":stats(&wall),"transition_closure_us":stats(&pure),"ending":counts(&e.snapshot().unwrap()),"rss_kib":rss_kib()}),
    );
    e.release().unwrap();
}
fn fairness(config: &str, state: &State, h: usize, groups: [usize; 3]) {
    let label = format!("fair-{h}-{}", groups[0]);
    let (e, meter) = pg(config, &label, state);
    let mut admitted = BTreeMap::new();
    for (scope, n) in ["a", "b", "c"].into_iter().zip(groups) {
        for i in 0..n {
            let id = e
                .admit(scope, "main", Value::Null, authority(scope), NOW + i as i64)
                .unwrap();
            admitted.insert(id, Instant::now());
        }
    }
    meter.clear();
    let mut host = FakeHost::default();
    let mut wall = vec![];
    let mut queue = vec![];
    let mut order = vec![];
    for _ in 0..groups.iter().sum() {
        let candidate = e
            .query(&IndexQuery::Ready {
                scope_after: None,
                limit: 1,
            })
            .unwrap()
            .items
            .remove(0);
        let t = Instant::now();
        assert_eq!(e.drive(&mut host, NOW + 100, 1).unwrap(), 1);
        wall.push(t.elapsed().as_micros() as u64);
        queue.push(admitted[&candidate.key].elapsed().as_micros() as u64);
        order.push(candidate.scope);
    }
    let end = e.snapshot().unwrap();
    assert!(admitted.keys().all(|id| end.runs[id].status.terminal()));
    let roundrobin = order
        .iter()
        .take(groups[1] * 3)
        .enumerate()
        .all(|(i, s)| s == ["a", "b", "c"][i % 3]);
    emit(
        json!({"scenario":"ready_queue","history":h,"groups":groups,"dispatch_call_us":stats(&wall),"admission_to_first_visit_upper_bound_us":stats(&queue),"scope_order":order,"equal_prefix_round_robin":roundrobin,"dispatch_budget_pass":stats(&wall)["p95"].as_u64().unwrap()<100000,"queue_budget_pass":stats(&queue)["p95"].as_u64().unwrap()<100000,"store":meter.report(),"ending":counts(&end)}),
    );
    e.release().unwrap();
}
fn concurrent(config: &str, state: &State, threads: usize) {
    let (e, meter) = pg(config, &format!("concurrent-{threads}"), state);
    let e = Arc::new(e);
    let barrier = Arc::new(Barrier::new(threads));
    meter.clear();
    let started = Instant::now();
    let mut handles = vec![];
    for tid in 0..threads {
        let e = e.clone();
        let barrier = barrier.clone();
        handles.push(std::thread::spawn(move||{let mut outcomes=vec![];barrier.wait();for i in 0..8{let t=Instant::now();let mut attempts=0;let result=loop{attempts+=1;match e.admit("a","main",Value::Null,authority("a"),NOW+(tid*8+i)as i64){Err(Error::Storage(StorageError::Conflict))if attempts<8=>continue,r=>break r}};outcomes.push(json!({"thread":tid,"operation":i,"attempts":attempts,"success":result.is_ok(),"wall_us":t.elapsed().as_micros()as u64,"error":result.err().map(|e|e.to_string())}));}outcomes}));
    }
    let outcomes: Vec<_> = handles
        .into_iter()
        .flat_map(|h| h.join().unwrap())
        .collect();
    let wall: Vec<_> = outcomes
        .iter()
        .map(|v| v["wall_us"].as_u64().unwrap())
        .collect();
    emit(
        json!({"scenario":"concurrent_admission","threads":threads,"per_thread":8,"maximum_immediate_attempts":8,"elapsed_us":started.elapsed().as_micros(),"wall_us":stats(&wall),"outcomes":outcomes,"store":meter.report(),"ending":counts(&e.snapshot().unwrap())}),
    );
    e.release().unwrap();
}
fn pending(config: &str, n: usize) {
    let e = base();
    for i in 0..n {
        let id = e
            .admit("parent", "main", Value::Null, authority("a"), i as i64)
            .unwrap();
        e.advance(&id, i as i64).unwrap();
        let child = e.snapshot().unwrap().runs[&id].children[0].clone();
        e.advance(&child, i as i64).unwrap();
        e.advance(&id, i as i64).unwrap();
    }
    let s = e.snapshot().unwrap();
    assert_eq!(
        s.runs
            .values()
            .filter(|r| r.parent.is_some() && !r.status.terminal())
            .count(),
        n
    );
    let (mut e, meter) = pg(config, &format!("pending-{n}"), &s);
    let (wall, pure) = phase(&mut e, "event");
    emit(
        json!({"scenario":"pending_children","pending":n,"starting":counts(&s),"ending":counts(&e.snapshot().unwrap()),"event_wall_us":stats(&wall),"transition_closure_us":stats(&pure),"store":meter.report(),"budget_pass":stats(&wall)["p95"].as_u64().unwrap()<25000,"rss_kib":rss_kib()}),
    );
    e.release().unwrap();
}
fn quotas(config: &str, state: &State) {
    for kind in [
        "runs",
        "events",
        "receipts",
        "state_bytes",
        "queued_destinations",
    ] {
        let (mut e, _) = pg(config, &format!("quota-{kind}"), state);
        if kind == "queued_destinations" {
            let mut a = artifact("triggered", "a", ret());
            let c = event_contract();
            a.content.playbook.triggers.push(Trigger {
                id: "on-signal".into(),
                event: ContractRef {
                    name: c.name.clone(),
                    fingerprint: c.fingerprint().unwrap(),
                },
                entry: "main".into(),
                input: Expr::literal(Value::Null),
            });
            a = Artifact::seal(a.content).unwrap();
            install(&e, "triggered", a);
            e.config.max_queued_destinations = 1;
            e.admit_event("pb02", event("first"), NOW).unwrap();
        } else {
            match kind {
                "runs" => e.config.max_runs = 2,
                "events" => e.config.max_events = 2,
                "receipts" => e.config.max_receipts = 2,
                "state_bytes" => {
                    e.config.max_state_bytes =
                        canonical_bytes(&e.snapshot().unwrap()).unwrap().len() + 64
                }
                _ => unreachable!(),
            }
            if kind != "state_bytes" {
                for i in 0..2 {
                    if kind == "events" {
                        e.admit_event("pb02", event(&format!("quota-{i}")), NOW)
                            .unwrap();
                    } else {
                        e.admit("a", "main", Value::Null, authority("a"), NOW + i)
                            .unwrap();
                    }
                }
            }
        }
        let before = e.snapshot().unwrap();
        let t = Instant::now();
        let refusal = if matches!(kind, "events" | "queued_destinations") {
            e.admit_event("pb02", event("excess"), NOW).map(|_| ())
        } else {
            e.admit("a", "main", Value::Null, authority("a"), NOW + 10)
                .map(|_| ())
        };
        let elapsed = t.elapsed().as_micros();
        assert!(refusal.is_err());
        let after = e.snapshot().unwrap();
        assert_eq!(
            canonical_bytes(&before).unwrap(),
            canonical_bytes(&after).unwrap()
        );
        emit(
            json!({"scenario":"quota_refusal","quota":kind,"limits":e.config,"refused":true,"state_unchanged":true,"refusal_us":elapsed,"retained":counts(&after),"error":refusal.unwrap_err().to_string()}),
        );
        e.release().unwrap();
    }
}
fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().nth(1).as_deref() == Some("--check-fixtures") {
        let e = base();
        let id = e.admit("parent", "main", Value::Null, authority("a"), 0)?;
        e.advance(&id, 0)?;
        let child = e.snapshot()?.runs[&id].children[0].clone();
        e.advance(&child, 0)?;
        e.advance(&id, 0)?;
        assert!(!e.snapshot()?.runs[&child].status.terminal());
        e.admit_event("pb02", event("fixture-check"), 0)?;
        emit(json!({"fixture_check":"passed","state":counts(&e.snapshot()?)}));
        return Ok(());
    }
    if cfg!(debug_assertions) {
        return Err("release benchmark only".into());
    }
    if std::env::args().nth(1).as_deref() == Some("--state-costs") {
        state_costs();
        return Ok(());
    }
    let config = std::env::var("AQ_PLAYBOOKS_TEST_DATABASE_URL")?;
    let token = std::env::var("AQ_PB02_DISPOSABLE_TOKEN")?;
    let mut client = Client::connect(&config, NoTls)?;
    let row = client.query_one("SELECT token FROM public.aq_pb02_disposable", &[])?;
    assert_eq!(
        row.get::<_, String>(0),
        token,
        "disposable database marker mismatch"
    );
    emit(
        json!({"scenario":"configuration","version":2,"samples":SAMPLES,"full_history_sizes":HISTORY,"minimal_history_sizes":[0,1,8,32],"minimal_repeats":2,"minimal_event_samples":64,"minimal_ready_batches":24,"budgets_us":{"transition":1000,"event_admission":25000,"ready_dispatch":100000},"seed":"real MemoryStorage engine admissions/transitions/events; one public Storage CAS imports each fully indexed snapshot outside measurement","protection":"disabled","payload":"null run input,128-byte event string,return-only steps; no provider/network effects","default_quotas":EngineConfig::default(),"started_unix_ms":SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()}),
    );
    if std::env::args().nth(1).as_deref() == Some("--minimal") {
        minimal(&config);
        emit(json!({"scenario":"complete","profile":"minimal","rss_kib":rss_kib()}));
        return Ok(());
    }
    let e = base();
    let empty = e.snapshot()?;
    let mut previous = 0;
    for h in HISTORY {
        for i in previous..h {
            let id = e.admit("a", "main", Value::Null, authority("a"), i as i64)?;
            e.advance(&id, i as i64)?;
            e.admit_event("pb02", event(&format!("history-{i}")), i as i64)?;
        }
        previous = h;
        let state = e.snapshot()?;
        for kind in ["admit", "event"] {
            report_phase(&config, &state, h, kind);
        }
        memory_phase(&state, h);
        for groups in [[4, 1, 1], [16, 4, 4]] {
            fairness(&config, &state, h, groups);
        }
    }
    for n in [0, 8, 32, 128] {
        pending(&config, n);
    }
    for threads in [1, 2, 4] {
        concurrent(&config, &empty, threads);
    }
    quotas(&config, &empty);
    emit(json!({"scenario":"complete","rss_kib":rss_kib()}));
    Ok(())
}

fn minimal_config() -> EngineConfig {
    EngineConfig {
        max_runs: 35,
        max_events: 96,
        max_receipts: 70,
        max_queued_destinations: 1,
        max_state_bytes: 262144,
    }
}
fn minimal(config: &str) {
    let seed = Engine::new(MemoryStorage::default(), "minimal-seed", "seed").unwrap();
    install(&seed, "a", artifact("minimal-return", "a", ret()));
    let mut previous = 0;
    for history in [0, 1, 8, 32] {
        for i in previous..history {
            let id = seed
                .admit("a", "main", Value::Null, authority("a"), i as i64)
                .unwrap();
            seed.advance(&id, i as i64).unwrap();
            seed.admit_event("pb02", event(&format!("minimal-history-{i}")), i as i64)
                .unwrap();
        }
        previous = history;
        let state = seed.snapshot().unwrap();
        for repeat in 0..2 {
            let (mut e, meter) = pg(config, &format!("minimal-event-{history}-{repeat}"), &state);
            e.config = minimal_config();
            let (wall, pure) = phase_n(&mut e, "event", 64);
            let store = meter.report();
            let end = e.snapshot().unwrap();
            emit(
                json!({"scenario":"minimal_event","history":history,"repeat":repeat,"installed_artifacts":end.artifacts.len(),"limits":e.config,"starting":counts(&state),"ending":counts(&end),"wall_us":stats(&wall),"transition_closure_us":stats(&pure),"store":store,"budget_pass":stats(&wall)["p95"].as_u64().unwrap()<25000&&stats(&pure)["p95"].as_u64().unwrap()<1000}),
            );
            e.release().unwrap();
            for backlog in [1, 3] {
                let mut calls = vec![];
                let mut queue = vec![];
                let mut final_counts = Value::Null;
                for batch in 0..24 {
                    let (mut e, _) = pg(
                        config,
                        &format!("minimal-ready-{history}-{repeat}-{backlog}-{batch}"),
                        &state,
                    );
                    e.config = minimal_config();
                    let mut admitted = BTreeMap::new();
                    for i in 0..backlog {
                        let id = e
                            .admit("a", "main", Value::Null, authority("a"), NOW + i as i64)
                            .unwrap();
                        admitted.insert(id, Instant::now());
                    }
                    for _ in 0..backlog {
                        let selected = e
                            .query(&IndexQuery::Ready {
                                scope_after: None,
                                limit: 1,
                            })
                            .unwrap()
                            .items
                            .remove(0);
                        let t = Instant::now();
                        assert_eq!(e.drive(&mut FakeHost::default(), NOW + 100, 1).unwrap(), 1);
                        calls.push(t.elapsed().as_micros() as u64);
                        queue.push(admitted[&selected.key].elapsed().as_micros() as u64);
                    }
                    let end = e.snapshot().unwrap();
                    assert!(end.runs.values().all(|r| r.status.terminal()));
                    final_counts = counts(&end);
                    e.release().unwrap();
                }
                emit(
                    json!({"scenario":"minimal_ready_queue","history":history,"repeat":repeat,"backlog":backlog,"batches":24,"installed_artifacts":1,"limits":minimal_config(),"starting":counts(&state),"ending_each_batch":final_counts,"dispatch_call_us":stats(&calls),"admission_to_first_visit_upper_bound_us":stats(&queue),"budget_pass":stats(&calls)["p95"].as_u64().unwrap()<100000&&stats(&queue)["p95"].as_u64().unwrap()<100000}),
                );
            }
        }
    }
}

fn state_costs() {
    use std::hint::black_box;
    let seed = base();
    let mut prior = 0;
    for history in HISTORY {
        for i in prior..history {
            let id = seed
                .admit("a", "main", Value::Null, authority("a"), i as i64)
                .unwrap();
            seed.advance(&id, i as i64).unwrap();
            seed.admit_event("pb02", event(&format!("cpu-history-{i}")), i as i64)
                .unwrap();
        }
        prior = history;
        let state = seed.snapshot().unwrap();
        let value = serde_json::to_value(&state).unwrap();
        let mut clone = vec![];
        let mut index_clone = vec![];
        let mut quota = vec![];
        let mut encode = vec![];
        let mut decode = vec![];
        for _ in 0..24 {
            let t = Instant::now();
            black_box(state.clone());
            clone.push(t.elapsed().as_micros() as u64);
            let t = Instant::now();
            black_box(state.indexes.clone());
            index_clone.push(t.elapsed().as_micros() as u64);
            let t = Instant::now();
            black_box(canonical_bytes(&state).unwrap());
            quota.push(t.elapsed().as_micros() as u64);
            let t = Instant::now();
            black_box(serde_json::to_value(&state).unwrap());
            encode.push(t.elapsed().as_micros() as u64);
            let input = value.clone();
            let t = Instant::now();
            black_box(serde_json::from_value::<State>(input).unwrap());
            decode.push(t.elapsed().as_micros() as u64);
        }
        emit(
            json!({"scenario":"state_cpu_costs","history":history,"state":counts(&state),"samples":24,"state_clone_us":stats(&clone),"index_clone_us":stats(&index_clone),"exact_quota_canonical_us":stats(&quota),"serde_value_encode_us":stats(&encode),"serde_value_decode_us":stats(&decode),"rss_kib":rss_kib(),"limits":"Synthetic unprotected CPU probe; decoding input Value clone excluded; no SQL, no provider, no benchmark engine optimization."}),
        );
    }
}
