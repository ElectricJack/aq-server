//! AQ-owned bounded embedding. Not mounted by the daemon and no domain commands
//! are registered. Synchronous engine work runs only on one dedicated OS thread.
use aq_playbooks_model::{Artifact, CommandContract};
use aq_playbooks_runner::{
    Authority, CancellationResult, DispatchResult, Engine, EngineConfig, Host, IndexQuery, Intent,
    Storage,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::VecDeque,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::oneshot;

pub mod protected;
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
        }
    }
}
impl Limits {
    fn validate(&self) -> Result<(), Error> {
        if !(1..=128).contains(&self.work_queue)
            || !(1..=32).contains(&self.control_queue)
            || !(1024..=1_048_576).contains(&self.request_bytes)
            || !(1024..=1_048_576).contains(&self.response_bytes)
            || !(1..=64).contains(&self.transitions)
            || self.engine.max_runs == 0
            || self.engine.max_events == 0
            || self.engine.max_runs > 10_000
            || self.engine.max_events > 100_000
            || self.engine.max_queued_destinations == 0
            || self.engine.max_queued_destinations > 10_000
            || self.engine.max_receipts == 0
            || self.engine.max_receipts > 1_000_000
            || !(1024..=67_108_864).contains(&self.engine.max_state_bytes)
        {
            return Err(Error::Invalid);
        }
        Ok(())
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
    Admit {
        binding: String,
        entry: String,
        input: Value,
        authority: Authority,
        now_ms: i64,
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
        matches!(self, Self::Cancel { .. } | Self::Stop)
    }
    fn validate(&self) -> Result<(), Error> {
        let valid = |s: &str| !s.is_empty() && s.len() <= 512 && !s.contains('\0');
        let authority =
            |a: &Authority| valid(&a.actor) && valid(&a.scope) && a.capabilities.is_empty();
        let ok = match self {
            // Empty production registry until individually owned AQ adapters exist.
            Self::Install { artifact } => {
                artifact.content.registry.commands.is_empty()
                    && artifact.content.registry.events.is_empty()
            }
            Self::Activate {
                binding,
                artifact,
                authority: a,
            } => valid(binding) && valid(artifact) && authority(a),
            Self::Admit {
                binding,
                entry,
                authority: a,
                ..
            } => valid(binding) && valid(entry) && authority(a),
            Self::Runs { after, limit } => {
                (1..=100).contains(limit) && after.as_ref().is_none_or(|s| valid(s))
            }
            Self::Cancel { run, .. } => valid(run),
            Self::Tick { .. } | Self::Stop => true,
        };
        if ok { Ok(()) } else { Err(Error::Invalid) }
    }
}

struct Envelope {
    request: Request,
    reply: oneshot::Sender<Result<Value, Error>>,
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
impl Owner {
    /// The storage factory, including connection establishment and schema work,
    /// executes on the dedicated thread. It must configure bounded I/O itself.
    pub async fn start<S, F>(namespace: String, limits: Limits, factory: F) -> Result<Self, Error>
    where
        S: Storage + 'static,
        F: FnOnce() -> Result<S, Error> + Send + 'static,
    {
        limits.validate()?;
        if namespace.is_empty() || namespace.len() > 512 || namespace.contains('\0') {
            return Err(Error::Invalid);
        }
        let shared = Arc::new(Shared {
            queues: Mutex::new(Queues::default()),
            wake: Condvar::new(),
            clients: AtomicUsize::new(1),
            limits,
        });
        let owner = Self {
            shared: shared.clone(),
        };
        let (ready_tx, ready_rx) = oneshot::channel();
        std::thread::Builder::new()
            .name("aq-playbook-owner".into())
            .spawn(move || {
                let _exit = ThreadExit(shared.clone());
                let result = factory().and_then(|store| {
                    let mut engine =
                        Engine::new(store, namespace, uuid::Uuid::new_v4().to_string())
                            .map_err(engine_error)?;
                    engine.config = shared.limits.engine.clone();
                    let state = engine.snapshot().map_err(engine_error)?;
                    if state.artifacts.values().any(|a| {
                        !a.content.registry.commands.is_empty()
                            || !a.content.registry.events.is_empty()
                    }) {
                        let _ = engine.release();
                        return Err(Error::Invalid);
                    }
                    Ok(engine)
                });
                let engine = match result {
                    Ok(engine) => engine,
                    Err(e) => {
                        let _ = ready_tx.send(Err(e));
                        return;
                    }
                };
                if ready_tx.send(Ok(())).is_err() {
                    let _ = engine.release();
                    return;
                }
                let mut host = UnavailableHost;
                loop {
                    let envelope = {
                        let mut q = shared.queues.lock().unwrap_or_else(|e| e.into_inner());
                        loop {
                            if q.closing {
                                q.work.clear();
                                q.control.clear();
                                break None;
                            }
                            if let Some(e) = q.control.pop_front().or_else(|| q.work.pop_front()) {
                                break Some(e);
                            }
                            q = shared.wake.wait(q).unwrap_or_else(|e| e.into_inner());
                        }
                    };
                    let Some(envelope) = envelope else {
                        break;
                    };
                    let stop = matches!(envelope.request, Request::Stop);
                    let result = if stop {
                        engine.release().map(|_| Value::Null).map_err(engine_error)
                    } else {
                        execute(
                            &engine,
                            &mut host,
                            envelope.request,
                            shared.limits.transitions,
                        )
                    };
                    // A size failure after a commit is unknown, never 'not applied'.
                    let result = result.and_then(|v| {
                        if bounded_json(&v, shared.limits.response_bytes) {
                            Ok(v)
                        } else {
                            Err(Error::Unknown)
                        }
                    });
                    let _ = envelope.reply.send(result);
                    if stop {
                        break;
                    }
                }
                {
                    let mut q = shared.queues.lock().unwrap_or_else(|e| e.into_inner());
                    q.closing = true;
                    q.work.clear();
                    q.control.clear();
                }
                let _ = engine.release();
            })
            .map_err(|_| Error::Thread)?;
        ready_rx.await.map_err(|_| Error::Stopped)??;
        Ok(owner)
    }
    /// A timeout does not cancel admitted work or relinquish namespace ownership.
    /// Callers must reconcile effects; blindly resubmitting Admit can duplicate runs.
    pub async fn call(&self, request: Request, deadline: Duration) -> Result<Value, Error> {
        request.validate()?;
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
            queue.push_back(Envelope { request, reply });
            self.shared.wake.notify_one();
        }
        tokio::time::timeout(deadline, receiver)
            .await
            .map_err(|_| Error::Unknown)?
            .map_err(|_| Error::Stopped)?
    }
}
fn engine_error(e: impl std::fmt::Display) -> Error {
    Error::Engine(e.to_string())
}
struct UnavailableHost;
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
fn execute<S: Storage>(
    engine: &Engine<S>,
    host: &mut UnavailableHost,
    request: Request,
    transitions: usize,
) -> Result<Value, Error> {
    let value = match request {
        Request::Install { artifact } => {
            engine.install(*artifact).map_err(engine_error)?;
            Value::Null
        }
        Request::Activate {
            binding,
            artifact,
            authority,
        } => {
            engine
                .activate(&binding, &artifact, authority)
                .map_err(engine_error)?;
            Value::Null
        }
        Request::Admit {
            binding,
            entry,
            input,
            authority,
            now_ms,
        } => Value::String(
            engine
                .admit(&binding, &entry, input, authority, now_ms)
                .map_err(engine_error)?,
        ),
        Request::Runs { after, limit } => serde_json::to_value(
            engine
                .query(&IndexQuery::Runs { after, limit })
                .map_err(engine_error)?,
        )
        .map_err(engine_error)?,
        Request::Tick { now_ms } => {
            serde_json::json!({"transitions": engine.drive(host, now_ms, transitions).map_err(engine_error)?})
        }
        Request::Cancel { run, now_ms } => {
            engine.cancel(&run, now_ms).map_err(engine_error)?;
            Value::Null
        }
        // Stop acknowledgement proves this thread reached a boundary, not that
        // external effects stopped; this foundation has no registered effects.
        Request::Stop => Value::Null,
    };
    Ok(value)
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
