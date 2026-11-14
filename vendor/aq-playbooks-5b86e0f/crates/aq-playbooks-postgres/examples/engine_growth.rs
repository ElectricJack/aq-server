//! Stored size and per-transition cost
//! of one namespace as completed playbook runs accumulate, per storage
//! configuration. Drives a 6-step playbook (effect -> correlated event wait ->
//! decision -> effect -> return) to completion N times with a virtual clock.
//!
//! Runs are admitted with `admit_once` (keyed admission).
//!
//! Usage: engine_growth <steady|burst|a|b|c|d|e> <runs> <database url>
//!                      [<run spacing ms> [<window ms>]]   (configs a-e only)
//!   a protected format 2, no GC          b unprotected format 2, no GC
//!   c unprotected format 3, no GC        d format 3, GC after every run
//!   e protected format 2, GC after every run
//! d/e use the smallest retention windows the engine accepts (1 ms).
//! The virtual clock starts each run `run spacing` (default 100 s) after the
//! previous one and steps 10 ms per transition; spacing 0 freezes the clock
//! (a burst: every commit carries time 0). `window` sets the policy's event
//! and keyed admission windows (spec 0003), else they keep their defaults.
//! JSON lines on stdout; progress on stderr.
//!
//! Spec 0003 acceptance (protected format 2, `collect_garbage` never called,
//! event and admission windows 10 minutes, every run admitted with
//! `admit_once_at(.., Some(now), now)` and its event with
//! `admit_event_at(.., Some(now), now)`):
//! - `steady`: run k starts at k * 10,000 ms. Prints the `verdict` lines of
//!   acceptance 1 (record bytes and median step at the last run within 1.2x
//!   of run 200; `namespaces` on disk within 3x of run 2,000) and 3 (paging
//!   `Runs` lists every finished run once, each equal to the projection
//!   captured in the commit that removed it).
//! - `burst`: the clock frozen at 0 for all runs, then the NR-2 sequence:
//!   the clock moves past the window and 20 runs complete at that frozen
//!   clock (the literal "after 20" figure), then the clock moves past the
//!   window once more and one more run completes; that record is compared
//!   with run 1. Prints the verdict of acceptance 2 (and 3).
//! - `ENGINE_GROWTH_HISTORY_KEEP=<n>` (optional) sets `history_keep`;
//!   acceptance 3 then expects the newest `min(runs, n)` runs.
//!
//! Optional environment (defaults keep the behaviour above), used to run
//! the benchmark on top of a restored legacy namespace (Task 9):
//! - `ENGINE_GROWTH_KEY_PREFIX`: prefix of admission keys, tickets and event
//!   ids, so new runs do not repeat keys the namespace remembers;
//! - `ENGINE_GROWTH_START_MS`: added to every host time;
//! - `ENGINE_GROWTH_COMMIT_LOG=<n>`: a `commit` JSON line for each of the
//!   first `n` commits, counted from the open commit (1): its step time,
//!   the record size, the legacy cohorts and legacy window entries left in
//!   the State it wrote, history rows, legacy tombstone rows and the drain
//!   state. Gathered outside the timed window.
use aq_playbooks_model::*;
use aq_playbooks_postgres::{PostgresStorage, ProtectionKeyring};
use aq_playbooks_runner::{
    retention::RetentionPolicy,
    storage::*,
    transition::{TransitionOutcome, TransitionRequest},
    *,
};
use postgres::{Client, NoTls};
use serde_json::{Value, json};
use std::result::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
    time::Instant,
};

pub(crate) const NS: &str = "growth";
const RUN_SPACING_MS: i64 = 100_000;
const CHECKPOINTS: [usize; 9] = [1, 50, 100, 200, 500, 1000, 2000, 5000, 10000];
const STOP_US: u64 = 5_000_000;
/// Spec 0003 acceptance: 10 s per run, 10-minute windows.
pub(crate) const STEADY_SPACING_MS: i64 = 10_000;
pub(crate) const ACCEPTANCE_WINDOW_MS: u64 = 600_000;

fn set(xs: &[&str]) -> BTreeSet<String> {
    xs.iter().map(|s| s.to_string()).collect()
}
pub(crate) fn authority() -> Authority {
    Authority {
        actor: "bench".into(),
        scope: "project".into(),
        capabilities: set(&["execute"]),
    }
}
fn r(root: &str, path: &[&str]) -> Expr {
    Expr::Ref {
        root: root.into(),
        path: path.iter().map(|s| s.to_string()).collect(),
    }
}
fn ticket_object() -> Expr {
    Expr::Object {
        fields: BTreeMap::from([("ticket".into(), r("input", &["ticket"]))]),
    }
}
fn command(name: &str) -> CommandContract {
    CommandContract {
        name: name.into(),
        version: "1".into(),
        input: Schema::object(BTreeMap::from([("ticket".into(), Schema::string(64))])),
        outcomes: BTreeMap::from([(
            "ok".into(),
            Schema::object(BTreeMap::from([("summary".into(), Schema::string(2048))])),
        )]),
        capabilities: set(&["execute"]),
        scopes: set(&["project"]),
        effect: Effect::Mutation,
        recovery: Recovery::KeyedIdempotent,
        asynchronous: None,
        cancellation: false,
        public_result_fields: BTreeSet::new(),
        sensitive_input_fields: BTreeSet::new(),
        sensitive_output_fields: BTreeSet::new(),
        max_payload_bytes: 4096,
        deadline_ms: 60_000,
    }
}
pub(crate) fn signal_contract() -> EventContract {
    EventContract {
        sensitive_payload_fields: BTreeSet::new(),
        name: "bench.signal".into(),
        version: "1".into(),
        payload: Schema::object(BTreeMap::from([("detail".into(), Schema::string(1024))])),
        trusted_producers: set(&["host"]),
        scopes: set(&["project"]),
        correlation_fields: set(&["ticket"]),
        subject_field: None,
        revision_field: None,
        max_payload_bytes: 2048,
        retention_ms: 60_000,
    }
}
fn cref(c: &CommandContract) -> ContractRef {
    ContractRef {
        name: c.name.clone(),
        fingerprint: c.fingerprint().unwrap(),
    }
}
fn edges(xs: &[(&str, &str)]) -> BTreeMap<String, String> {
    xs.iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect()
}
pub(crate) fn playbook() -> Artifact {
    let prepare = command("bench.prepare");
    let finish = command("bench.finish");
    let signal = signal_contract();
    let steps = BTreeMap::from([
        (
            "prepare".into(),
            Step {
                op: Operation::Invoke {
                    command: cref(&prepare),
                    input: ticket_object(),
                    bind: "prepared".into(),
                    retry: None,
                },
                edges: edges(&[("ok", "await")]),
                on_error: Some("failed".into()),
            },
        ),
        (
            "await".into(),
            Step {
                op: Operation::Wait {
                    event: Some(ContractRef {
                        name: signal.name.clone(),
                        fingerprint: signal.fingerprint().unwrap(),
                    }),
                    correlation: BTreeMap::from([("ticket".into(), r("input", &["ticket"]))]),
                    minimum_revision: None,
                    deadline_ms: 60_000,
                    bind: "signal".into(),
                },
                edges: edges(&[("received", "check"), ("timeout", "failed")]),
                on_error: Some("failed".into()),
            },
        ),
        (
            "check".into(),
            Step {
                op: Operation::Decide {
                    predicate: Expr::Exists {
                        value: Box::new(r("prepared", &[])),
                    },
                },
                edges: edges(&[
                    ("true", "finish"),
                    ("false", "failed"),
                    ("unknown", "failed"),
                ]),
                on_error: Some("failed".into()),
            },
        ),
        (
            "finish".into(),
            Step {
                op: Operation::Invoke {
                    command: cref(&finish),
                    input: ticket_object(),
                    bind: "finished".into(),
                    retry: None,
                },
                edges: edges(&[("ok", "done")]),
                on_error: Some("failed".into()),
            },
        ),
        (
            "done".into(),
            Step {
                op: Operation::Return {
                    outcome: "ok".into(),
                    output: ticket_object(),
                },
                edges: BTreeMap::new(),
                on_error: None,
            },
        ),
        (
            "failed".into(),
            Step {
                op: Operation::Return {
                    outcome: "error".into(),
                    output: Expr::literal(Value::Null),
                },
                edges: BTreeMap::new(),
                on_error: None,
            },
        ),
    ]);
    Artifact::seal(ArtifactContent {
        format_version: 1,
        compiler: "engine-growth-bench".into(),
        source: SourceBinding {
            digest: hash_bytes(b"engine-growth-bench"),
            header: SourceHeader {
                format_version: 1,
                id: "growth".into(),
                scope: "project".into(),
                interfaces: set(&["main"]),
                definitions: BTreeSet::new(),
            },
        },
        playbook: Playbook {
            id: "growth".into(),
            purpose: "engine growth measurement".into(),
            scope: "project".into(),
            entries: BTreeMap::from([(
                "main".into(),
                EntryPoint {
                    input: Schema::object(BTreeMap::from([("ticket".into(), Schema::string(64))])),
                    outcomes: BTreeMap::from([
                        (
                            "ok".into(),
                            Schema::object(BTreeMap::from([("ticket".into(), Schema::string(64))])),
                        ),
                        ("error".into(), Schema::Null),
                    ]),
                    start: "prepare".into(),
                },
            )]),
            steps,
            triggers: vec![],
            limits: Limits::default(),
            required_capabilities: set(&["execute"]),
        },
        registry: Registry {
            commands: BTreeMap::from([
                (prepare.name.clone(), prepare),
                (finish.name.clone(), finish),
            ]),
            events: BTreeMap::from([(signal.name.clone(), signal)]),
        },
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::new(),
        source_map: BTreeMap::new(),
    })
    .unwrap()
}

pub(crate) struct BenchHost;
impl Host for BenchHost {
    fn authorize(&self, a: &Authority, c: &CommandContract) -> bool {
        c.capabilities.is_subset(&a.capabilities) && c.scopes.contains(&a.scope)
    }
    fn dispatch(&mut self, i: &Intent) -> DispatchResult {
        DispatchResult::Completed {
            outcome: "ok".into(),
            output: json!({"summary": format!("{} handled {}", i.command.name, "s".repeat(400))}),
        }
    }
    fn reconcile(&mut self, i: &Intent) -> DispatchResult {
        self.dispatch(i)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Sample {
    pub(crate) us: u64,
    wal: i64,
    path: &'static str,
    gc: bool,
}
#[derive(Default)]
struct Rec {
    open: Option<(Instant, i64)>,
    samples: Vec<Sample>,
    unchanged_fallback: u64,
    gc: bool,
}
/// Times every Engine transaction: a supported `transition_v1` call, or the
/// unsupported probe + `load` + plan + `compare_and_swap` sequence. WAL bytes
/// are read on a separate monitor connection outside the timed window.
pub(crate) struct Measured {
    pub(crate) inner: PostgresStorage,
    pub(crate) monitor: Mutex<Client>,
    rec: Mutex<Rec>,
    /// `ENGINE_GROWTH_COMMIT_LOG`: commits to log, and commits seen.
    commit_log: (u64, std::sync::atomic::AtomicU64),
    /// The public projection (`inspection::public_run`) of every run a
    /// committed State removed (`state.settled`), taken at that commit:
    /// what inspection returned for the run just before it left.
    pub(crate) captured: Mutex<BTreeMap<String, Value>>,
    /// Each logged commit: its number, `SIZE_SQL` total and `drain.done`.
    pub(crate) commits: Mutex<Vec<(u64, i64, bool)>>,
}
impl Measured {
    /// Wraps `inner`; logs the first `commit_log` commits (`commit` lines).
    pub(crate) fn new(inner: PostgresStorage, url: &str, commit_log: u64) -> Self {
        Measured {
            inner,
            monitor: Mutex::new(Client::connect(url, NoTls).unwrap()),
            rec: Mutex::default(),
            commit_log: (commit_log, Default::default()),
            captured: Mutex::default(),
            commits: Mutex::default(),
        }
    }
    fn capture(&self, s: &State) {
        let mut captured = self.captured.lock().unwrap();
        for cohort in &s.settled {
            for run in &cohort.runs {
                captured.insert(run.id.clone(), inspection::public_run(run));
            }
        }
    }
}
/// Legacy items left in `s` (Task 9): finished runs (the benchmark's new
/// runs leave the State when they finish, so a terminal run in it was
/// written by the older engine), and keyed receipts and events without a
/// key in the expiry order (written before it).
fn legacy_left(s: &State) -> Value {
    use aq_playbooks_runner::retention::{
        WINDOW_ADMISSION, WINDOW_EVENT, admission_expiry, event_expiry, window_key,
    };
    let cohorts = s
        .runs
        .values()
        .filter(|r| r.status.terminal() && r.root == r.id)
        .count();
    let keyed = s
        .keyed_admissions
        .keys()
        .filter(|k| {
            admission_expiry(s, k).is_none_or(|at| {
                !s.retention
                    .expiry
                    .contains_key(&window_key(at, &format!("{WINDOW_ADMISSION}{k}")))
            })
        })
        .count();
    let events = s
        .events
        .keys()
        .filter(|k| {
            event_expiry(s, k).is_none_or(|at| {
                !s.retention
                    .expiry
                    .contains_key(&window_key(at, &format!("{WINDOW_EVENT}{k}")))
            })
        })
        .count();
    json!({"cohorts": cohorts, "window_entries": keyed + events, "keyed": keyed, "events": events,
           "runs": s.runs.len(), "receipts": s.receipts.len(),
           "tombstones": s.retention.event_tombstones.len(),
           "drain": s.retention.drain})
}
impl Measured {
    /// One `commit` line (see the module docs) after a commit of `s`.
    fn log_commit(&self, s: &State, us: u64, path: &str) {
        let n = self
            .commit_log
            .1
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        if n > self.commit_log.0 {
            return;
        }
        let mut m = self.monitor.lock().unwrap();
        let one = |m: &mut Client, sql: &str| -> i64 { m.query_one(sql, &[&NS]).unwrap().get(0) };
        let record = one(
            &mut m,
            "SELECT coalesce(max(pg_column_size(x.*)),0)::bigint FROM aq_playbooks.namespaces x WHERE namespace=$1",
        );
        let history = one(
            &mut m,
            "SELECT count(*) FROM aq_playbooks.run_history WHERE namespace=$1",
        );
        let history_runs = one(
            &mut m,
            "SELECT count(*) FROM aq_playbooks.run_history WHERE namespace=$1 AND kind='runs'",
        );
        let rows = one(
            &mut m,
            "SELECT count(*) FROM aq_playbooks.disposal_tombstones WHERE namespace=$1 AND kind IN ('event','run','intent')",
        );
        drop(m);
        let total = sizes(&self.monitor)["total_bytes"].clone();
        self.commits
            .lock()
            .unwrap()
            .push((n, total.as_i64().unwrap(), s.retention.drain.done));
        println!(
            "{}",
            json!({"kind":"commit","commit":n,"us":us,"path":path,"record_bytes":record,
                   "total_bytes":total,
                   "history_rows":history,"history_runs":history_runs,"tombstone_rows":rows,
                   "legacy":legacy_left(s)})
        );
    }
}
impl Measured {
    pub(crate) fn lsn(&self) -> i64 {
        let v: i64 = self
            .monitor
            .lock()
            .unwrap()
            .query_one(
                "SELECT pg_wal_lsn_diff(pg_current_wal_insert_lsn(),'0/0')::bigint",
                &[],
            )
            .unwrap()
            .get(0);
        v
    }
    pub(crate) fn close_unchanged(&self) {
        let mut rec = self.rec.lock().unwrap();
        if rec.open.take().is_some() {
            rec.unchanged_fallback += 1;
        }
    }
    pub(crate) fn drain(&self) -> Vec<Sample> {
        std::mem::take(&mut self.rec.lock().unwrap().samples)
    }
    fn set_gc(&self, gc: bool) {
        self.rec.lock().unwrap().gc = gc;
    }
}
impl Storage for Measured {
    fn persists_history(&self) -> bool {
        self.inner.persists_history()
    }
    fn history_page(&self, n: &str, q: &InspectionQuery) -> Result<Vec<HistoryItem>, StorageError> {
        self.inner.history_page(n, q)
    }
    fn delete_history(&self, n: &str, o: &str, before_ms: i64) -> Result<u64, StorageError> {
        self.inner.delete_history(n, o, before_ms)
    }
    fn inspect_catalog(
        &self,
        n: &str,
        o: &str,
        q: &CatalogQuery,
    ) -> Result<CatalogPage, CatalogError> {
        self.inner.inspect_catalog(n, o, q)
    }
    fn inspect(&self, n: &str, q: &InspectionQuery) -> Result<InspectionPage, StorageError> {
        self.inner.inspect(n, q)
    }
    fn protection_available(&self) -> bool {
        self.inner.protection_available()
    }
    fn query(&self, n: &str, q: &IndexQuery) -> Result<IndexPage, StorageError> {
        self.close_unchanged();
        self.inner.query(n, q)
    }
    fn load(&self, n: &str) -> Result<VersionedState, StorageError> {
        self.inner.load(n)
    }
    fn compare_and_swap(&self, n: &str, o: &str, rev: u64, s: &State) -> Result<u64, StorageError> {
        let out = self.inner.compare_and_swap(n, o, rev, s);
        if out.is_ok() {
            self.capture(s);
        }
        let open = self.rec.lock().unwrap().open.take();
        if let Some((t0, l0)) = open {
            let us = t0.elapsed().as_micros() as u64;
            let wal = self.lsn() - l0;
            let mut rec = self.rec.lock().unwrap();
            let gc = rec.gc;
            rec.samples.push(Sample {
                us,
                wal,
                path: "full",
                gc,
            });
            drop(rec);
            if out.is_ok() {
                self.log_commit(s, us, "full");
            }
        }
        out
    }
    fn acquire(&self, n: &str, o: &str) -> Result<(), StorageError> {
        self.inner.acquire(n, o)
    }
    fn release(&self, n: &str, o: &str) -> Result<(), StorageError> {
        self.inner.release(n, o)
    }
    fn transition_v1(
        &self,
        n: &str,
        o: &str,
        req: &mut TransitionRequest<'_>,
    ) -> aq_playbooks_runner::Result<TransitionOutcome> {
        self.close_unchanged();
        let before = self.inner.transition_stats();
        let l0 = self.lsn();
        let t0 = Instant::now();
        let out = self.inner.transition_v1(n, o, req);
        let us = t0.elapsed().as_micros() as u64;
        match &out {
            Ok(TransitionOutcome::Unsupported) => {
                self.rec.lock().unwrap().open = Some((t0, l0));
            }
            Ok(TransitionOutcome::Committed { .. }) => {
                let after = self.inner.transition_stats();
                let path = if after.heavy > before.heavy {
                    "heavy"
                } else {
                    "incremental"
                };
                let wal = self.lsn() - l0;
                let mut rec = self.rec.lock().unwrap();
                let gc = rec.gc;
                rec.samples.push(Sample { us, wal, path, gc });
            }
            _ => {}
        }
        out
    }
    fn transition_v1_support(&self, n: &str) -> Result<bool, StorageError> {
        self.inner.transition_v1_support(n)
    }
}

const SIZE_SQL: &str = "
WITH t(tbl, n, bytes, maxrow, logical) AS (
  SELECT 'namespaces', count(*), coalesce(sum(pg_column_size(x.*)),0), coalesce(max(pg_column_size(x.*)),0),
         coalesce(sum(coalesce(octet_length(x.snapshot::text),0)+coalesce(octet_length(x.encrypted_snapshot),0)+coalesce(octet_length(x.state_header),0)),0)
    FROM aq_playbooks.namespaces x WHERE namespace=$1
  UNION ALL SELECT 'state_records', count(*), coalesce(sum(pg_column_size(x.*)),0), coalesce(max(pg_column_size(x.*)),0),
         coalesce(sum(octet_length(x.value)+octet_length(x.sort_key)),0)
    FROM aq_playbooks.state_records x WHERE namespace=$1
  UNION ALL SELECT 'runtime_index', count(*), coalesce(sum(pg_column_size(x.*)),0), coalesce(max(pg_column_size(x.*)),0), 0
    FROM aq_playbooks.runtime_index x WHERE namespace=$1
  UNION ALL SELECT 'inspection_rows', count(*), coalesce(sum(pg_column_size(x.*)),0), coalesce(max(pg_column_size(x.*)),0),
         coalesce(sum(octet_length(x.public_value::text)),0)
    FROM aq_playbooks.inspection_rows x WHERE namespace=$1
  UNION ALL SELECT 'disposal_tombstones', count(*), coalesce(sum(pg_column_size(x.*)),0), coalesce(max(pg_column_size(x.*)),0), 0
    FROM aq_playbooks.disposal_tombstones x WHERE namespace=$1
  UNION ALL SELECT 'namespace_artifacts', count(*), coalesce(sum(pg_column_size(x.*)),0), coalesce(max(pg_column_size(x.*)),0), 0
    FROM aq_playbooks.namespace_artifacts x WHERE namespace=$1
  UNION ALL SELECT 'artifacts(referenced)', count(*), coalesce(sum(pg_column_size(x.*)),0), coalesce(max(pg_column_size(x.*)),0),
         coalesce(sum(octet_length(x.canonical_bytes)),0)
    FROM aq_playbooks.artifacts x WHERE digest IN (SELECT digest FROM aq_playbooks.namespace_artifacts WHERE namespace=$1)
  UNION ALL SELECT 'encrypted_artifacts', count(*), coalesce(sum(pg_column_size(x.*)),0), coalesce(max(pg_column_size(x.*)),0),
         coalesce(sum(octet_length(x.ciphertext)),0)
    FROM aq_playbooks.encrypted_artifacts x WHERE namespace=$1
  UNION ALL SELECT 'catalog_rows', count(*), coalesce(sum(pg_column_size(x.*)),0), coalesce(max(pg_column_size(x.*)),0),
         coalesce(sum(coalesce(octet_length(x.plain_bytes),0)+coalesce(octet_length(x.encrypted_bytes),0)),0)
    FROM aq_playbooks.catalog_rows x WHERE namespace=$1
)
SELECT tbl, n, bytes::bigint, maxrow::bigint, logical::bigint FROM t";

/// On-disk size and tuple statistics of the `namespaces` table and its
/// TOAST table (bloat from rewriting the record; whether autovacuum ran).
fn namespaces_table(monitor: &Mutex<Client>) -> Value {
    let mut c = monitor.lock().unwrap();
    let rows = c
        .query(
            "SELECT CASE WHEN n.nspname='pg_toast' THEN 'toast' ELSE 'namespaces' END, \
                    pg_relation_size(c.oid), pg_total_relation_size(c.oid), \
                    s.n_live_tup, s.n_dead_tup, s.autovacuum_count, s.vacuum_count, \
                    s.last_autovacuum::text \
             FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace \
             JOIN pg_stat_all_tables s ON s.relid=c.oid \
             WHERE c.oid='aq_playbooks.namespaces'::regclass \
                OR c.oid=(SELECT reltoastrelid FROM pg_class WHERE oid='aq_playbooks.namespaces'::regclass)",
            &[],
        )
        .unwrap();
    let mut out = serde_json::Map::new();
    for row in rows {
        out.insert(
            row.get::<_, String>(0),
            json!({"relation_bytes": row.get::<_, i64>(1), "total_bytes": row.get::<_, i64>(2),
                   "n_live_tup": row.get::<_, i64>(3), "n_dead_tup": row.get::<_, i64>(4),
                   "autovacuum_count": row.get::<_, i64>(5), "vacuum_count": row.get::<_, i64>(6),
                   "last_autovacuum": row.get::<_, Option<String>>(7)}),
        );
    }
    Value::Object(out)
}
/// On-disk size of every engine table (the database holds one namespace):
/// heap, TOAST (with its index), indexes and their total, with the tuple
/// and autovacuum counts.
pub(crate) fn disk(monitor: &Mutex<Client>) -> Value {
    let mut c = monitor.lock().unwrap();
    let rows = c
        .query(
            "SELECT c.relname::text, pg_relation_size(c.oid), \
                    CASE WHEN c.reltoastrelid = 0 THEN 0 ELSE pg_total_relation_size(c.reltoastrelid) END, \
                    pg_indexes_size(c.oid), pg_total_relation_size(c.oid), \
                    coalesce(s.n_live_tup, 0), coalesce(s.n_dead_tup, 0), coalesce(s.autovacuum_count, 0) \
             FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace \
             LEFT JOIN pg_stat_all_tables s ON s.relid=c.oid \
             WHERE n.nspname='aq_playbooks' AND c.relkind='r' ORDER BY 1",
            &[],
        )
        .unwrap();
    let mut out = serde_json::Map::new();
    let mut all = 0i64;
    for row in rows {
        let total: i64 = row.get(4);
        all += total;
        out.insert(
            row.get::<_, String>(0),
            json!({"heap_bytes": row.get::<_, i64>(1), "toast_bytes": row.get::<_, i64>(2),
                   "index_bytes": row.get::<_, i64>(3), "total_bytes": total,
                   "n_live_tup": row.get::<_, i64>(5), "n_dead_tup": row.get::<_, i64>(6),
                   "autovacuum_count": row.get::<_, i64>(7)}),
        );
    }
    json!({"all_tables_bytes": all, "tables": out})
}
pub(crate) fn sizes(monitor: &Mutex<Client>) -> Value {
    let mut c = monitor.lock().unwrap();
    let mut tables = serde_json::Map::new();
    let (mut total, mut maxrow, mut logical) = (0i64, 0i64, 0i64);
    for row in c.query(SIZE_SQL, &[&NS]).unwrap() {
        let (t, n, b, m, l): (String, i64, i64, i64, i64) =
            (row.get(0), row.get(1), row.get(2), row.get(3), row.get(4));
        total += b;
        maxrow = maxrow.max(m);
        logical += l;
        tables.insert(t, json!({"rows":n,"bytes":b,"max_row":m,"logical":l}));
    }
    let records: Vec<Value> = c
        .query(
            "SELECT field, count(*), sum(octet_length(value)+octet_length(sort_key))::bigint, max(pg_column_size(x.*))
             FROM aq_playbooks.state_records x WHERE namespace=$1 GROUP BY field ORDER BY field",
            &[&NS],
        )
        .unwrap()
        .iter()
        .map(|r| {
            json!({"field": r.get::<_, String>(0), "rows": r.get::<_, i64>(1),
                   "bytes": r.get::<_, i64>(2), "max_row": r.get::<_, i32>(3)})
        })
        .collect();
    let physical: Vec<Value> = c
        .query(
            "SELECT c.relname::text, pg_total_relation_size(c.oid)::bigint FROM pg_class c
             JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='aq_playbooks' AND c.relkind='r' ORDER BY 1",
            &[],
        )
        .unwrap()
        .iter()
        .map(|r| json!([r.get::<_, String>(0), r.get::<_, i64>(1)]))
        .collect();
    let db: i64 = c
        .query_one("SELECT pg_database_size(current_database())", &[])
        .unwrap()
        .get(0);
    json!({"total_bytes": total, "largest_row": maxrow, "logical_bytes": logical, "tables": tables,
           "state_records_by_field": records, "physical_relations": physical, "database_bytes": db})
}

fn len(v: &Value) -> usize {
    serde_json::to_vec(v).unwrap().len()
}
fn count(v: &Value) -> usize {
    match v {
        Value::Object(m) => m.len(),
        Value::Array(a) => a.len(),
        _ => 1,
    }
}
/// Serialized bytes and element count of every State field (and of the
/// retention/indexes sub-fields), from a complete load.
fn breakdown(state: &State) -> Value {
    let v = serde_json::to_value(state).unwrap();
    let mut out = serde_json::Map::new();
    for (k, f) in v.as_object().unwrap() {
        out.insert(k.clone(), json!([len(f), count(f)]));
        if (k == "retention" || k == "indexes") && f.is_object() {
            for (sk, sf) in f.as_object().unwrap() {
                out.insert(format!("{k}.{sk}"), json!([len(sf), count(sf)]));
            }
        }
    }
    json!({"state_json_bytes": len(&v), "fields": out})
}

fn pct(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() as f64) * p).ceil() as usize;
    sorted[i.clamp(1, sorted.len()) - 1]
}
fn median(v: &mut [u64]) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort();
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2] as f64
    } else {
        (v[n / 2 - 1] + v[n / 2]) as f64 / 2.0
    }
}

/// What serving a run needs of the namespace.
pub(crate) struct Bench<'a> {
    pub(crate) e: &'a Engine<Measured>,
    pub(crate) activation_revision: u64,
    /// Prefix of admission keys, tickets and event ids.
    pub(crate) prefix: String,
    /// Pass the producer's request time (`admit_once_at`, `admit_event_at`).
    pub(crate) timed: bool,
}

/// Serves run `k` from its admission at `now` to completion, each
/// transition `step` ms after the last. Returns the run id and the time of
/// its last transition.
pub(crate) fn serve_run(
    b: &Bench,
    host: &mut BenchHost,
    k: usize,
    mut now: i64,
    step: i64,
) -> (String, i64) {
    let (e, prefix) = (b.e, &b.prefix);
    let ticket = format!("{prefix}ticket-{k:06}");
    // Keyed admission, as aq-server's host uses (`admit_once`).
    let id = e
        .admit_once_at(
            KeyedAdmissionRequest {
                key: format!("{prefix}admission-{k:06}"),
                binding: "growth".into(),
                expected_activation_revision: b.activation_revision,
                entry: "main".into(),
                input: json!({"ticket": ticket}),
                authority: authority(),
            },
            b.timed.then_some(now),
            now,
        )
        .unwrap()
        .receipt
        .run_id;
    let mut guard = 0;
    loop {
        now += step;
        guard += 1;
        if e.drive(host, now, 1).unwrap() == 0 || guard > 40 {
            break;
        }
    }
    now += step;
    let c = signal_contract();
    let d = e
        .admit_event_at(
            "host",
            EventEnvelope {
                producer: "host".into(),
                event_id: format!("{prefix}signal-{k:06}"),
                contract: ContractRef {
                    name: c.name.clone(),
                    fingerprint: c.fingerprint().unwrap(),
                },
                scope: "project".into(),
                correlation: BTreeMap::from([("ticket".into(), ticket.clone())]),
                subject: None,
                revision: None,
                causation: None,
                occurred_at_ms: Some(now),
                observed_at_ms: None,
                payload: json!({"detail": "d".repeat(256)}),
            },
            0,
            b.timed.then_some(now),
            now,
        )
        .unwrap();
    assert!(!d.duplicate);
    loop {
        now += step;
        guard += 1;
        if e.drive(host, now, 1).unwrap() == 0 || guard > 80 {
            break;
        }
    }
    (id, now)
}

/// Every item of a `Runs` inspection, paged 1,000 at a time.
pub(crate) fn all_runs<S: Storage>(e: &Engine<S>) -> Vec<Value> {
    let mut items = vec![];
    let mut after = None;
    loop {
        let page = e
            .inspect(&InspectionQuery {
                kind: InspectionKind::Runs,
                run: None,
                after: after.clone(),
                limit: 1000,
            })
            .unwrap();
        items.extend(page.items);
        match page.next {
            Some(next) => after = Some(next),
            None => return items,
        }
    }
}

/// What a checkpoint measured, for the verdicts.
#[derive(Clone, Copy, Debug)]
struct Mark {
    /// `SIZE_SQL` total: the record and the namespace's rows.
    total_bytes: i64,
    /// Median step over the checkpoint's window (the last 20 runs).
    median_us: f64,
    /// `aq_playbooks.namespaces` on disk: heap, TOAST and indexes.
    namespaces_disk: i64,
}

/// The run loop's measurements.
struct Measurement {
    config: String,
    /// The `phase` of the `run` lines.
    phase: String,
    gc: bool,
    window: Vec<Vec<Sample>>,
    gc_calls: Vec<Value>,
    marks: BTreeMap<usize, Mark>,
    /// Run ids in admission order.
    ids: Vec<String>,
    started: Instant,
    stopped: Option<usize>,
}
impl Measurement {
    /// Serves run `k` at `now`, prints its `run` line and, at a checkpoint
    /// (or when `checkpoint`), its `checkpoint` line.
    fn run(
        &mut self,
        b: &Bench,
        host: &mut BenchHost,
        k: usize,
        now: i64,
        step: i64,
        checkpoint: bool,
    ) {
        let e = b.e;
        let run_started = Instant::now();
        let wal0 = e.store.lsn();
        let (id, mut now) = serve_run(b, host, k, now, step);
        e.store.close_unchanged();
        let run_wall_us = run_started.elapsed().as_micros() as u64;
        let wal_run = e.store.lsn() - wal0;
        let samples = e.store.drain();
        let mut gc_this = vec![];
        if self.gc {
            e.store.set_gc(true);
            now += 70_000; // past the 60 s event contract retention
            loop {
                let before = e.store.inner.transition_stats();
                let l0 = e.store.lsn();
                let t0 = Instant::now();
                let report = e.collect_garbage(now, 1000).unwrap();
                let us = t0.elapsed().as_micros() as u64;
                let wal = e.store.lsn() - l0;
                let after = e.store.inner.transition_stats();
                let call = json!({"run":k,"us":us,"wal":wal,"report":report,
                    "heavy":after.heavy-before.heavy,"incremental":after.incremental-before.incremental,
                    "records_written":after.records_written-before.records_written,
                    "record_reads":after.record_reads-before.record_reads});
                gc_this.push(call.clone());
                self.gc_calls.push(call);
                if !report.more {
                    break;
                }
                now += 1;
            }
            e.store.close_unchanged();
            e.store.drain();
            e.store.set_gc(false);
        }
        let max_us = samples.iter().map(|s| s.us).max().unwrap_or(0);
        let wall: Vec<u64> = samples.iter().map(|s| s.us).collect();
        let paths: BTreeMap<&str, usize> = samples.iter().fold(BTreeMap::new(), |mut m, s| {
            *m.entry(s.path).or_default() += 1;
            m
        });
        println!(
            "{}",
            json!({"kind":"run","run":k,"phase":self.phase,"transitions":samples.len(),"transition_us":wall,
                   "transition_wal":samples.iter().map(|s| s.wal).collect::<Vec<_>>(),
                   "paths":paths,"run_wall_us":run_wall_us,"run_wal":wal_run,
                   "gc":gc_this, "gc_samples_flag": samples.iter().any(|s| s.gc)})
        );
        self.window.push(samples);
        if self.window.len() > 20 {
            self.window.remove(0);
        }
        if checkpoint || CHECKPOINTS.contains(&k) {
            self.checkpoint(e, k, &id);
        }
        self.ids.push(id);
        if max_us > STOP_US {
            self.stopped = Some(k);
        }
    }
    fn checkpoint(&mut self, e: &Engine<Measured>, k: usize, id: &str) {
        let config = &self.config;
        let mut all: Vec<u64> = self.window.iter().flatten().map(|s| s.us).collect();
        let mut wal: Vec<u64> = self
            .window
            .iter()
            .flatten()
            .map(|s| s.wal.max(0) as u64)
            .collect();
        let max = all.iter().copied().max().unwrap_or(0);
        let med = median(&mut all);
        let p95 = pct(&all, 0.95);
        let wal_med = median(&mut wal);
        let wal_max = wal.iter().copied().max().unwrap_or(0);
        let state = e.snapshot().unwrap();
        // A finished run has left the State (spec 0003): read its
        // public projection from run history.
        let status = state
            .runs
            .get(id)
            .map(|r| format!("{:?}/{:?}", r.status, r.outcome))
            .or_else(|| {
                e.inspect(&InspectionQuery {
                    kind: InspectionKind::Runs,
                    run: Some(id.to_string()),
                    after: None,
                    limit: 1,
                })
                .unwrap()
                .items
                .first()
                .map(|r| format!("history:{}/{}", r["status"], r["outcome"]))
            });
        let completed = state
            .runs
            .values()
            .filter(|r| r.status == RunStatus::Completed && r.outcome.as_deref() == Some("ok"))
            .count();
        let recent_gc: Vec<u64> = self
            .gc_calls
            .iter()
            .rev()
            .take(20)
            .map(|c| c["us"].as_u64().unwrap())
            .collect();
        let sizes = sizes(&e.store.monitor);
        let disk = disk(&e.store.monitor);
        println!(
            "{}",
            json!({"kind":"checkpoint","config":config,"run":k,
                   "window_runs": self.window.len(), "window_transitions": all.len(),
                   "transition_median_us": med, "transition_p95_us": p95, "transition_max_us": max,
                   "wal_per_transition_median": wal_med, "wal_per_transition_max": wal_max,
                   "last_run_status": status, "runs_in_state": state.runs.len(), "completed_ok_in_state": completed,
                   "gc_last20_us": recent_gc,
                   "stats": format!("{:?}", e.store.inner.transition_stats()),
                   "elapsed_s": self.started.elapsed().as_secs_f64(),
                   "sizes": sizes, "state": breakdown(&state),
                   "namespaces_table": namespaces_table(&e.store.monitor), "disk": disk})
        );
        eprintln!(
            "[{config}] run {k}: median {med:.0} us max {max} us, total {} B, elapsed {:.1}s",
            sizes["total_bytes"],
            self.started.elapsed().as_secs_f64()
        );
        self.marks.insert(
            k,
            Mark {
                total_bytes: sizes["total_bytes"].as_i64().unwrap(),
                median_us: med,
                namespaces_disk: disk["tables"]["namespaces"]["total_bytes"]
                    .as_i64()
                    .unwrap(),
            },
        );
    }
}

/// Acceptance 3: paging `Runs` lists the newest `min(runs, history_keep)`
/// finished runs once each, and each equals the projection captured in the
/// commit that removed it.
fn history_verdict(e: &Engine<Measured>, ids: &[String], history_keep: u64) -> Value {
    let items = all_runs(e);
    let listed: BTreeSet<String> = items
        .iter()
        .map(|v| v["id"].as_str().unwrap().to_string())
        .collect();
    let expected_n = ids.len().min(history_keep as usize);
    let expected: BTreeSet<String> = ids[ids.len() - expected_n..].iter().cloned().collect();
    let captured = e.store.captured.lock().unwrap();
    let mismatched = items
        .iter()
        .filter(|v| captured.get(v["id"].as_str().unwrap()) != Some(*v))
        .count();
    let live = e.snapshot().unwrap().runs.len();
    let pass = items.len() == expected_n && listed == expected && mismatched == 0 && live == 0;
    json!({"kind":"verdict","acceptance":3,"pass":pass,
           "listed_items":items.len(),"distinct_ids":listed.len(),"expected":expected_n,
           "listed_equals_newest":listed == expected,"projection_mismatches":mismatched,
           "captured_runs":captured.len(),"live_runs":live,"history_keep":history_keep})
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mode = args[1].as_str();
    let runs: usize = args[2].parse().unwrap();
    let url = &args[3];
    let acceptance = matches!(mode, "steady" | "burst");
    let config = if acceptance { "a" } else { mode };
    let spacing: i64 = match mode {
        "steady" => STEADY_SPACING_MS,
        "burst" => 0,
        _ => args.get(4).map_or(RUN_SPACING_MS, |v| v.parse().unwrap()),
    };
    let window_ms: Option<u64> = if acceptance {
        Some(ACCEPTANCE_WINDOW_MS)
    } else {
        args.get(5).map(|v| v.parse().unwrap())
    };
    // A frozen clock does not step within a run either.
    let step: i64 = if spacing == 0 { 0 } else { 10 };
    let protected = matches!(config, "a" | "e");
    let records = matches!(config, "c" | "d");
    let gc = matches!(config, "d" | "e");
    let inner = if protected {
        let keyring = ProtectionKeyring::new(
            "bench-key",
            BTreeMap::from([("bench-key".to_string(), [7u8; 32])]),
        )
        .unwrap();
        PostgresStorage::connect_protected(url, keyring).unwrap()
    } else if records {
        PostgresStorage::connect_records(url).unwrap()
    } else {
        PostgresStorage::connect(url).unwrap()
    };
    let env = |name: &str| std::env::var(name).ok();
    let prefix = env("ENGINE_GROWTH_KEY_PREFIX").unwrap_or_default();
    let start_ms: i64 = env("ENGINE_GROWTH_START_MS").map_or(0, |v| v.parse().unwrap());
    let history_keep: Option<u64> = env("ENGINE_GROWTH_HISTORY_KEEP").map(|v| v.parse().unwrap());
    let store = Measured::new(
        inner,
        url,
        env("ENGINE_GROWTH_COMMIT_LOG").map_or(0, |v| v.parse().unwrap()),
    );
    let e = Engine::new(store, NS, "bench-owner").unwrap();
    let a = playbook();
    let digest = a.digest.clone();
    e.install(a).unwrap();
    e.activate_at("growth", &digest, authority(), 0).unwrap();
    if gc {
        e.set_retention_policy(RetentionPolicy {
            event_replay_ms: 1,
            terminal_run_ms: 1,
            audit_ms: 1,
            max_tombstones: 100_000,
            ..RetentionPolicy::default()
        })
        .unwrap();
    }
    if window_ms.is_some() || history_keep.is_some() {
        let mut policy = e.snapshot().unwrap().retention.policy;
        if let Some(window_ms) = window_ms {
            policy.event_replay_ms = window_ms;
            policy.admission_replay_ms = window_ms;
        }
        if let Some(keep) = history_keep {
            policy.history_keep = keep;
        }
        e.set_retention_policy(policy).unwrap();
    }
    let policy = e.snapshot().unwrap().retention.policy;
    let format: i32 = e
        .store
        .monitor
        .lock()
        .unwrap()
        .query_one(
            "SELECT writer_format FROM aq_playbooks.namespaces WHERE namespace=$1",
            &[&NS],
        )
        .unwrap()
        .get(0);
    println!(
        "{}",
        json!({"kind":"setup","mode":mode,"config":config,"protected":protected,"records":records,"gc":gc,
               "run_spacing_ms":spacing,"window_ms":window_ms,"timed_requests":acceptance,
               "policy":policy,
               "writer_format":format,"transition_v1_support":e.store.transition_v1_support(NS).unwrap(),
               "artifact_bytes": canonical_bytes(&e.snapshot().unwrap().artifacts[&digest]).unwrap().len(),
               "setup": sizes(&e.store.monitor)})
    );
    let bench = Bench {
        e: &e,
        activation_revision: e.snapshot().unwrap().bindings["growth"].revision,
        prefix,
        timed: acceptance,
    };
    e.store.drain();
    let mut host = BenchHost;
    let mut b = Measurement {
        config: config.to_string(),
        phase: mode.to_string(),
        gc,
        window: vec![],
        gc_calls: vec![],
        marks: BTreeMap::new(),
        ids: vec![],
        started: Instant::now(),
        stopped: None,
    };
    for k in 1..=runs {
        let now = start_ms + k as i64 * spacing;
        b.run(&bench, &mut host, k, now, step, k == runs);
        if b.stopped.is_some() {
            break;
        }
    }
    let completed = b.stopped.is_none();
    if mode == "steady" && completed {
        let (at200, at2000, last) = (b.marks.get(&200), b.marks.get(&2000), b.marks[&runs]);
        let bytes_ratio = at200.map(|m| last.total_bytes as f64 / m.total_bytes as f64);
        let median_ratio = at200.map(|m| last.median_us / m.median_us);
        let disk_ratio = at2000.map(|m| last.namespaces_disk as f64 / m.namespaces_disk as f64);
        let pass = bytes_ratio.is_some_and(|r| r <= 1.2)
            && median_ratio.is_some_and(|r| r <= 1.2)
            && disk_ratio.is_none_or(|r| r <= 3.0);
        println!(
            "{}",
            json!({"kind":"verdict","acceptance":1,"pass":pass,"runs":runs,
                   "record_bytes_at_200":at200.map(|m| m.total_bytes),"record_bytes_last":last.total_bytes,
                   "record_ratio":bytes_ratio,
                   "median_us_at_200":at200.map(|m| m.median_us),"median_us_last":last.median_us,
                   "median_ratio":median_ratio,
                   "namespaces_disk_at_2000":at2000.map(|m| m.namespaces_disk),
                   "namespaces_disk_last":last.namespaces_disk,"namespaces_disk_ratio":disk_ratio})
        );
    }
    if mode == "burst" && completed {
        let s = e.snapshot().unwrap();
        let left = json!({"runs":s.runs.len(),"intents":s.intents.len(),"waits":s.waits.len(),
                          "budgets":s.budgets.len(),"events":s.events.len(),
                          "keyed_admissions":s.keyed_admissions.len(),
                          "replay_order":s.retention.replay_order.len(),
                          "replay_held":s.retention.replay_held.len(),
                          "expiry":s.retention.expiry.len()});
        let nothing_live =
            s.runs.is_empty() && s.intents.is_empty() && s.waits.is_empty() && s.budgets.is_empty();
        let order_bounded = s.retention.replay_order.len() <= policy.replay_keep;
        drop(s);
        let (first, last) = (b.marks[&1], b.marks[&runs]);
        let per_run = (last.total_bytes - first.total_bytes) as f64 / (runs.max(2) - 1) as f64;
        // NR-2 (plan "Spec check"): move the clock past the window and
        // complete 20 runs at that frozen clock; they drain the burst. That
        // record is the literal "after 20" figure.
        let past = ACCEPTANCE_WINDOW_MS as i64 + STEADY_SPACING_MS;
        let t1 = start_ms + past;
        b.phase = "drain_frozen".into();
        for j in 1..=20 {
            b.run(&bench, &mut host, runs + j, t1, 0, j == 20);
        }
        let literal = b.marks[&(runs + 20)];
        // Then once more past the window, one more run: compared with run 1.
        b.phase = "after_drain".into();
        b.run(&bench, &mut host, runs + 21, t1 + past, 0, true);
        let nr2 = b.marks[&(runs + 21)];
        let nr2_ratio = nr2.total_bytes as f64 / first.total_bytes as f64;
        let pass = per_run <= 2048.0 && nothing_live && order_bounded && nr2_ratio <= 1.2;
        println!(
            "{}",
            json!({"kind":"verdict","acceptance":2,"pass":pass,"runs":runs,
                   "record_bytes_at_1":first.total_bytes,"record_bytes_at_burst_end":last.total_bytes,
                   "growth_per_run_bytes":per_run,"left_after_burst":left,"nothing_live_left":nothing_live,
                   "replay_order_within_replay_keep":order_bounded,"replay_keep":policy.replay_keep,
                   "nr2_record_bytes":nr2.total_bytes,"nr2_ratio":nr2_ratio,
                   "literal_after_20_record_bytes":literal.total_bytes,
                   "literal_after_20_ratio":literal.total_bytes as f64 / first.total_bytes as f64,
                   "median_us_at_1":first.median_us,"median_us_at_burst_end":last.median_us})
        );
    }
    if acceptance && completed {
        println!("{}", history_verdict(&e, &b.ids, policy.history_keep));
    }
    println!(
        "{}",
        json!({"kind":"done","mode":mode,"config":config,"stopped_early_at_run":b.stopped,
               "elapsed_s": b.started.elapsed().as_secs_f64()})
    );
}
