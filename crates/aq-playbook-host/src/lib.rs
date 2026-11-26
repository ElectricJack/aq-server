//! AQ-owned bounded embedding. Not mounted by the daemon and no domain commands
//! are registered. Synchronous engine work runs only on one dedicated OS thread.
use aq_playbooks_model::{Artifact, CommandContract, EventEnvelope, Registry};
use aq_playbooks_runner::{
    Authority, CancellationResult, DispatchResult, Engine, EngineConfig, Host, IndexQuery, Intent,
    IntentStatus, KeyedAdmissionRequest, Storage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::{BTreeSet, VecDeque},
    sync::{
        Arc, Condvar, Mutex, OnceLock, RwLock, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::oneshot;
use validation::validate_registry;

pub mod drive;
pub mod engine_retention;
mod operations;
pub mod plugins;
pub mod protected;
pub mod retention;
mod validation;
mod worker;
pub use aq_playbooks_model as model;
pub use aq_playbooks_postgres as postgres_storage;
pub use aq_playbooks_runner as runner;
pub use postgres as postgres_client;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostMode {
    Dispatch,
    Reconcile,
    RetryPlugins,
    Cancel,
}
/// Recovery mode authorizes historical lookup only; implementations must never
/// let it renew a grant or authorize a fresh business effect.
/// Supplied only after this thread acquires the actual engine namespace. This
/// callback context is not a serializable credential or HTTP authority proof.
#[derive(Clone, Debug)]
pub struct AcquiredOwner {
    pub namespace: String,
    pub incarnation: String,
}

pub trait DomainHost: Host {
    fn set_mode(&mut self, mode: HostMode);
    /// Use the caller's logical time unless this host owns a live clock.
    fn transition_time(&self, now_ms: i64) -> i64 {
        now_ms
    }
    /// Resume an admitted decision, read an exact retained receipt, or prove
    /// that an expired asynchronous operation was never admitted.
    fn reconcile_delivery(&mut self, _intent: &Intent) -> Option<DispatchResult> {
        None
    }
    fn dispatch_unavailable_plugin(&mut self, _intent: &Intent) -> DispatchResult {
        DispatchResult::Unknown {
            reason: "plugin contract is unavailable".into(),
        }
    }
}
pub use aq_playbooks_runner::MemoryStorage;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("invalid embedding request or limits")]
    Invalid,
    #[error("embedding queue full; request was not admitted")]
    Overloaded,
    #[error("owner stopped; request outcome may be unknown")]
    Stopped,
    #[error("response deadline elapsed; admitted operation outcome is unknown")]
    Unknown,
    #[error("engine refused: {0}")]
    Engine(String),
    /// The engine no longer remembers this event or admission key and its
    /// request time is older than the replay window. Nothing was written;
    /// the outcome belongs to this one request.
    #[error("{0}")]
    Stale(String),
    /// The engine does not hold what the request names (for an event:
    /// it has forgotten it).
    #[error("engine: not found: {0}")]
    NotFound(String),
    #[error("synchronous owner thread could not start")]
    Thread,
}

#[derive(Clone, Debug)]
pub struct Limits {
    pub work_queue: usize,
    pub control_queue: usize,
    pub request_bytes: usize,
    pub response_bytes: usize,
    pub transitions: usize,
    pub engine: EngineConfig,
    /// Engine session deadlines; the owner-call deadlines derive from them.
    pub timeouts: protected::Timeouts,
    /// Rolling deletion of the server's finished bridge rows.
    pub retention: retention::Retention,
    /// The engine's replay windows and rolling sizes, applied at open.
    pub engine_retention: engine_retention::EngineRetention,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            work_queue: 32,
            control_queue: 8,
            request_bytes: 262_144,
            response_bytes: 262_144,
            transitions: 8,
            engine: EngineConfig::default(),
            timeouts: protected::Timeouts::default(),
            retention: retention::Retention::default(),
            engine_retention: engine_retention::EngineRetention::default(),
        }
    }
}
impl Limits {
    fn validate(&self) -> Result<(), Error> {
        let valid = self.queues_valid() && self.engine_valid() && self.policies_valid();
        valid.then_some(()).ok_or(Error::Invalid)
    }

    fn queues_valid(&self) -> bool {
        (1..=128).contains(&self.work_queue)
            && (1..=32).contains(&self.control_queue)
            && (1024..=1_048_576).contains(&self.request_bytes)
            && (1024..=1_048_576).contains(&self.response_bytes)
            && (1..=64).contains(&self.transitions)
    }

    fn engine_valid(&self) -> bool {
        (1..=10_000).contains(&self.engine.max_runs)
            && (1..=100_000).contains(&self.engine.max_events)
            && (1..=10_000).contains(&self.engine.max_queued_destinations)
            && (1..=1_000_000).contains(&self.engine.max_receipts)
            && (1024..=67_108_864).contains(&self.engine.max_state_bytes)
    }

    fn policies_valid(&self) -> bool {
        self.timeouts.valid() && self.retention.valid() && self.engine_retention.valid()
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Install {
        artifact: Box<Artifact>,
    },
    Activate {
        binding: String,
        artifact: String,
        authority: Authority,
    },
    ActivateAt {
        binding: String,
        artifact: String,
        authority: Authority,
        now_ms: i64,
    },
    ConfigureBinding {
        binding: String,
        artifact: String,
        authority: Authority,
        may_activate: bool,
        schedules: Vec<model::schedules::BindingSchedule>,
        interval_event: Option<model::ContractRef>,
        now_ms: i64,
    },
    DeactivateBinding {
        binding: String,
        revision: u64,
        now_ms: i64,
    },
    CollectIntervalEvents {
        now_ms: i64,
    },
    Admit {
        binding: String,
        entry: String,
        input: Value,
        authority: Authority,
        now_ms: i64,
    },
    AdmitOnce {
        request: Box<KeyedAdmissionRequest>,
        /// When the host first created this admission (its own clock).
        #[serde(default)]
        requested_at_ms: Option<i64>,
        now_ms: i64,
    },
    Admission {
        key: String,
    },
    Event {
        authenticated_producer: String,
        epoch: u64,
        event: Box<EventEnvelope>,
        /// When the host first created this event (its own clock), not the
        /// event's business time.
        #[serde(default)]
        requested_at_ms: Option<i64>,
        now_ms: i64,
    },
    Fanout {
        event_key: String,
        limit: usize,
        now_ms: i64,
    },
    CancelEffects {
        now_ms: i64,
    },
    Inspect {
        query: runner::InspectionQuery,
    },
    EventStatus {
        event_key: String,
    },
    Recover {
        now_ms: i64,
    },
    ReconcileDelivery {
        intents: Vec<String>,
        scan_after: Option<String>,
        now_ms: i64,
    },
    Binding {
        binding: String,
    },
    Runs {
        after: Option<String>,
        limit: usize,
    },
    Tick {
        now_ms: i64,
    },
    Cancel {
        run: String,
        now_ms: i64,
    },
    Stop,
}
impl Request {
    fn control(&self) -> bool {
        matches!(
            self,
            Self::Cancel { .. }
                | Self::CancelEffects { .. }
                | Self::DeactivateBinding { .. }
                | Self::Stop
        )
    }
}

enum Envelope {
    Operation {
        request: Request,
        reply: oneshot::Sender<Result<Value, Error>>,
    },
    Catalog {
        query: runner::CatalogQuery,
        reply: oneshot::Sender<Result<runner::CatalogPage, CatalogCallError>>,
    },
}
/// Additive typed read errors; legacy Request/Error wire shapes remain unchanged.
#[derive(Debug, thiserror::Error)]
pub enum CatalogCallError {
    #[error(transparent)]
    Host(#[from] Error),
    #[error(transparent)]
    Catalog(#[from] runner::CatalogError),
}
#[derive(Default)]
struct Queues {
    work: VecDeque<Envelope>,
    control: VecDeque<Envelope>,
    closing: bool,
}
struct Shared {
    queues: Mutex<Queues>,
    wake: Condvar,
    clients: AtomicUsize,
    limits: Limits,
    registry: Registry,
    declared_events: RwLock<BTreeSet<(String, String)>>,
    plugin_artifacts: AtomicBool,
    stale_artifacts: OnceLock<BTreeSet<String>>,
    catalog_supported: AtomicBool,
}
impl Shared {
    fn catalog_supported(&self) -> bool {
        self.catalog_supported.load(Ordering::Acquire)
            && !self
                .queues
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .closing
    }
}
/// CPU-only live interface support; does not retain an Owner client or metadata.
#[derive(Clone)]
pub struct CatalogSupport {
    shared: Weak<Shared>,
}
impl CatalogSupport {
    pub fn available(&self) -> bool {
        self.shared
            .upgrade()
            .is_some_and(|shared| shared.catalog_supported())
    }
}
/// Cloning this handle never creates a second namespace owner. Drop does not join
/// the thread: a stuck storage call retains its ownership until it really returns.
pub struct Owner {
    shared: Arc<Shared>,
}
impl Clone for Owner {
    fn clone(&self) -> Self {
        self.shared.clients.fetch_add(1, Ordering::Relaxed);
        Self {
            shared: self.shared.clone(),
        }
    }
}
impl Drop for Owner {
    fn drop(&mut self) {
        if self.shared.clients.fetch_sub(1, Ordering::AcqRel) == 1 {
            let mut q = self.shared.queues.lock().unwrap_or_else(|e| e.into_inner());
            q.closing = true;
            self.shared.wake.notify_one();
        }
    }
}
struct ThreadExit(Arc<Shared>);
impl Drop for ThreadExit {
    fn drop(&mut self) {
        let mut q = self.0.queues.lock().unwrap_or_else(|e| e.into_inner());
        q.closing = true;
        q.work.clear();
        q.control.clear();
        self.0.wake.notify_all();
    }
}
pub struct Registration {
    namespace: String,
    limits: Limits,
    registry: Registry,
}
impl Registration {
    pub fn new(namespace: String, limits: Limits, registry: Registry) -> Self {
        Self {
            namespace,
            limits,
            registry,
        }
    }
}

impl Owner {
    /// The storage factory, including connection establishment and schema work,
    /// executes on the dedicated thread. It must configure bounded I/O itself.
    pub async fn start<S, F>(namespace: String, limits: Limits, factory: F) -> Result<Self, Error>
    where
        S: Storage + 'static,
        F: FnOnce() -> Result<S, Error> + Send + 'static,
    {
        Self::start_registered(
            Registration::new(namespace, limits, Registry::default()),
            factory,
            |_| Ok(UnavailableHost),
        )
        .await
    }
    /// Explicit trusted host registration. Installation checks the full catalog.
    /// Restart reopens pinned artifacts and marks stale extension contracts for
    /// dispatch refusal. Callers supply the actual owning AQ service adapters.
    pub async fn start_registered<S, F, H, HF>(
        registration: Registration,
        factory: F,
        host_factory: HF,
    ) -> Result<Self, Error>
    where
        S: Storage + 'static,
        F: FnOnce() -> Result<S, Error> + Send + 'static,
        H: DomainHost + 'static,
        HF: FnOnce(AcquiredOwner) -> Result<H, Error> + Send + 'static,
    {
        let Registration {
            namespace,
            limits,
            registry,
        } = registration;
        limits.validate()?;
        validate_registry(&registry)?;
        if !validation::valid(&namespace) {
            return Err(Error::Invalid);
        }
        let shared = Arc::new(Shared {
            queues: Mutex::new(Queues::default()),
            wake: Condvar::new(),
            clients: AtomicUsize::new(1),
            limits,
            registry,
            declared_events: RwLock::default(),
            plugin_artifacts: AtomicBool::default(),
            stale_artifacts: OnceLock::default(),
            catalog_supported: AtomicBool::new(false),
        });
        let owner = Self {
            shared: shared.clone(),
        };
        let (ready_tx, ready_rx) = oneshot::channel();
        std::thread::Builder::new()
            .name("aq-playbook-owner".into())
            .spawn(move || {
                let _exit = ThreadExit(shared.clone());
                let result = factory().and_then(|store| worker::open(&shared, namespace, store));
                let engine = match result {
                    Ok(engine) => engine,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                let mut host = match worker::host(&engine, &shared, host_factory) {
                    Ok(host) => host,
                    Err(error) => {
                        let _ = engine.release();
                        let _ = ready_tx.send(Err(error));
                        return;
                    }
                };
                if ready_tx.send(Ok(())).is_err() {
                    let _ = engine.release();
                    return;
                }
                shared.serve(&engine, &mut host);
                let _ = engine.release();
            })
            .map_err(|_| Error::Thread)?;
        ready_rx.await.map_err(|_| Error::Stopped)??;
        Ok(owner)
    }
    pub fn has_plugin_artifacts(&self) -> bool {
        self.shared.plugin_artifacts.load(Ordering::Acquire)
    }
    pub fn declares_event(&self, name: &str, fingerprint: &str) -> bool {
        self.shared
            .declared_events
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(&(name.to_owned(), fingerprint.to_owned()))
    }
    /// A timeout does not cancel admitted work or relinquish namespace ownership.
    /// Callers must reconcile effects; blindly resubmitting Admit can duplicate runs.
    pub async fn call(&self, request: Request, deadline: Duration) -> Result<Value, Error> {
        request.validate()?;
        if let Request::Install { artifact } = &request
            && !accepts(&self.shared.registry, &artifact.content.registry)
        {
            return Err(Error::Invalid);
        }
        if !bounded_json(&request, self.shared.limits.request_bytes) {
            return Err(Error::Invalid);
        }
        let (reply, receiver) = oneshot::channel();
        {
            let mut q = self.shared.queues.lock().unwrap_or_else(|e| e.into_inner());
            if q.closing {
                return Err(Error::Stopped);
            }
            let (queue, bound) = if request.control() {
                (&mut q.control, self.shared.limits.control_queue)
            } else {
                (&mut q.work, self.shared.limits.work_queue)
            };
            if queue.len() >= bound {
                return Err(Error::Overloaded);
            }
            queue.push_back(Envelope::Operation { request, reply });
            self.shared.wake.notify_one();
        }
        tokio::time::timeout(deadline, receiver)
            .await
            .map_err(|_| Error::Unknown)?
            .map_err(|_| Error::Stopped)?
    }
    /// Interface support established once by an actual acquired bounded query.
    /// It does not cache metadata or promise later profile/ownership availability.
    pub fn catalog_supported(&self) -> bool {
        self.shared.catalog_supported()
    }
    pub fn catalog_support(&self) -> CatalogSupport {
        CatalogSupport {
            shared: Arc::downgrade(&self.shared),
        }
    }
    pub async fn inspect_catalog(
        &self,
        query: runner::CatalogQuery,
        deadline: Duration,
    ) -> Result<runner::CatalogPage, CatalogCallError> {
        query.validate()?;
        if !self.catalog_supported() {
            return Err(runner::CatalogError::Unsupported.into());
        }
        if !serde_json::to_vec(&query)
            .is_ok_and(|bytes| bytes.len() <= self.shared.limits.request_bytes)
        {
            return Err(Error::Invalid.into());
        }
        let (reply, receiver) = oneshot::channel();
        {
            let mut queues = self.shared.queues.lock().unwrap_or_else(|e| e.into_inner());
            if queues.closing {
                return Err(Error::Stopped.into());
            }
            if queues.work.len() >= self.shared.limits.work_queue {
                return Err(Error::Overloaded.into());
            }
            queues.work.push_back(Envelope::Catalog { query, reply });
            self.shared.wake.notify_one();
        }
        tokio::time::timeout(deadline, receiver)
            .await
            .map_err(|_| CatalogCallError::Host(Error::Unknown))?
            .map_err(|_| CatalogCallError::Host(Error::Stopped))?
    }
}
fn runner_error(e: aq_playbooks_runner::Error) -> Error {
    match e {
        aq_playbooks_runner::Error::Exhausted(_) => Error::Overloaded,
        stale @ aq_playbooks_runner::Error::Stale { .. } => Error::Stale(stale.to_string()),
        aq_playbooks_runner::Error::NotFound(what) => Error::NotFound(what),
        // The commit failed in storage, or its outcome is not known: the
        // caller delivers the request again (the engine answers a duplicate).
        aq_playbooks_runner::Error::Storage(_) => Error::Unknown,
        other => Error::Engine(other.to_string()),
    }
}
fn engine_error(e: impl std::fmt::Display) -> Error {
    Error::Engine(e.to_string())
}
struct UnavailableHost;
impl DomainHost for UnavailableHost {
    fn set_mode(&mut self, _: HostMode) {}
}
impl Host for UnavailableHost {
    fn authorize(&self, _: &Authority, _: &CommandContract) -> bool {
        false
    }
    fn dispatch(&mut self, _: &Intent) -> DispatchResult {
        DispatchResult::Rejected {
            reason: "AQ domain adapter not registered".into(),
        }
    }
    fn reconcile(&mut self, _: &Intent) -> DispatchResult {
        DispatchResult::Unknown {
            reason: "AQ domain recovery adapter not registered".into(),
        }
    }
    fn cancel(&mut self, _: &Intent) -> CancellationResult {
        CancellationResult::Refused
    }
}

fn reconcile_delivery<S: Storage>(
    engine: &Engine<S>,
    host: &mut impl DomainHost,
    ids: &[String],
    scan_after: Option<&str>,
    now_ms: i64,
) -> Result<Value, Error> {
    use runner::transition::{TransitionOutcome, TransitionRequest};
    let mut page = (Vec::new(), None);
    let mut plan = |state: &mut runner::State| {
        page = delivery_recovery_page(state, ids, scan_after, now_ms)?;
        Ok(false)
    };
    let request = &mut TransitionRequest {
        operation: "read_delivery_recovery_page",
        plan: &mut plan,
    };
    match engine
        .store
        .transition_v1(&engine.namespace, &engine.owner, request)
        .map_err(engine_error)?
    {
        TransitionOutcome::Unsupported => {
            page = delivery_recovery_page(
                &engine.snapshot().map_err(engine_error)?,
                ids,
                scan_after,
                now_ms,
            )
            .map_err(engine_error)?;
        }
        TransitionOutcome::Unchanged => {}
        TransitionOutcome::Committed { .. } => return Err(Error::Unknown),
    }
    let (candidates, next_after) = page;
    let mut count = 0;
    for intent in candidates {
        if routing_expired_before_dispatch(&intent) {
            let at = host.transition_time(now_ms).max(now_ms);
            engine.cancel(&intent.run, at).map_err(engine_error)?;
            count += 1;
            continue;
        }
        let Some(answer) = host.reconcile_delivery(&intent) else {
            continue;
        };
        let close = matches!(&answer, DispatchResult::Rejected { reason } if reason == DELIVERY_ADMISSION_EXPIRED);
        if !close
            && !matches!(
                answer,
                DispatchResult::Accepted { .. } | DispatchResult::Completed { .. }
            )
        {
            continue;
        }
        let at = host.transition_time(now_ms).max(now_ms);
        if !close || intent.reason.as_deref() != Some(DELIVERY_ADMISSION_EXPIRED) {
            engine
                .record_result(&intent.id, answer, at)
                .map_err(engine_error)?;
        }
        if close {
            // A rejected reply leaves an already blocked run blocked in the
            // engine. Retire this exact run after sealing its no-effect result.
            // A later page also finds the rejected intent if the process stops
            // between these two engine commits.
            engine.cancel(&intent.run, at).map_err(engine_error)?;
        }
        count += 1;
    }
    Ok(serde_json::json!({"reconciled":count,"next_after":next_after}))
}

pub const DELIVERY_ADMISSION_EXPIRED: &str = "delivery_request_expired_before_admission";

fn delivery_recovery_page(
    state: &runner::State,
    ids: &[String],
    after: Option<&str>,
    now_ms: i64,
) -> runner::Result<(Vec<Intent>, Option<String>)> {
    const SCAN: usize = 32;
    let mut candidates = reconcilable_delivery(state, ids)?;
    let keys = state.intents.keys_after(after, SCAN);
    let mut expired = 0;
    for key in &keys {
        if ids.contains(key) {
            continue;
        }
        if let Some(intent) = reconcilable_delivery(state, std::slice::from_ref(key))?.pop()
            && intent.deadline_at_ms <= now_ms
        {
            candidates.push(intent);
            expired += 1;
            if expired == 4 {
                return Ok((candidates, Some(key.clone())));
            }
        }
    }
    let next = (keys.len() == SCAN).then(|| keys.last().cloned()).flatten();
    Ok((candidates, next))
}

/// Commands that durably admit an asynchronous delivery operation before doing any external work.
pub const ASYNC_DELIVERY_COMMANDS: [&str; 6] = [
    "main.sync",
    "branch.publish",
    "pull_request.open",
    "pull_request.await_checks",
    "pull_request.merge",
    "pull_request.close",
];

/// The async delivery operations and the locally fenced delivery effects.
/// A receipt or pending record commits before any change outside that transaction.
pub const DELIVERY_RECOVERY_COMMANDS: [&str; 11] = [
    ASYNC_DELIVERY_COMMANDS[0],
    ASYNC_DELIVERY_COMMANDS[1],
    ASYNC_DELIVERY_COMMANDS[2],
    ASYNC_DELIVERY_COMMANDS[3],
    ASYNC_DELIVERY_COMMANDS[4],
    ASYNC_DELIVERY_COMMANDS[5],
    "work.read",
    "work.complete",
    "work.return",
    "work.hold",
    "supervisor.notify",
];

/// Rejected routing commands can have external effects. Retire only an expiry
/// the engine recorded before ever handing the command to the host.
fn routing_expired_before_dispatch(intent: &Intent) -> bool {
    matches!(
        intent.command.name.as_str(),
        "work.route" | "work.unroutable"
    ) && intent.status == IntentStatus::Rejected
        && intent.dispatch_sequence.is_none()
        && intent.reason.as_deref() == Some("command deadline expired before dispatch")
}

fn reconcilable_delivery(state: &runner::State, ids: &[String]) -> runner::Result<Vec<Intent>> {
    let mut found = Vec::new();
    for id in ids {
        let Some(intent) = state
            .intents
            .get(id)
            .filter(|intent| !intent.cancel_requested)
        else {
            continue;
        };
        let Some(run) = state.runs.get(&intent.run) else {
            continue;
        };
        if run.status.terminal()
            || !matches!(&run.pending, Some(runner::Pending::Effect { intent: pending }) if pending == &intent.id)
        {
            continue;
        }
        let blocked_rejection =
            run.status == runner::RunStatus::Blocked && intent.status == IntentStatus::Rejected;
        if blocked_rejection && routing_expired_before_dispatch(intent) {
            found.push(intent.clone());
            continue;
        }
        if !DELIVERY_RECOVERY_COMMANDS.contains(&intent.command.name.as_str())
            || !(blocked_rejection
                || (intent.status == IntentStatus::Rejected
                    && intent.reason.as_deref() == Some(DELIVERY_ADMISSION_EXPIRED))
                || matches!(
                    intent.status,
                    IntentStatus::Dispatched | IntentStatus::Unknown
                )
                || (intent.status == IntentStatus::Prepared && intent.dispatch_sequence.is_some()))
        {
            continue;
        }
        let authority = &intent.authority;
        let scope = model::digest(&(&authority.actor, &authority.scope))?;
        if state.execution_denied.contains(&run.artifact)
            || state
                .authority_revocations
                .get(&scope)
                .is_some_and(|revoked| !revoked.is_disjoint(&intent.command.capabilities))
        {
            continue;
        }
        found.push(intent.clone());
    }
    Ok(found)
}

/// Run-local pins remain available to the owner even after an extension
/// changes. Unknown lookup results must never authorize a new plugin call.
struct PluginCatalogHost<'a, S: Storage, H> {
    engine: &'a Engine<S>,
    host: &'a mut H,
    stale_artifacts: Option<&'a BTreeSet<String>>,
}

impl<S: Storage, H: DomainHost> Host for PluginCatalogHost<'_, S, H> {
    fn authorize(&self, authority: &Authority, contract: &CommandContract) -> bool {
        self.host.authorize(authority, contract)
    }

    fn dispatch(&mut self, intent: &Intent) -> DispatchResult {
        if let Some(stale) = self.stale_artifacts
            && plugins::command_plugin(&intent.command.name).is_some()
        {
            let Ok(state) = self.engine.snapshot() else {
                return DispatchResult::Unknown {
                    reason: "cannot read plugin artifact pins".into(),
                };
            };
            let Some(run) = state.runs.get(&intent.run) else {
                return DispatchResult::Unknown {
                    reason: "cannot read plugin run".into(),
                };
            };
            if stale.contains(&run.artifact)
                || state
                    .runs
                    .get(&run.root)
                    .is_some_and(|root| stale.contains(&root.artifact))
            {
                return self.host.dispatch_unavailable_plugin(intent);
            }
        }
        self.host.dispatch(intent)
    }

    fn reconcile(&mut self, intent: &Intent) -> DispatchResult {
        self.host.reconcile(intent)
    }

    fn cancel(&mut self, intent: &Intent) -> CancellationResult {
        self.host.cancel(intent)
    }
}

// Count through a capped writer rather than allocating an unbounded serialized
// copy merely to decide that a request is too large.
fn bounded_json(value: &impl Serialize, limit: usize) -> bool {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.0 {
                return Err(std::io::Error::other("embedding envelope bound"));
            }
            self.0 -= bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter(limit), value).is_ok()
}

fn remember_contracts(shared: &Shared, registry: &Registry) {
    if registry
        .commands
        .keys()
        .any(|name| plugins::command_plugin(name).is_some())
    {
        shared.plugin_artifacts.store(true, Ordering::Release);
    }
    shared
        .declared_events
        .write()
        .unwrap_or_else(|e| e.into_inner())
        .extend(
            registry
                .events
                .values()
                .filter_map(|contract| Some((contract.name.clone(), contract.fingerprint().ok()?))),
        );
}

fn accepts(registry: &Registry, artifact: &Registry) -> bool {
    accepts_base(registry, artifact) && plugins_match(registry, artifact)
}

// Existing runs keep their pinned extension contracts across plugin upgrades
// or removal. Only base-profile changes prevent reopening a namespace.
fn accepts_base(registry: &Registry, artifact: &Registry) -> bool {
    artifact.commands.iter().all(|(name, contract)| {
        plugins::extension_command(name, contract) || accepts_command(registry, contract)
    }) && artifact.events.iter().all(|(name, contract)| {
        plugins::command_plugin(name).is_some() || registry.events.get(name) == Some(contract)
    })
}

/// Exact host contracts, including the pinned Host9 work.read on Host10 so
/// the existing routing artifact can be installed in the new namespace, and
/// the catalog 2.6.0 work.read the delivery source is authored against.
pub fn accepts_command(registry: &Registry, contract: &CommandContract) -> bool {
    let current = registry.commands.get(&contract.name);
    if current == Some(contract) {
        return true;
    }
    #[cfg(feature = "delivery_playbook")]
    if contract.name == "work.read" {
        let pinned = |catalog: &'static OnceLock<Registry>, json: &str| {
            catalog
                .get_or_init(|| {
                    serde_json::from_str(json).expect("vendored AQ host catalog parses")
                })
                .commands
                .get(&contract.name)
                == Some(contract)
        };
        static HOST9: OnceLock<Registry> = OnceLock::new();
        static DELIVERY: OnceLock<Registry> = OnceLock::new();
        let host9 = include_str!("../../../vendor/aq-host-catalog-2.2.0/registry.json");
        let delivery = include_str!("../../../vendor/aq-host-catalog-2.6.0/registry.json");
        return match current.map(|c| c.version.as_str()) {
            Some("2.3.0") => pinned(&HOST9, host9) || pinned(&DELIVERY, delivery),
            // Catalog 2.5.0 (spec 0017): the delivery source keeps its 2.6.0 work.read.
            Some("2.5.0") if cfg!(feature = "project_environments") => pinned(&DELIVERY, delivery),
            _ => false,
        };
    }
    false
}

pub fn plugins_match(registry: &Registry, artifact: &Registry) -> bool {
    artifact.commands.iter().all(|(name, contract)| {
        plugins::command_plugin(name).is_none() || registry.commands.get(name) == Some(contract)
    }) && artifact.events.iter().all(|(name, contract)| {
        plugins::command_plugin(name).is_none() || registry.events.get(name) == Some(contract)
    })
}

fn cancel_effect<S: Storage>(
    engine: &Engine<S>,
    host: &mut impl DomainHost,
    now_ms: i64,
) -> Result<bool, Error> {
    let page = engine
        .query(&IndexQuery::Effects {
            kind: runner::EffectQueueKind::Cancel,
            scope_after: None,
            limit: 1,
        })
        .map_err(engine_error)?;
    if page.items.is_empty() {
        return Ok(false);
    }
    // This owner serializes all engine calls. Do not invoke dispatch_next in
    // cancellation mode unless its cancellation queue is known nonempty.
    Ok(engine
        .dispatch_next(host, now_ms)
        .map_err(engine_error)?
        .is_some())
}
