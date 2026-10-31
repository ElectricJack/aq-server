//! Deterministic durable workflow transitions with explicit effect handoffs.
//! All methods take trusted host/storage time; producer timestamps never win races.
pub mod inspection;
pub mod metrics;
pub mod storage;
pub use inspection::{InspectionKind, InspectionPage, InspectionQuery};
pub mod indexes;
pub mod retention;
use aq_playbooks_model::*;
pub use indexes::{EffectQueueKind, IndexItem, IndexPage, IndexQuery, RuntimeIndexes};
pub use retention::{GcReport, RetentionPolicy, RetentionState};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
pub use storage::{MemoryStorage, Storage, StorageError, VersionedState};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Storage(#[from] StorageError),
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error("refused: {0}")]
    Refused(String),
    #[error("integrity conflict: {0}")]
    Conflict(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error("execution budget exhausted: {0}")]
    Exhausted(String),
}
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Authority {
    pub actor: String,
    pub scope: String,
    pub capabilities: BTreeSet<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Activation {
    pub artifact: String,
    pub authority: Authority,
    pub revision: u64,
    #[serde(default)]
    pub controls: ActivationControls,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ActivationControls {
    pub calls_enabled: bool,
    pub event_starts_enabled: bool,
    pub execution_enabled: bool,
}
impl Default for ActivationControls {
    fn default() -> Self {
        Self {
            calls_enabled: true,
            event_starts_enabled: true,
            execution_enabled: true,
        }
    }
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Admitted,
    Running,
    Waiting,
    Finishing,
    Cancelling,
    Blocked,
    Completed,
    Failed,
    Cancelled,
}
impl RunStatus {
    pub fn terminal(&self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Run {
    #[serde(default)]
    pub catalog_revision: u64,
    pub id: String,
    pub root: String,
    pub parent: Option<String>,
    pub origin: Option<String>,
    pub artifact: String,
    pub entry: String,
    pub dependencies: BTreeMap<String, String>,
    pub authority: Authority,
    pub step: String,
    pub status: RunStatus,
    pub input: Value,
    pub variables: BTreeMap<String, Value>,
    pub outcome: Option<String>,
    pub output: Option<Value>,
    pub reason: Option<String>,
    pub admitted_at_ms: i64,
    pub visits: u64,
    pub depth: u32,
    pub children: Vec<String>,
    pub pending: Option<Pending>,
    pub loops: Vec<LoopFrame>,
    pub attempts: BTreeMap<String, u32>,
    pub pending_return: Option<(String, Value)>,
    #[serde(default)]
    pub pending_failure: bool,
    pub terminal_sequence: Option<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopFrame {
    pub site: String,
    pub item: String,
    pub items: Vec<Value>,
    pub index: usize,
    pub body: String,
    pub next: String,
    pub failure: IterationFailure,
    pub previous: Option<Value>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Pending {
    Effect {
        intent: String,
    },
    Event {
        wait: String,
    },
    Child {
        child: String,
        bind: String,
    },
    Join {
        members: Vec<String>,
        #[serde(default)]
        member_keys: Vec<String>,
        mode: JoinMode,
        accepted: BTreeSet<String>,
        deadline: i64,
        remaining: RemainingPolicy,
        bind: String,
    },
    Retry {
        deadline: i64,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Budget {
    pub visits: u64,
    pub children: u64,
    pub iterations: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Receipt {
    pub sequence: u64,
    pub run: String,
    pub step: String,
    pub at_ms: i64,
    pub kind: String,
    pub detail: Value,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IntentStatus {
    Prepared,
    Dispatched,
    Accepted,
    Completed,
    Unknown,
    Rejected,
    Cancelled,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Intent {
    pub id: String,
    pub run: String,
    pub step: String,
    pub invocation: u64,
    #[serde(default)]
    pub deadline_at_ms: i64,
    pub command: CommandContract,
    pub input: Value,
    pub input_digest: String,
    pub authority: Authority,
    pub status: IntentStatus,
    #[serde(default)]
    pub cancel_requested: bool,
    #[serde(default)]
    pub cancel_dispatched: bool,
    pub handle: Option<String>,
    pub result: Option<(String, Value)>,
    pub reason: Option<String>,
    #[serde(default)]
    pub late_results: Vec<(String, Value)>,
    #[serde(default)]
    pub completion_event: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitRecord {
    pub id: String,
    pub run: String,
    pub step: String,
    pub event: Option<ContractRef>,
    pub scope: String,
    pub correlation: BTreeMap<String, String>,
    pub minimum_revision: Option<u64>,
    pub deadline: i64,
    pub bind: String,
    pub winner: Option<String>,
    pub expired: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Destination {
    pub key: String,
    pub artifact: String,
    pub entry: String,
    pub input: Value,
    pub authority: Authority,
    pub dependencies: BTreeMap<String, String>,
    pub run: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AcceptedEvent {
    pub key: String,
    pub envelope: EventEnvelope,
    pub digest: String,
    pub accepted_at_ms: i64,
    pub sequence: u64,
    pub catalog_revision: u64,
    pub destinations: Vec<Destination>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventDisposition {
    pub key: String,
    pub duplicate: bool,
    pub destinations: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct State {
    #[serde(default)]
    pub catalog_history: BTreeMap<u64, BTreeMap<String, Activation>>,
    #[serde(default)]
    pub catalog_history_at_ms: BTreeMap<u64, i64>,
    #[serde(default)]
    pub retention: RetentionState,
    #[serde(default)]
    pub execution_denied: BTreeSet<String>,
    #[serde(default)]
    pub authority_revocations: BTreeMap<String, BTreeSet<String>>,
    #[serde(default)]
    pub recovery_requests: BTreeMap<String, RecoveryRequest>,
    #[serde(skip)]
    pub protection_available: bool,
    #[serde(default)]
    pub indexes: RuntimeIndexes,
    pub artifacts: BTreeMap<String, Artifact>,
    pub bindings: BTreeMap<String, Activation>,
    pub catalog_revision: u64,
    pub runs: BTreeMap<String, Run>,
    pub receipts: Vec<Receipt>,
    pub intents: BTreeMap<String, Intent>,
    pub waits: BTreeMap<String, WaitRecord>,
    pub events: BTreeMap<String, AcceptedEvent>,
    pub budgets: BTreeMap<String, Budget>,
    pub sequence: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecoveryRequest {
    pub id: String,
    pub intent: String,
    pub actor: String,
    pub reason: String,
    pub evidence_refs: Vec<String>,
    pub requested_at_ms: i64,
    pub consumed: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DispatchResult {
    Completed { outcome: String, output: Value },
    Accepted { handle: String },
    Unknown { reason: String },
    Rejected { reason: String },
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CancellationResult {
    Confirmed,
    Accepted,
    Refused,
    TooLate,
    Unknown,
}
pub trait Host {
    fn authorize(&self, authority: &Authority, contract: &CommandContract) -> bool;
    fn dispatch(&mut self, intent: &Intent) -> DispatchResult;
    fn reconcile(&mut self, intent: &Intent) -> DispatchResult;
    /// A supported cancellation is a keyed idempotent request. The stable key is
    /// `cancellation_key(intent)`, independent of the original business effect.
    /// Lost responses are reconciled before delivery retry; acceptance is never
    /// interpreted as confirmed stopping.
    fn cancel(&mut self, _intent: &Intent) -> CancellationResult {
        CancellationResult::Unknown
    }
}
pub fn cancellation_key(intent: &Intent) -> Result<String> {
    Ok(digest(&(&intent.id, "cancel"))?)
}
/// A runner incarnation must be unique. Do not share its identifier between processes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineConfig {
    pub max_runs: usize,
    pub max_events: usize,
    pub max_queued_destinations: usize,
    pub max_state_bytes: usize,
    pub max_receipts: usize,
}
impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_runs: 10_000,
            max_events: 100_000,
            max_queued_destinations: 10_000,
            max_state_bytes: 67_108_864,
            max_receipts: 1_000_000,
        }
    }
}
pub struct Engine<S: Storage> {
    pub store: S,
    pub namespace: String,
    pub owner: String,
    pub config: EngineConfig,
    pub metrics: metrics::Metrics,
}
impl<S: Storage> Engine<S> {
    pub fn new(store: S, namespace: impl Into<String>, owner: impl Into<String>) -> Result<Self> {
        let e = Self {
            store,
            namespace: namespace.into(),
            owner: owner.into(),
            config: EngineConfig::default(),
            metrics: metrics::Metrics::default(),
        };
        e.store.acquire(&e.namespace, &e.owner)?;
        e.transaction(|s| {
            if !s.indexes.initialized {
                indexes::rebuild(s)?;
            }
            Ok(())
        })?;
        Ok(e)
    }
    pub fn with_config(
        store: S,
        namespace: impl Into<String>,
        owner: impl Into<String>,
        config: EngineConfig,
    ) -> Result<Self> {
        if config.max_runs == 0
            || config.max_events == 0
            || config.max_queued_destinations == 0
            || config.max_state_bytes < 1024
            || config.max_receipts == 0
        {
            return Err(Error::Refused("invalid namespace resource bounds".into()));
        }
        let mut e = Self::new(store, namespace, owner)?;
        e.config = config;
        Ok(e)
    }
    fn measure_host<T>(&self, f: impl FnOnce() -> T) -> T {
        let started = std::time::Instant::now();
        let result = f();
        self.metrics
            .record_time(metrics::Timing::Host, started.elapsed());
        result
    }
    pub fn release(&self) -> Result<()> {
        Ok(self.store.release(&self.namespace, &self.owner)?)
    }
    pub fn snapshot(&self) -> Result<State> {
        Ok(self.store.load(&self.namespace)?.state)
    }
    fn transaction<T>(&self, f: impl FnOnce(&mut State) -> Result<T>) -> Result<T> {
        let started = std::time::Instant::now();
        let loaded = self.store.load(&self.namespace);
        self.metrics
            .record_time(metrics::Timing::Load, started.elapsed());
        let mut v = loaded?;
        if !v.state.indexes.initialized {
            indexes::rebuild(&mut v.state)?;
        }
        v.state.protection_available = self.store.protection_available();
        let started = std::time::Instant::now();
        let evaluated = f(&mut v.state);
        self.metrics
            .record_time(metrics::Timing::Transition, started.elapsed());
        let result = evaluated?;
        indexes::sync(&mut v.state)?;
        if v.state.runs.len() > self.config.max_runs
            || v.state.events.len() > self.config.max_events
            || v.state
                .events
                .values()
                .flat_map(|e| &e.destinations)
                .filter(|d| d.run.is_none())
                .count()
                > self.config.max_queued_destinations
            || v.state.receipts.len() > self.config.max_receipts
            || canonical_bytes(&v.state)?.len() > self.config.max_state_bytes
        {
            return Err(Error::Refused(
                "namespace retention/admission quota exhausted".into(),
            ));
        }
        let started = std::time::Instant::now();
        let committed =
            self.store
                .compare_and_swap(&self.namespace, &self.owner, v.revision, &v.state);
        self.metrics
            .record_time(metrics::Timing::Commit, started.elapsed());
        committed?;
        Ok(result)
    }
    pub fn install(&self, artifact: Artifact) -> Result<()> {
        artifact.verify()?;
        self.transaction(|s| {
            if let Some(old) = s.artifacts.get(&artifact.digest)
                && old != &artifact
            {
                return Err(Error::Conflict("artifact replacement".into()));
            }
            s.artifacts.insert(artifact.digest.clone(), artifact);
            Ok(())
        })
    }
    pub fn activate(&self, binding: &str, artifact: &str, authority: Authority) -> Result<()> {
        let now_ms = self
            .store
            .load(&self.namespace)?
            .state
            .receipts
            .last()
            .map(|receipt| receipt.at_ms)
            .unwrap_or(0);
        self.activate_at(binding, artifact, authority, now_ms)
    }
    pub fn activate_at(
        &self,
        binding: &str,
        artifact: &str,
        authority: Authority,
        now_ms: i64,
    ) -> Result<()> {
        self.transaction(|s| {
            let a = s
                .artifacts
                .get(artifact)
                .ok_or_else(|| Error::NotFound(artifact.into()))?;
            check_authority(a, &authority)?;
            s.catalog_revision += 1;
            let controls = s
                .bindings
                .get(binding)
                .map(|activation| activation.controls.clone())
                .unwrap_or_default();
            if !controls.execution_enabled {
                s.execution_denied.insert(artifact.into());
            }
            s.bindings.insert(
                binding.into(),
                Activation {
                    artifact: artifact.into(),
                    authority,
                    revision: s.catalog_revision,
                    controls,
                },
            );
            s.catalog_history
                .insert(s.catalog_revision, s.bindings.clone());
            s.catalog_history_at_ms.insert(s.catalog_revision, now_ms);
            indexes::index_activation(s, binding)?;
            Ok(())
        })
    }
    pub fn admit(
        &self,
        binding: &str,
        entry: &str,
        input: Value,
        authority: Authority,
        now_ms: i64,
    ) -> Result<String> {
        let result = self.transaction(|s| {
            let activation = s
                .bindings
                .get(binding)
                .ok_or_else(|| Error::NotFound(binding.into()))?
                .clone();
            if !activation.controls.calls_enabled {
                return Err(Error::Refused("new calls disabled".into()));
            }
            let dependencies = resolve(s, &activation.artifact, &authority)?;
            let id = next_id(s, "run");
            admit_run(
                s,
                id.clone(),
                id.clone(),
                None,
                None,
                &activation.artifact,
                entry,
                input,
                authority,
                dependencies,
                0,
                now_ms,
            )?;
            Ok(id)
        });
        self.metrics.admission(&result, false);
        result
    }
    pub fn advance(&self, run_id: &str, now_ms: i64) -> Result<bool> {
        let (progress, queued_ms) = self.transaction(|s| {
            let queued_ms = s
                .runs
                .get(run_id)
                .filter(|r| r.visits == 0)
                .map(|r| now_ms.saturating_sub(r.admitted_at_ms).max(0) as u64);
            Ok((advance(s, run_id, now_ms)?, queued_ms))
        })?;
        if progress && let Some(elapsed) = queued_ms {
            self.metrics.record_queue(elapsed);
        }
        Ok(progress)
    }
    pub fn cancel(&self, run_id: &str, now_ms: i64) -> Result<()> {
        self.transaction(|s| {
            request_cancel(s, run_id, now_ms)?;
            Ok(())
        })
    }
    pub fn admit_event(
        &self,
        authenticated_producer: &str,
        event: EventEnvelope,
        now_ms: i64,
    ) -> Result<EventDisposition> {
        self.admit_event_in_epoch(authenticated_producer, event, 0, now_ms)
    }
    pub fn admit_event_in_epoch(
        &self,
        authenticated_producer: &str,
        event: EventEnvelope,
        epoch: u64,
        now_ms: i64,
    ) -> Result<EventDisposition> {
        let result = self.transaction(|s| {
            if s.retention
                .producer_epochs
                .get(authenticated_producer)
                .copied()
                .unwrap_or(0)
                != epoch
            {
                return Err(Error::Refused(
                    "producer replay epoch retired; explicit new replay identity required".into(),
                ));
            }
            let key = digest(&(authenticated_producer, epoch, &event.event_id))?;
            if let Some(tombstone) = s.retention.event_tombstones.get(&key) {
                if tombstone.digest != digest(&event)? {
                    return Err(Error::Conflict(
                        "expired event identity reused with changed content".into(),
                    ));
                }
                return Err(Error::Refused(
                    "event replay horizon expired; explicit new replay identity required".into(),
                ));
            }
            let content = digest(&event)?;
            if event.producer != authenticated_producer {
                return Err(Error::Refused("forged producer identity".into()));
            }
            if let Some(old) = s.events.get(&key) {
                if s.retention
                    .event_expires_at_ms
                    .get(&key)
                    .is_some_and(|expiry| now_ms > *expiry)
                {
                    return Err(Error::Refused(
                        "event replay horizon expired; explicit new replay identity required"
                            .into(),
                    ));
                }
                if old.digest != content {
                    return Err(Error::Conflict(
                        "event identity reused with changed content".into(),
                    ));
                }
                return Ok(EventDisposition {
                    key,
                    duplicate: true,
                    destinations: old.destinations.len(),
                });
            }
            let contract = find_event(s, &event.contract)?;
            if !contract.trusted_producers.contains(authenticated_producer)
                || !contract.scopes.contains(&event.scope)
            {
                return Err(Error::Refused("event producer or scope".into()));
            }
            validate_event(
                &event,
                &Registry {
                    commands: BTreeMap::new(),
                    events: BTreeMap::from([(contract.name.clone(), contract.clone())]),
                },
            )?;
            if !s.protection_available
                && (!contract.sensitive_payload_fields.is_empty()
                    || s.artifacts.values().any(|artifact| {
                        artifact
                            .content
                            .registry
                            .events
                            .get(&contract.name)
                            .is_some_and(|event_contract| event_contract == &contract)
                            && artifact.content.registry.commands.values().any(|command| {
                                !command.sensitive_output_fields.is_empty()
                                    && command.asynchronous.as_ref().is_some_and(|asynchronous| {
                                        asynchronous.completion_event == contract.name
                                    })
                            })
                    }))
            {
                return Err(Error::Refused(
                    "protected-value storage unavailable for event payload".into(),
                ));
            }
            let mut destinations = vec![];
            let mut blockers = vec![];
            let candidates = s
                .indexes
                .triggers
                .get(&indexes::trigger_key(&event.contract, &event.scope)?)
                .cloned()
                .unwrap_or_default();
            for binding in candidates {
                let activation = &s.bindings[&binding];
                let a = &s.artifacts[&activation.artifact];
                if activation.authority.scope != event.scope {
                    continue;
                }
                for trigger in &a.content.playbook.triggers {
                    if trigger.event != event.contract {
                        continue;
                    }
                    let mut ctx = EvalContext::new();
                    ctx.insert("event".into(), EvalValue::Known(event.payload.clone()));
                    let destination_key =
                        digest(&(&key, &binding, activation.revision, &trigger.id))?;
                    let input = match known_expr(&trigger.input, &ctx).and_then(|input| {
                        a.content.playbook.entries[&trigger.entry]
                            .input
                            .validate(&input)?;
                        check_bytes(&input, a.content.playbook.limits.max_payload_bytes)?;
                        Ok(input)
                    }) {
                        Ok(input) => input,
                        Err(error) => {
                            blockers.push((destination_key.clone(), "input", error.to_string()));
                            Value::Null
                        }
                    };
                    let deps = match resolve(s, &a.digest, &activation.authority) {
                        Ok(deps) => deps,
                        Err(error) => {
                            blockers.push((
                                destination_key.clone(),
                                "dependency",
                                error.to_string(),
                            ));
                            BTreeMap::new()
                        }
                    };
                    ensure_admission_protection(s, &a.digest, &deps, s.catalog_revision)?;
                    destinations.push(Destination {
                        key: destination_key,
                        artifact: a.digest.clone(),
                        entry: trigger.entry.clone(),
                        input,
                        authority: activation.authority.clone(),
                        dependencies: deps,
                        run: None,
                    });
                }
            }
            let count = destinations.len();
            for (destination, blocked_kind, reason) in blockers {
                receipt(
                    s,
                    &format!("event:{key}"),
                    "admission",
                    now_ms,
                    "destination_blocked",
                    json!({"destination":destination,"blocked_kind":blocked_kind,"reason":reason}),
                );
            }
            s.sequence += 1;
            let sequence = s.sequence;
            s.events.insert(
                key.clone(),
                AcceptedEvent {
                    key: key.clone(),
                    envelope: event,
                    digest: content,
                    accepted_at_ms: now_ms,
                    sequence,
                    catalog_revision: s.catalog_revision,
                    destinations,
                },
            );
            s.retention.event_epochs.insert(key.clone(), epoch);
            s.retention.event_expires_at_ms.insert(
                key.clone(),
                deadline(
                    now_ms,
                    s.retention
                        .policy
                        .event_replay_ms
                        .max(contract.retention_ms),
                )?,
            );
            indexes::index_event(s, &key)?;
            Ok(EventDisposition {
                key,
                duplicate: false,
                destinations: count,
            })
        });
        self.metrics.admission(
            &result,
            result
                .as_ref()
                .is_ok_and(|disposition| disposition.duplicate),
        );
        result
    }
    pub fn fanout(&self, event_key: &str, limit: usize, now_ms: i64) -> Result<usize> {
        self.transaction(|s| {
            let mut event = s
                .events
                .get(event_key)
                .ok_or_else(|| Error::NotFound(event_key.into()))?
                .clone();
            let mut count = 0;
            for d in event.destinations.iter_mut().filter(|d| d.run.is_none()) {
                if count >= limit {
                    break;
                }
                // An unresolved input belongs to the frozen event/proposal. A
                // later activation cannot repair or reinterpret it.
                if s.receipts.iter().any(|receipt| {
                    receipt.kind == "destination_blocked"
                        && receipt.detail.get("destination").and_then(Value::as_str) == Some(&d.key)
                        && receipt.detail.get("blocked_kind").and_then(Value::as_str)
                            == Some("input")
                }) {
                    continue;
                }
                // Missing immutable bytes may be installed later. Resolution
                // still uses the accepted exact artifact, never current bindings.
                if d.dependencies.is_empty() {
                    let Ok(dependencies) =
                        resolve_at(s, &d.artifact, &d.authority, event.catalog_revision)
                    else {
                        continue;
                    };
                    d.dependencies = dependencies;
                }
                let id = format!("event-{}", d.key);
                admit_run(
                    s,
                    id.clone(),
                    id.clone(),
                    None,
                    None,
                    &d.artifact,
                    &d.entry,
                    d.input.clone(),
                    d.authority.clone(),
                    d.dependencies.clone(),
                    0,
                    now_ms,
                )?;
                s.runs.get_mut(&id).unwrap().catalog_revision = event.catalog_revision;
                d.run = Some(id);
                count += 1;
            }
            s.events.insert(event_key.into(), event);
            Ok(count)
        })
    }
    /// Persist dispatch state before handing anything to a host. Crashes here leave
    /// a recoverable ambiguity rather than an apparently safe fresh invocation.
    pub fn dispatch_next(&self, host: &mut impl Host, now_ms: i64) -> Result<Option<String>> {
        let cancellation = self.transaction(|s| {
            let after = s.indexes.effect_cursor.get("cancel").cloned();
            let candidate = s
                .indexes
                .query(
                    s,
                    &IndexQuery::Effects {
                        kind: EffectQueueKind::Cancel,
                        scope_after: after,
                        limit: 1,
                    },
                )
                .items
                .into_iter()
                .next();
            let id = candidate.map(|item| {
                s.indexes
                    .effect_item_cursor
                    .insert(format!("cancel:{}", item.scope), item.key.clone());
                s.indexes.effect_cursor.insert("cancel".into(), item.scope);
                item.key
            });
            if let Some(id) = id {
                indexes::mark_intent(s, &id);
                let intent = s.intents.get_mut(&id).unwrap();
                if !intent.command.cancellation
                    || !host.authorize(&intent.authority, &intent.command)
                {
                    intent.cancel_dispatched = true;
                    return Ok(None);
                }
                intent.cancel_dispatched = true;
                let copy = intent.clone();
                receipt(
                    s,
                    &copy.run,
                    &copy.step,
                    now_ms,
                    "cancel_dispatch",
                    json!({"operation":id}),
                );
                Ok(Some(copy))
            } else {
                Ok(None)
            }
        })?;
        if let Some(intent) = cancellation {
            let disposition = self.measure_host(|| host.cancel(&intent));
            self.transaction(|s| {
                indexes::mark_intent(s, &intent.id);
                if matches!(disposition, CancellationResult::Confirmed) {
                    s.intents.get_mut(&intent.id).unwrap().status = IntentStatus::Cancelled;
                }
                receipt(
                    s,
                    &intent.run,
                    &intent.step,
                    now_ms,
                    "cancel_response",
                    json!({"operation":intent.id,"disposition":disposition}),
                );
                Ok(())
            })?;
            return Ok(Some(intent.id));
        }

        let selected = self.transaction(|s| {
            let after = s.indexes.effect_cursor.get("dispatch").cloned();
            let Some(item) = s
                .indexes
                .query(
                    s,
                    &IndexQuery::Effects {
                        kind: EffectQueueKind::Dispatch,
                        scope_after: after,
                        limit: 1,
                    },
                )
                .items
                .into_iter()
                .next()
            else {
                return Ok(None);
            };
            s.indexes
                .effect_item_cursor
                .insert(format!("dispatch:{}", item.scope), item.key.clone());
            s.indexes
                .effect_cursor
                .insert("dispatch".into(), item.scope);
            let id = item.key;
            indexes::mark_intent(s, &id);
            let intent = s.intents[&id].clone();
            if now_ms >= intent.deadline_at_ms {
                let expired = s.intents.get_mut(&id).unwrap();
                expired.status = IntentStatus::Rejected;
                expired.reason = Some("command deadline expired before dispatch".into());
                receipt(
                    s,
                    &intent.run,
                    &intent.step,
                    now_ms,
                    "command_deadline_expired",
                    json!({"operation":id,"issued":false}),
                );
                return Ok(Some(s.intents[&id].clone()));
            }
            if !host.authorize(&intent.authority, &intent.command)
                || !execution_allowed(
                    s,
                    &s.runs[&intent.run].artifact,
                    &intent.authority,
                    &intent.command.capabilities,
                )?
            {
                let i = s.intents.get_mut(&id).unwrap();
                i.status = IntentStatus::Unknown;
                i.reason = Some("authority revoked".into());
                let run = s.runs.get_mut(&intent.run).unwrap();
                run.status = RunStatus::Blocked;
                run.reason = Some("authority revoked".into());
                receipt(
                    s,
                    &intent.run,
                    &intent.step,
                    now_ms,
                    "authority_blocked",
                    Value::Null,
                );
                return Ok(None);
            }
            s.intents.get_mut(&id).unwrap().status = IntentStatus::Dispatched;
            receipt(
                s,
                &intent.run,
                &intent.step,
                now_ms,
                "dispatch",
                json!({"operation":id}),
            );
            Ok(Some(s.intents[&id].clone()))
        })?;
        let Some(intent) = selected else {
            return Ok(None);
        };
        if intent.status == IntentStatus::Rejected {
            return Ok(Some(intent.id));
        }
        let result = self.measure_host(|| host.dispatch(&intent));
        self.record_result(&intent.id, result, now_ms)?;
        Ok(Some(intent.id))
    }
    pub fn record_result(
        &self,
        intent_id: &str,
        result: DispatchResult,
        now_ms: i64,
    ) -> Result<()> {
        self.transaction(|s| record_result(s, intent_id, result, now_ms))
    }
    pub fn recover(&self, host: &mut impl Host, now_ms: i64) -> Result<usize> {
        let state = self.snapshot()?;
        let mut n = 0;
        let candidates = self.store.query(
            &self.namespace,
            &IndexQuery::Effects {
                kind: EffectQueueKind::Recovery,
                scope_after: state.indexes.effect_cursor.get("recovery").cloned(),
                limit: 64,
            },
        )?;
        for item in candidates.items {
            let Some(intent) = state.intents.get(&item.key) else {
                continue;
            };
            if !host.authorize(&intent.authority, &intent.command)
                || !execution_allowed(
                    &state,
                    &state.runs[&intent.run].artifact,
                    &intent.authority,
                    &intent.command.capabilities,
                )?
            {
                continue;
            }
            self.transaction(|s| {
                s.indexes
                    .effect_item_cursor
                    .insert(format!("recovery:{}", item.scope), item.key.clone());
                s.indexes
                    .effect_cursor
                    .insert("recovery".into(), item.scope.clone());
                receipt(
                    s,
                    &intent.run,
                    &intent.step,
                    now_ms,
                    "recovery_attempt",
                    json!({"operation":intent.id}),
                );
                Ok(())
            })?;
            if intent.cancel_requested && intent.cancel_dispatched && intent.command.cancellation {
                let result = self.measure_host(|| host.reconcile(intent));
                let running = matches!(result, DispatchResult::Accepted { .. });
                self.record_result(&intent.id, result, now_ms)?;
                if running {
                    self.transaction(|s| {
                        let current = s.intents.get_mut(&intent.id).unwrap();
                        current.cancel_dispatched = false;
                        indexes::mark_intent(s, &intent.id);
                        receipt(
                            s,
                            &intent.run,
                            &intent.step,
                            now_ms,
                            "cancel_retry_authorized_by_status",
                            json!({"operation":intent.id}),
                        );
                        Ok(())
                    })?;
                }
                n += 1;
                continue;
            }
            let authorized_request = state
                .recovery_requests
                .values()
                .find(|request| request.intent == intent.id && !request.consumed);
            if (now_ms >= intent.deadline_at_ms || intent.cancel_requested)
                && intent.command.recovery != Recovery::Reconcilable
                && authorized_request.is_none()
            {
                self.record_result(&intent.id, DispatchResult::Unknown {
                    reason: "operation expired or cancellation requested; replay cannot submit another effect".into()
                }, now_ms)?;
                n += 1;
                continue;
            }
            let result = match intent.command.recovery {
                Recovery::Observation
                | Recovery::NaturallyIdempotent
                | Recovery::KeyedIdempotent => {
                    self.transaction(|s| {
                        let current = s.intents.get_mut(&intent.id).unwrap();
                        current.status = IntentStatus::Prepared;
                        indexes::mark_intent(s, &intent.id);
                        Ok(())
                    })?;
                    n += 1;
                    continue;
                }
                Recovery::Reconcilable => {
                    if let Some(request) = authorized_request {
                        self.transaction(|s| {
                            s.recovery_requests.get_mut(&request.id).unwrap().consumed = true;
                            Ok(())
                        })?;
                    }
                    self.measure_host(|| host.reconcile(intent))
                }
                Recovery::Unsafe => {
                    let request = state
                        .recovery_requests
                        .values()
                        .find(|request| request.intent == intent.id && !request.consumed);
                    if let Some(request) = request {
                        self.transaction(|s| {
                            s.recovery_requests.get_mut(&request.id).unwrap().consumed = true;
                            receipt(
                                s,
                                &intent.run,
                                &intent.step,
                                now_ms,
                                "authorized_recovery",
                                json!({"request":request.id,"actor":request.actor}),
                            );
                            Ok(())
                        })?;
                        self.measure_host(|| host.reconcile(intent))
                    } else {
                        DispatchResult::Unknown {
                            reason: "unsafe external outcome requires authorized reconciliation"
                                .into(),
                        }
                    }
                }
            };
            self.record_result(&intent.id, result, now_ms)?;
            n += 1;
        }
        Ok(n)
    }
    pub fn drive(
        &self,
        host: &mut impl Host,
        now_ms: i64,
        max_transitions: usize,
    ) -> Result<usize> {
        let mut total = 0;
        while total < max_transitions {
            let due = self
                .store
                .query(&self.namespace, &IndexQuery::DueTimers { now_ms, limit: 1 })?;
            let completions = self.store.query(
                &self.namespace,
                &IndexQuery::Effects {
                    kind: EffectQueueKind::Completion,
                    scope_after: None,
                    limit: 1,
                },
            )?;
            if !due.items.is_empty() || !completions.items.is_empty() {
                self.transaction(|s| {
                    process_completions(s, now_ms)?;
                    process_operation_deadlines(s, now_ms);
                    let timers = s
                        .indexes
                        .query(s, &IndexQuery::DueTimers { now_ms, limit: 64 })
                        .items;
                    for item in timers {
                        if let Some(timer) = s.indexes.timers.get(&item.key).cloned() {
                            indexes::remove_timer(s, &item.key);
                            if timer.intent.is_none() {
                                indexes::wake(s, &timer.run);
                            }
                        }
                    }
                    Ok(())
                })?;
            }
            let item = self
                .store
                .query(
                    &self.namespace,
                    &IndexQuery::Ready {
                        scope_after: None,
                        limit: 1,
                    },
                )?
                .items
                .into_iter()
                .next();
            let dispatch = self.store.query(
                &self.namespace,
                &IndexQuery::Effects {
                    kind: EffectQueueKind::Dispatch,
                    scope_after: None,
                    limit: 1,
                },
            )?;
            let cancel = self.store.query(
                &self.namespace,
                &IndexQuery::Effects {
                    kind: EffectQueueKind::Cancel,
                    scope_after: None,
                    limit: 1,
                },
            )?;
            let has_effect = !dispatch.items.is_empty() || !cancel.items.is_empty();
            if item.is_none() && !has_effect {
                break;
            }
            let (effect_turn, queued_ms) = self.transaction(|s| {
                if has_effect && (s.indexes.dispatch_turn || item.is_none()) {
                    s.indexes.dispatch_turn = false;
                    Ok((true, None))
                } else if let Some(item) = item {
                    let mut queued_ms = None;
                    if s.indexes.ready_tokens.contains_key(&item.key) {
                        s.indexes.scope_cursor = Some(item.scope);
                        indexes::unqueue(s, &item.key);
                        let elapsed = s
                            .runs
                            .get(&item.key)
                            .filter(|r| r.visits == 0)
                            .map(|r| now_ms.saturating_sub(r.admitted_at_ms).max(0) as u64);
                        if advance(s, &item.key, now_ms)? {
                            queued_ms = elapsed;
                        }
                    }
                    s.indexes.dispatch_turn = true;
                    Ok((false, queued_ms))
                } else {
                    Ok((false, None))
                }
            })?;
            if let Some(elapsed) = queued_ms {
                self.metrics.record_queue(elapsed);
            }
            if effect_turn {
                self.dispatch_next(host, now_ms)?;
            }
            total += 1;
        }
        Ok(total)
    }
}
fn check_bytes(value: &Value, limit: usize) -> Result<()> {
    if canonical_bytes(value)?.len() > limit {
        return Err(Error::Refused("payload byte limit".into()));
    }
    Ok(())
}
fn check_authority(artifact: &Artifact, authority: &Authority) -> Result<()> {
    if artifact.content.playbook.scope != authority.scope
        || !artifact
            .content
            .playbook
            .required_capabilities
            .is_subset(&authority.capabilities)
    {
        return Err(Error::Refused("scope or capability ceiling".into()));
    }
    Ok(())
}
fn next_id(s: &mut State, prefix: &str) -> String {
    s.sequence += 1;
    format!("{prefix}-{:016x}", s.sequence)
}
fn receipt(s: &mut State, run: &str, step: &str, now: i64, kind: &str, detail: Value) {
    s.sequence += 1;
    if let Some(r) = s.runs.get(run) {
        s.indexes
            .root_last_activity
            .entry(r.root.clone())
            .and_modify(|at| *at = (*at).max(now))
            .or_insert(now);
    }
    let offset = s.receipts.len();
    s.indexes
        .receipts
        .entry(run.into())
        .or_default()
        .insert(s.sequence, offset);
    s.indexes.all_receipts.insert(s.sequence, offset);
    s.receipts.push(Receipt {
        sequence: s.sequence,
        run: run.into(),
        step: step.into(),
        at_ms: now,
        kind: kind.into(),
        detail,
    });
}
fn resolve(s: &State, artifact: &str, authority: &Authority) -> Result<BTreeMap<String, String>> {
    resolve_at(s, artifact, authority, s.catalog_revision)
}
fn resolve_at(
    s: &State,
    artifact: &str,
    authority: &Authority,
    revision: u64,
) -> Result<BTreeMap<String, String>> {
    fn visit(
        s: &State,
        id: &str,
        a: &Authority,
        stack: &mut BTreeSet<String>,
        out: &mut BTreeMap<String, String>,
        revision: u64,
    ) -> Result<()> {
        if !stack.insert(id.into()) {
            return Err(Error::Refused("recursive dependency".into()));
        }
        let artifact = s
            .artifacts
            .get(id)
            .ok_or_else(|| Error::NotFound(id.into()))?;
        artifact.verify()?;
        check_authority(artifact, a)?;
        for (name, dep) in &artifact.content.dependencies {
            let selected = match &dep.selection {
                DependencySelection::Exact => dep.artifact.clone(),
                DependencySelection::Catalog { allowed_artifacts } => {
                    let bindings = s.catalog_history.get(&revision).unwrap_or(&s.bindings);
                    let binding = bindings
                        .get(&dep.binding)
                        .ok_or_else(|| Error::NotFound(dep.binding.clone()))?;
                    if !binding.controls.calls_enabled {
                        return Err(Error::Refused("dependency new calls disabled".into()));
                    }
                    if !allowed_artifacts.is_empty()
                        && !allowed_artifacts.contains(&binding.artifact)
                    {
                        return Err(Error::Refused(
                            "catalog target outside allowed artifacts".into(),
                        ));
                    }
                    binding.artifact.clone()
                }
            };
            let child = s
                .artifacts
                .get(&selected)
                .ok_or_else(|| Error::NotFound(selected.clone()))?;
            if !execution_allowed(
                s,
                &selected,
                a,
                &child.content.playbook.required_capabilities,
            )? {
                return Err(Error::Refused(
                    "dependency execution authority revoked".into(),
                ));
            }
            if dep.scope != a.scope || !dep.capabilities.is_subset(&a.capabilities) {
                return Err(Error::Refused("dependency authority".into()));
            }
            for (entry, interface) in &dep.interfaces {
                let actual = child
                    .content
                    .playbook
                    .entries
                    .get(entry)
                    .ok_or_else(|| Error::Refused("missing child interface".into()))?;
                if !interface_compatible(actual, interface) {
                    return Err(Error::Refused("incompatible child interface".into()));
                }
            }
            out.insert(format!("{id}/{name}"), selected.clone());
            visit(s, &selected, a, stack, out, revision)?;
        }
        stack.remove(id);
        Ok(())
    }
    let mut out = BTreeMap::new();
    visit(
        s,
        artifact,
        authority,
        &mut BTreeSet::new(),
        &mut out,
        revision,
    )?;
    Ok(out)
}
#[allow(clippy::too_many_arguments)]
fn admit_run(
    s: &mut State,
    id: String,
    root: String,
    parent: Option<String>,
    origin: Option<String>,
    artifact: &str,
    entry: &str,
    input: Value,
    authority: Authority,
    dependencies: BTreeMap<String, String>,
    depth: u32,
    now: i64,
) -> Result<()> {
    if s.runs.contains_key(&id) {
        return Ok(());
    }
    ensure_admission_protection(
        s,
        artifact,
        &dependencies,
        s.runs
            .get(&root)
            .map(|run| run.catalog_revision)
            .unwrap_or(s.catalog_revision),
    )?;
    let a = s
        .artifacts
        .get(artifact)
        .ok_or_else(|| Error::NotFound(artifact.into()))?;
    check_authority(a, &authority)?;
    let e = a
        .content
        .playbook
        .entries
        .get(entry)
        .ok_or_else(|| Error::NotFound(entry.into()))?;
    e.input.validate(&input)?;
    check_bytes(&input, a.content.playbook.limits.max_payload_bytes)?;
    let step = e.start.clone();
    s.runs.insert(
        id.clone(),
        Run {
            catalog_revision: s
                .runs
                .get(&root)
                .map(|run| run.catalog_revision)
                .unwrap_or(s.catalog_revision),
            id: id.clone(),
            root: root.clone(),
            parent,
            origin,
            artifact: artifact.into(),
            entry: entry.into(),
            dependencies,
            authority,
            step: step.clone(),
            status: RunStatus::Admitted,
            input,
            variables: BTreeMap::new(),
            outcome: None,
            output: None,
            reason: None,
            admitted_at_ms: now,
            visits: 0,
            depth,
            children: vec![],
            pending: None,
            loops: vec![],
            attempts: BTreeMap::new(),
            pending_return: None,
            pending_failure: false,
            terminal_sequence: None,
        },
    );
    s.budgets.entry(root).or_default();
    indexes::mark_run(s, &id);
    receipt(s, &id, &step, now, "admitted", json!({"artifact":artifact}));
    Ok(())
}
fn context(s: &State, r: &Run) -> EvalContext {
    let mut c = EvalContext::new();
    c.insert("input".into(), EvalValue::Known(r.input.clone()));
    c.insert("vars".into(), EvalValue::Known(json!(r.variables)));
    for (k, v) in &r.variables {
        c.insert(k.clone(), EvalValue::Known(v.clone()));
    }
    let definitions = &s.artifacts[&r.artifact].content.definitions;
    c.insert(
        "definitions".into(),
        EvalValue::Known(Value::Object(
            definitions
                .iter()
                .map(|(k, v)| (k.clone(), v.value.clone()))
                .collect(),
        )),
    );
    c
}
fn known_expr(expr: &Expr, ctx: &EvalContext) -> Result<Value> {
    match eval(expr, ctx, &EvalLimits::default())? {
        EvalValue::Known(v) => Ok(v),
        EvalValue::Missing => Err(Error::Refused("expression input missing".into())),
        EvalValue::Unknown(r) => Err(Error::Refused(format!("expression unknown: {r}"))),
    }
}
fn find_event(s: &State, reference: &ContractRef) -> Result<EventContract> {
    for a in s.artifacts.values() {
        if let Some(c) = a.content.registry.events.get(&reference.name)
            && c.fingerprint()? == reference.fingerprint
        {
            return Ok(c.clone());
        }
    }
    Err(Error::Refused("unknown event contract".into()))
}
fn advance(s: &mut State, id: &str, now: i64) -> Result<bool> {
    process_completions(s, now)?;
    process_operation_deadlines(s, now);
    indexes::unqueue(s, id);
    let mut r = s
        .runs
        .get(id)
        .ok_or_else(|| Error::NotFound(id.into()))?
        .clone();
    if r.status.terminal() {
        return Ok(false);
    }
    let before = serde_json::to_value(&r).map_err(|e| Error::Refused(e.to_string()))?;
    let checkpoint = s.clone();
    let original = r.clone();
    let result = advance_inner(s, &mut r, now);
    if let Err(error) = result {
        *s = checkpoint;
        r = original;
        let message = error.to_string();
        if matches!(error, Error::Exhausted(_)) {
            fail_run(s, &mut r, message, now)?;
        } else {
            let step = s.artifacts[&r.artifact]
                .content
                .playbook
                .steps
                .get(&r.step)
                .cloned();
            receipt(s, &r.id, &r.step, now, "error", json!({"reason":message}));
            if let Some(frame) = r.loops.last().cloned() {
                match frame.failure {
                    IterationFailure::Continue => {
                        r.step = frame.site;
                        r.pending = None;
                        r.status = RunStatus::Running;
                    }
                    IterationFailure::Stop => {
                        r.loops.pop();
                        if let Some(previous) = frame.previous {
                            r.variables.insert(frame.item, previous);
                        } else {
                            r.variables.remove(&frame.item);
                        }
                        let target = s.artifacts[&r.artifact].content.playbook.steps[&frame.site]
                            .on_error
                            .clone();
                        if let Some(target) = target {
                            r.step = target;
                            r.pending = None;
                            r.status = RunStatus::Running;
                        } else {
                            r.status = RunStatus::Blocked;
                            r.reason = Some(message);
                        }
                    }
                }
            } else if let Some(target) = step.and_then(|x| x.on_error) {
                r.step = target;
                r.pending = None;
                r.status = RunStatus::Running;
            } else {
                r.status = RunStatus::Blocked;
                r.reason = Some(message);
            }
        }
    }
    let changed = before != serde_json::to_value(&r).map_err(|e| Error::Refused(e.to_string()))?;
    s.runs.insert(id.into(), r);
    indexes::mark_run(s, id);
    Ok(changed)
}
fn advance_inner(s: &mut State, r: &mut Run, now: i64) -> Result<()> {
    if r.status == RunStatus::Cancelling {
        settle_cancel(s, r, now)?;
        return Ok(());
    }
    if r.status == RunStatus::Finishing {
        if settled(s, r) {
            if r.pending_failure {
                finish(s, r, RunStatus::Failed, None, None, now);
            } else {
                let (outcome, output) = r.pending_return.take().unwrap();
                finish(s, r, RunStatus::Completed, Some(outcome), Some(output), now);
            }
        }
        return Ok(());
    }
    if !execution_allowed(
        s,
        &r.artifact,
        &r.authority,
        &s.artifacts[&r.artifact]
            .content
            .playbook
            .required_capabilities,
    )? {
        r.status = RunStatus::Blocked;
        r.reason = Some("execution authority revoked".into());
        return Ok(());
    }
    if r.status == RunStatus::Blocked {
        return Ok(());
    }
    let own_limits = &s.artifacts[&r.artifact].content.playbook.limits;
    let root_run = &s.runs[&r.root];
    let root_limits = &s.artifacts[&root_run.artifact].content.playbook.limits;
    if now.saturating_sub(root_run.admitted_at_ms) >= root_limits.max_duration_ms as i64
        || now.saturating_sub(r.admitted_at_ms) > own_limits.max_duration_ms as i64
    {
        return Err(Error::Exhausted("duration".into()));
    }
    let history = canonical_bytes(&(
        s.receipts
            .iter()
            .filter(|receipt| {
                s.runs
                    .get(&receipt.run)
                    .is_some_and(|run| run.root == r.root)
            })
            .collect::<Vec<_>>(),
        s.intents
            .values()
            .filter(|intent| {
                s.runs
                    .get(&intent.run)
                    .is_some_and(|run| run.root == r.root)
            })
            .collect::<Vec<_>>(),
        s.runs
            .values()
            .filter(|run| run.root == r.root)
            .collect::<Vec<_>>(),
    ))?
    .len();
    if history >= root_limits.max_history_bytes {
        return Err(Error::Exhausted("retained history".into()));
    }
    if let Some(pending) = r.pending.clone() {
        return poll_pending(s, r, pending, now);
    }
    let a = s.artifacts[&r.artifact].clone();
    let limits = &a.content.playbook.limits;
    let root = &s.runs[&r.root];
    let root_limits = &s.artifacts[&root.artifact].content.playbook.limits;
    let budget = s.budgets.get_mut(&r.root).unwrap();
    if budget.visits >= root_limits.max_step_visits || r.visits >= limits.max_step_visits {
        return Err(Error::Exhausted("step visits".into()));
    }
    if now.saturating_sub(r.admitted_at_ms) > limits.max_duration_ms as i64 {
        return Err(Error::Exhausted("duration".into()));
    }
    budget.visits += 1;
    r.visits += 1;
    r.status = RunStatus::Running;
    let step = a
        .content
        .playbook
        .steps
        .get(&r.step)
        .ok_or_else(|| Error::NotFound(r.step.clone()))?
        .clone();
    let ctx = context(s, r);
    match step.op {
        Operation::Decide { predicate } => {
            let outcome = match eval(&predicate, &ctx, &EvalLimits::default())? {
                EvalValue::Known(Value::Bool(true)) => "true",
                EvalValue::Known(Value::Bool(false)) => "false",
                EvalValue::Missing | EvalValue::Unknown(_) => "unknown",
                _ => return Err(Error::Refused("predicate must be boolean".into())),
            };
            follow(s, r, &step.edges, outcome, now)?;
        }
        Operation::Invoke {
            command,
            input,
            bind: _,
            retry: _,
        } => {
            let contract = a
                .content
                .registry
                .commands
                .get(&command.name)
                .ok_or_else(|| Error::Refused("unknown command".into()))?
                .clone();
            if contract.fingerprint()? != command.fingerprint {
                return Err(Error::Conflict("command fingerprint".into()));
            }
            if !contract.capabilities.is_subset(&r.authority.capabilities)
                || !contract.scopes.contains(&r.authority.scope)
            {
                return Err(Error::Refused("command authority ceiling".into()));
            }
            // Confidential persistence requires a separately configured protection provider.
            if (!contract.sensitive_input_fields.is_empty()
                || !contract.sensitive_output_fields.is_empty())
                && !s.protection_available
            {
                return Err(Error::Refused("protected-value storage unavailable".into()));
            }
            let input = known_expr(&input, &ctx)?;
            contract.input.validate(&input)?;
            check_bytes(
                &input,
                contract
                    .max_payload_bytes
                    .min(limits.max_payload_bytes)
                    .min(root_limits.max_payload_bytes),
            )?;
            if s.intents
                .values()
                .filter(|i| {
                    s.runs.get(&i.run).is_some_and(|x| x.root == r.root)
                        && !matches!(
                            i.status,
                            IntentStatus::Completed
                                | IntentStatus::Rejected
                                | IntentStatus::Cancelled
                        )
                })
                .count()
                >= root_limits.max_concurrent_effects as usize
                || s.intents
                    .values()
                    .filter(|intent| {
                        intent.run == r.id
                            && !matches!(
                                intent.status,
                                IntentStatus::Completed
                                    | IntentStatus::Rejected
                                    | IntentStatus::Cancelled
                            )
                    })
                    .count()
                    >= limits.max_concurrent_effects as usize
            {
                return Err(Error::Refused("outstanding effect limit".into()));
            }
            let id = digest(&(
                &r.id,
                &r.step,
                r.visits,
                &r.loops
                    .iter()
                    .map(|f| (&f.site, f.index))
                    .collect::<Vec<_>>(),
                r.attempts.get(&attempt_key(r)?),
            ))?;
            let deadline_at_ms = deadline(now, contract.deadline_ms)?
                .min(deadline(r.admitted_at_ms, limits.max_duration_ms)?)
                .min(deadline(root.admitted_at_ms, root_limits.max_duration_ms)?);
            s.intents.insert(
                id.clone(),
                Intent {
                    id: id.clone(),
                    run: r.id.clone(),
                    step: r.step.clone(),
                    invocation: r.visits,
                    deadline_at_ms,
                    input_digest: digest(&input)?,
                    input,
                    command: contract,
                    authority: r.authority.clone(),
                    status: IntentStatus::Prepared,
                    cancel_requested: false,
                    cancel_dispatched: false,
                    handle: None,
                    result: None,
                    reason: None,
                    late_results: vec![],
                    completion_event: None,
                },
            );
            indexes::mark_intent(s, &id);
            r.pending = Some(Pending::Effect { intent: id.clone() });
            r.status = RunStatus::Waiting;
            receipt(
                s,
                &r.id,
                &r.step,
                now,
                "intent",
                json!({"operation":id,"input_digest":s.intents[&id].input_digest}),
            );
        }
        Operation::Wait {
            event,
            correlation,
            minimum_revision,
            deadline_ms,
            bind,
        } => {
            if let Some(e) = &event {
                let c = find_event(s, e)?;
                if c.retention_ms < deadline_ms {
                    return Err(Error::Refused(
                        "wait exceeds supported event retention horizon".into(),
                    ));
                }
            }
            let mut corr = BTreeMap::new();
            for (k, v) in correlation {
                let v = known_expr(&v, &ctx)?;
                let v = v
                    .as_str()
                    .ok_or_else(|| Error::Refused("correlation must be string".into()))?;
                corr.insert(k, v.into());
            }
            if event.is_some() && corr.is_empty() {
                return Err(Error::Refused(
                    "event wait requires exact invocation correlation".into(),
                ));
            }
            let minimum = minimum_revision
                .map(|e| {
                    known_expr(&e, &ctx).and_then(|v| {
                        v.as_u64().ok_or_else(|| {
                            Error::Refused("minimum revision must be nonnegative integer".into())
                        })
                    })
                })
                .transpose()?;
            if let Some(reference) = &event {
                let key = indexes::correlation_key(reference, &r.authority.scope, &corr)?;
                if s.retention.expired_correlations.contains_key(&key) {
                    return Err(Error::Refused(
                        "required correlation history expired; explicit new invocation required"
                            .into(),
                    ));
                }
            }
            let id = digest(&(&r.id, &r.step, r.visits))?;
            s.waits.insert(
                id.clone(),
                WaitRecord {
                    id: id.clone(),
                    run: r.id.clone(),
                    step: r.step.clone(),
                    event,
                    scope: r.authority.scope.clone(),
                    correlation: corr,
                    minimum_revision: minimum,
                    deadline: deadline(now, deadline_ms)?,
                    bind,
                    winner: None,
                    expired: false,
                },
            );
            r.pending = Some(Pending::Event { wait: id });
            r.status = RunStatus::Waiting;
            poll_pending(s, r, r.pending.clone().unwrap(), now)?;
        }
        Operation::ForEach {
            collection,
            item,
            body,
            next,
            max_items,
            concurrency,
            on_failure,
        } => {
            if concurrency != 1 {
                return Err(Error::Refused(
                    "parallel foreach unsupported; use owned async calls and joins".into(),
                ));
            }
            if r.loops.last().is_some_and(|f| f.site == r.step) {
                let mut frame = r.loops.pop().unwrap();
                frame.index += 1;
                if frame.index >= frame.items.len() {
                    if let Some(previous) = frame.previous {
                        r.variables.insert(frame.item, previous);
                    } else {
                        r.variables.remove(&frame.item);
                    }
                    r.step = next;
                } else {
                    reserve_iteration(s, r, limits)?;
                    r.variables
                        .insert(frame.item.clone(), frame.items[frame.index].clone());
                    r.step = frame.body.clone();
                    r.loops.push(frame);
                }
            } else {
                let values = known_expr(&collection, &ctx)?;
                let items = values
                    .as_array()
                    .ok_or_else(|| Error::Refused("foreach collection must be array".into()))?
                    .clone();
                if items.len() > max_items {
                    return Err(Error::Refused("foreach item bound exceeded".into()));
                }
                if r.loops.len() >= limits.max_iteration_depth as usize {
                    return Err(Error::Refused("iteration depth exceeded".into()));
                }
                if items.is_empty() {
                    r.step = next;
                } else {
                    reserve_iteration(s, r, limits)?;
                    let previous = r.variables.insert(item.clone(), items[0].clone());
                    r.loops.push(LoopFrame {
                        site: r.step.clone(),
                        item,
                        items,
                        index: 0,
                        body: body.clone(),
                        next,
                        failure: on_failure,
                        previous,
                    });
                    r.step = body;
                }
            }
            receipt(
                s,
                &r.id,
                &r.step,
                now,
                "iteration",
                json!({"path":r.loops.iter().map(|x|x.index).collect::<Vec<_>>()}),
            );
        }
        Operation::Call {
            dependency,
            entry,
            input,
            mode,
            bind,
        } => {
            if let Some(dependency_contract) = a.content.dependencies.get(&dependency)
                && s.bindings
                    .get(&dependency_contract.binding)
                    .is_some_and(|binding| !binding.controls.calls_enabled)
            {
                return Err(Error::Refused("new child calls disabled".into()));
            }
            let child_artifact = r
                .dependencies
                .get(&format!("{}/{}", r.artifact, dependency))
                .ok_or_else(|| Error::Refused("dependency absent from frozen manifest".into()))?
                .clone();
            let child = &s.artifacts[&child_artifact];
            check_authority(child, &r.authority)?;
            if r.depth + 1 > root_limits.max_depth || r.depth + 1 > limits.max_depth {
                return Err(Error::Exhausted("child depth".into()));
            }
            let budget = s.budgets.get_mut(&r.root).unwrap();
            if budget.children >= root_limits.max_children
                || s.runs
                    .values()
                    .filter(|child| child.origin.as_deref() == Some(&r.id))
                    .count()
                    >= limits.max_children as usize
            {
                return Err(Error::Exhausted("child admissions".into()));
            }
            if s.runs
                .values()
                .filter(|child| child.origin.as_deref() == Some(&r.id) && !child.status.terminal())
                .count()
                >= limits.max_concurrent_children as usize
                || s.runs
                    .values()
                    .filter(|child| {
                        child.root == r.root && child.id != r.root && !child.status.terminal()
                    })
                    .count()
                    >= root_limits.max_concurrent_children as usize
            {
                return Err(Error::Refused("concurrent child limit".into()));
            }
            let input = known_expr(&input, &ctx)?;
            check_bytes(
                &input,
                limits.max_payload_bytes.min(root_limits.max_payload_bytes),
            )?;
            let id = digest(&(
                &r.id,
                &r.step,
                r.visits,
                r.loops
                    .iter()
                    .map(|f| (&f.site, f.index))
                    .collect::<Vec<_>>(),
            ))?;
            budget.children += 1;
            let independent = mode == CallMode::Independent;
            let parent = if independent {
                None
            } else {
                Some(r.id.clone())
            };
            // Independent starts retain the original shared allocation ledger. They
            // cannot replenish quotas by detaching, even after the origin finishes.
            admit_run(
                s,
                id.clone(),
                r.root.clone(),
                parent,
                Some(r.id.clone()),
                &child_artifact,
                &entry,
                input,
                r.authority.clone(),
                r.dependencies.clone(),
                r.depth + 1,
                now,
            )?;
            if !independent {
                r.children.push(id.clone());
            }
            receipt(
                s,
                &r.id,
                &r.step,
                now,
                "child_admitted",
                json!({"child":id,"mode":mode}),
            );
            if mode == CallMode::Wait {
                r.pending = Some(Pending::Child { child: id, bind });
                r.status = RunStatus::Waiting;
            } else {
                r.variables.insert(bind, json!({"child":id}));
                follow(s, r, &step.edges, "started", now)?;
            }
        }
        Operation::Join {
            members,
            member_keys,
            mode,
            accepted_outcomes,
            deadline_ms,
            remaining,
            bind,
        } => {
            let mut ids = vec![];
            for m in members {
                let value = known_expr(&m, &ctx)?;
                let member = if let Some(child) = value
                    .get("child")
                    .and_then(Value::as_str)
                    .or_else(|| value.as_str())
                {
                    if !r.children.iter().any(|x| x == child) {
                        return Err(Error::Refused("join requires owned child handle".into()));
                    }
                    child.to_string()
                } else if let Some(handle) = value.get("handle").and_then(Value::as_str) {
                    s.intents
                        .values()
                        .find(|i| i.run == r.id && i.handle.as_deref() == Some(handle))
                        .ok_or_else(|| {
                            Error::Refused("join requires owned operation handle".into())
                        })?
                        .id
                        .clone()
                } else {
                    return Err(Error::Refused(
                        "join member must be owned child or operation handle".into(),
                    ));
                };
                if ids.contains(&member) {
                    return Err(Error::Refused("duplicate join member".into()));
                }
                ids.push(member);
            }
            if ids.len() > limits.max_children as usize {
                return Err(Error::Refused("join member bound".into()));
            }
            r.pending = Some(Pending::Join {
                members: ids,
                member_keys,
                mode,
                accepted: accepted_outcomes,
                deadline: deadline(now, deadline_ms)?,
                remaining,
                bind,
            });
            r.status = RunStatus::Waiting;
            poll_pending(s, r, r.pending.clone().unwrap(), now)?;
        }
        Operation::Return { outcome, output } => {
            let output = known_expr(&output, &ctx)?;
            let entry = &a.content.playbook.entries[&r.entry];
            entry
                .outcomes
                .get(&outcome)
                .ok_or_else(|| Error::Refused("undeclared return outcome".into()))?
                .validate(&output)?;
            check_bytes(
                &output,
                limits.max_payload_bytes.min(root_limits.max_payload_bytes),
            )?;
            if settled(s, r) {
                finish(s, r, RunStatus::Completed, Some(outcome), Some(output), now);
            } else {
                r.pending_return = Some((outcome, output));
                r.status = RunStatus::Finishing;
                receipt(s, &r.id, &r.step, now, "finishing", Value::Null);
            }
        }
    }
    Ok(())
}
fn deadline(now: i64, duration: u64) -> Result<i64> {
    let duration =
        i64::try_from(duration).map_err(|_| Error::Refused("deadline overflow".into()))?;
    now.checked_add(duration)
        .ok_or_else(|| Error::Refused("deadline overflow".into()))
}
fn reserve_iteration(s: &mut State, r: &Run, limits: &Limits) -> Result<()> {
    let root = &s.runs[&r.root];
    let bound = s.artifacts[&root.artifact]
        .content
        .playbook
        .limits
        .max_iterations
        .min(limits.max_iterations);
    let b = s.budgets.get_mut(&r.root).unwrap();
    if b.iterations >= bound {
        return Err(Error::Exhausted("iterations".into()));
    }
    b.iterations += 1;
    Ok(())
}
fn follow(
    s: &mut State,
    r: &mut Run,
    edges: &BTreeMap<String, String>,
    outcome: &str,
    now: i64,
) -> Result<()> {
    let target = edges
        .get(outcome)
        .ok_or_else(|| Error::Refused(format!("unhandled outcome {outcome}")))?;
    receipt(
        s,
        &r.id,
        &r.step,
        now,
        "branch",
        json!({"outcome":outcome,"next":target}),
    );
    r.step = target.clone();
    r.pending = None;
    r.status = RunStatus::Running;
    Ok(())
}
fn finish(
    s: &mut State,
    r: &mut Run,
    status: RunStatus,
    outcome: Option<String>,
    output: Option<Value>,
    now: i64,
) {
    r.status = status;
    r.outcome = outcome;
    r.output = output;
    r.pending = None;
    receipt(
        s,
        &r.id,
        &r.step,
        now,
        "terminal",
        json!({"status":r.status,"outcome":r.outcome}),
    );
    r.terminal_sequence = Some(s.sequence);
    indexes::wake_dependents(s, &r.id);
}
fn settled(s: &State, r: &Run) -> bool {
    r.children.iter().all(|id| s.runs[id].status.terminal())
        && s.intents.values().filter(|i| i.run == r.id).all(|i| {
            matches!(
                i.status,
                IntentStatus::Completed | IntentStatus::Rejected | IntentStatus::Cancelled
            )
        })
}
fn poll_pending(s: &mut State, r: &mut Run, pending: Pending, now: i64) -> Result<()> {
    let step = s.artifacts[&r.artifact].content.playbook.steps[&r.step].clone();
    match pending {
        Pending::Retry { deadline } => {
            if now >= deadline {
                r.pending = None;
                r.status = RunStatus::Running;
            }
        }
        Pending::Effect { intent } => {
            let i = s.intents[&intent].clone();
            match i.status {
                IntentStatus::Completed => {
                    let (outcome, output) = i.result.unwrap();
                    if let Operation::Invoke { bind, retry, .. } = step.op {
                        if let Some(policy) = retry
                            && policy.outcomes.contains(&outcome)
                        {
                            let key = attempt_key(r)?;
                            let attempts = r.attempts.entry(key).or_insert(1);
                            if *attempts < policy.max_attempts {
                                *attempts += 1;
                                r.pending = Some(Pending::Retry {
                                    deadline: deadline(now, policy.backoff_ms)?,
                                });
                                receipt(
                                    s,
                                    &r.id,
                                    &r.step,
                                    now,
                                    "business_retry",
                                    json!({"attempt":attempts}),
                                );
                                return Ok(());
                            }
                        }
                        r.variables.insert(bind, output);
                    }
                    follow(s, r, &step.edges, &outcome, now)?;
                }
                IntentStatus::Rejected => {
                    return Err(Error::Refused("command admission rejected".into()));
                }
                IntentStatus::Unknown => {
                    r.status = RunStatus::Blocked;
                    r.reason = i.reason;
                }
                IntentStatus::Accepted if step.edges.contains_key("accepted") => {
                    if let Operation::Invoke { bind, .. } = &step.op {
                        let value = json!({"handle":i.handle});
                        i.command
                            .outcomes
                            .get("accepted")
                            .ok_or_else(|| {
                                Error::Refused(
                                    "accepted edge missing declared admission schema".into(),
                                )
                            })?
                            .validate(&value)?;
                        r.variables.insert(bind.clone(), value);
                    }
                    follow(s, r, &step.edges, "accepted", now)?;
                }
                _ => {}
            }
        }
        Pending::Event { wait } => {
            let mut w = s.waits[&wait].clone();
            let keys = if let Some(reference) = &w.event {
                s.indexes
                    .events
                    .get(&indexes::correlation_key(
                        reference,
                        &w.scope,
                        &w.correlation,
                    )?)
                    .cloned()
                    .unwrap_or_default()
            } else {
                BTreeMap::new()
            };
            let winner = keys
                .values()
                .filter_map(|id| s.events.get(id))
                .filter(|e| {
                    w.event
                        .as_ref()
                        .is_some_and(|reference| e.envelope.contract == *reference)
                        && e.envelope.scope == w.scope
                        && e.accepted_at_ms <= w.deadline
                        && w.correlation
                            .iter()
                            .all(|(k, v)| e.envelope.correlation.get(k) == Some(v))
                        && w.minimum_revision
                            .is_none_or(|rev| e.envelope.revision.is_some_and(|r| r >= rev))
                })
                .min_by_key(|e| e.sequence)
                .cloned();
            if let Some(event) = winner {
                w.winner = Some(event.key.clone());
                r.variables.insert(w.bind.clone(), event.envelope.payload);
                s.waits.insert(wait, w);
                follow(s, r, &step.edges, "received", now)?;
            } else if now >= w.deadline {
                w.expired = true;
                s.waits.insert(wait, w);
                follow(s, r, &step.edges, "timeout", now)?;
            }
        }
        Pending::Child { child, bind } => {
            let c = s.runs[&child].clone();
            if c.status.terminal() {
                let outcome = c.outcome.unwrap_or_else(|| {
                    match c.status {
                        RunStatus::Cancelled => "cancelled",
                        _ => "failed",
                    }
                    .into()
                });
                r.variables.insert(bind, c.output.unwrap_or(Value::Null));
                follow(s, r, &step.edges, &outcome, now)?;
            }
        }
        Pending::Join {
            members,
            member_keys,
            mode,
            accepted,
            deadline,
            remaining,
            bind,
        } => {
            let mut completed: Vec<_> = members
                .iter()
                .filter_map(|id| participant(s, id))
                .filter(|p| p.accepted_at_ms <= deadline)
                .collect();
            completed.sort_by_key(|p| p.sequence);
            let qualifies = |p: &Participant| {
                p.success
                    && (accepted.is_empty()
                        || p.outcome.as_ref().is_some_and(|o| accepted.contains(o)))
            };
            let outcome = match mode {
                JoinMode::All => {
                    if completed.len() == members.len() {
                        Some(if completed.iter().all(qualifies) {
                            "completed"
                        } else {
                            "no_match"
                        })
                    } else {
                        None
                    }
                }
                JoinMode::AnyCompletion => {
                    if !completed.is_empty() {
                        Some("completed")
                    } else {
                        None
                    }
                }
                JoinMode::AnyAccepted => {
                    if completed.iter().any(qualifies) {
                        Some("completed")
                    } else if completed.len() == members.len() {
                        Some("no_match")
                    } else {
                        None
                    }
                }
            }
            .or_else(|| (now >= deadline).then_some("timeout"));
            if let Some(outcome) = outcome {
                let selected: Vec<_> = match mode {
                    JoinMode::AnyAccepted => {
                        completed.iter().filter(|p| qualifies(p)).take(1).collect()
                    }
                    JoinMode::AnyCompletion => completed.iter().take(1).collect(),
                    JoinMode::All => completed.iter().collect(),
                };
                let result: serde_json::Map<_, _> = selected
                    .into_iter()
                    .map(|p| {
                        let key = members
                            .iter()
                            .position(|id| id == &p.id)
                            .and_then(|i| member_keys.get(i))
                            .cloned()
                            .unwrap_or_else(|| p.id.clone());
                        let value = if member_keys.is_empty() {
                            json!({"outcome":p.outcome,"output":p.output})
                        } else {
                            json!({"participant":p.id,"outcome":p.outcome,"output":p.output})
                        };
                        (key, value)
                    })
                    .collect();
                r.variables.insert(bind, Value::Object(result));
                let unfinished: Vec<_> = members
                    .into_iter()
                    .filter(|id| participant(s, id).is_none())
                    .collect();
                if remaining == RemainingPolicy::Cancel {
                    for id in unfinished {
                        if s.runs.contains_key(&id) {
                            request_cancel(s, &id, now)?;
                        } else {
                            let i = s.intents.get_mut(&id).unwrap();
                            i.cancel_requested = true;
                            indexes::mark_intent(s, &id);
                        }
                    }
                }
                follow(s, r, &step.edges, outcome, now)?;
            }
        }
    }
    Ok(())
}
fn record_result(s: &mut State, intent_id: &str, result: DispatchResult, now: i64) -> Result<()> {
    indexes::mark_intent(s, intent_id);
    let mut i = s
        .intents
        .get(intent_id)
        .ok_or_else(|| Error::NotFound(intent_id.into()))?
        .clone();
    if i.status == IntentStatus::Cancelled {
        if let DispatchResult::Completed { outcome, output } = result
            && !i.late_results.contains(&(outcome.clone(), output.clone()))
        {
            i.command
                .outcomes
                .get(&outcome)
                .ok_or_else(|| Error::Refused("undeclared late command outcome".into()))?
                .validate(&output)?;
            check_bytes(&output, i.command.max_payload_bytes)?;
            i.late_results.push((outcome, output));
            receipt(
                s,
                &i.run,
                &i.step,
                now,
                "late_result",
                json!({"operation":i.id}),
            );
            s.intents.insert(intent_id.into(), i);
        }
        return Ok(());
    }
    if matches!(i.status, IntentStatus::Completed) {
        if let DispatchResult::Completed { outcome, output } = result
            && i.result != Some((outcome, output))
        {
            return Err(Error::Conflict("operation result changed".into()));
        }
        return Ok(());
    }
    match result {
        DispatchResult::Completed { outcome, output } => {
            let validation = i
                .command
                .outcomes
                .get(&outcome)
                .ok_or_else(|| Error::Refused("undeclared command outcome".into()))
                .and_then(|schema| schema.validate(&output).map_err(Error::from))
                .and_then(|_| {
                    let run = &s.runs[&i.run];
                    let root = &s.runs[&run.root];
                    check_bytes(
                        &output,
                        i.command
                            .max_payload_bytes
                            .min(
                                s.artifacts[&run.artifact]
                                    .content
                                    .playbook
                                    .limits
                                    .max_payload_bytes,
                            )
                            .min(
                                s.artifacts[&root.artifact]
                                    .content
                                    .playbook
                                    .limits
                                    .max_payload_bytes,
                            ),
                    )
                });
            if validation.is_err() {
                i.status = IntentStatus::Unknown;
                i.reason=Some("command returned invalid outcome/output; external effect requires reconciliation".into());
                let run = s
                    .runs
                    .get_mut(&i.run)
                    .ok_or_else(|| Error::NotFound(i.run.clone()))?;
                if !matches!(run.status, RunStatus::Cancelling | RunStatus::Finishing) {
                    run.status = RunStatus::Blocked;
                }
                run.reason = i.reason.clone();
                receipt(
                    s,
                    &i.run,
                    &i.step,
                    now,
                    "invalid_output",
                    json!({"operation":i.id}),
                );
                s.intents.insert(intent_id.into(), i);
                return Ok(());
            }
            let public = output
                .as_object()
                .map(|o| {
                    Value::Object(
                        o.iter()
                            .filter(|(k, _)| i.command.public_result_fields.contains(*k))
                            .map(|(k, v)| (k.clone(), v.clone()))
                            .collect(),
                    )
                })
                .unwrap_or(Value::Null);
            i.status = IntentStatus::Completed;
            i.result = Some((outcome.clone(), output));
            receipt(
                s,
                &i.run,
                &i.step,
                now,
                "command_result",
                json!({"operation":i.id,"outcome":outcome,"public":public}),
            );
        }
        DispatchResult::Accepted { handle } => {
            if let Some(previous) = &i.handle {
                if previous != &handle {
                    return Err(Error::Conflict("operation admission handle changed".into()));
                }
                if i.status == IntentStatus::Accepted {
                    return Ok(());
                }
            }
            if i.command.asynchronous.is_none() {
                return Err(Error::Refused(
                    "synchronous contract returned async admission".into(),
                ));
            }
            i.status = IntentStatus::Accepted;
            i.handle = Some(handle);
            receipt(
                s,
                &i.run,
                &i.step,
                now,
                "operation_accepted",
                json!({"operation":i.id}),
            );
        }
        DispatchResult::Unknown { reason } => {
            i.status = IntentStatus::Unknown;
            i.reason = Some(reason);
            receipt(
                s,
                &i.run,
                &i.step,
                now,
                "operation_unknown",
                json!({"operation":i.id}),
            );
        }
        DispatchResult::Rejected { reason } => {
            i.status = IntentStatus::Rejected;
            i.reason = Some(reason);
            receipt(
                s,
                &i.run,
                &i.step,
                now,
                "operation_rejected",
                json!({"operation":i.id}),
            );
        }
    }
    let run = s
        .runs
        .get_mut(&i.run)
        .ok_or_else(|| Error::NotFound(i.run.clone()))?;
    if run.status == RunStatus::Blocked && i.status == IntentStatus::Completed {
        run.status = RunStatus::Waiting;
        run.reason = None;
    }
    s.intents.insert(intent_id.into(), i);
    Ok(())
}
fn request_cancel(s: &mut State, id: &str, now: i64) -> Result<()> {
    let mut r = s
        .runs
        .get(id)
        .ok_or_else(|| Error::NotFound(id.into()))?
        .clone();
    if r.status.terminal() {
        return Ok(());
    }
    r.status = RunStatus::Cancelling;
    receipt(s, id, &r.step, now, "cancel_requested", Value::Null);
    for child in r.children.clone() {
        request_cancel(s, &child, now)?;
    }
    for i in s.intents.values_mut().filter(|i| i.run == id) {
        if i.status == IntentStatus::Prepared {
            i.status = IntentStatus::Cancelled;
        } else if !matches!(
            i.status,
            IntentStatus::Completed | IntentStatus::Rejected | IntentStatus::Cancelled
        ) {
            i.cancel_requested = true;
        }
    }
    s.runs.insert(id.into(), r);
    indexes::mark_run(s, id);
    let intents = s
        .intents
        .values()
        .filter(|i| i.run == id)
        .map(|i| i.id.clone())
        .collect::<Vec<_>>();
    for intent in intents {
        indexes::mark_intent(s, &intent);
    }
    Ok(())
}
fn settle_cancel(s: &mut State, r: &mut Run, now: i64) -> Result<()> {
    if settled(s, r) {
        finish(s, r, RunStatus::Cancelled, None, None, now);
    } else if s.intents.values().any(|i| {
        i.run == r.id
            && matches!(
                i.status,
                IntentStatus::Dispatched | IntentStatus::Unknown | IntentStatus::Accepted
            )
    }) {
        r.reason = Some("cancellation awaits external operation settlement".into());
    }
    Ok(())
}

/// Fake dispatch data is keyed by exact command name. Responses are consumed in
/// order and recorded by the same Engine used by embedded production hosts.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct FakeHost {
    pub responses: BTreeMap<String, Vec<DispatchResult>>,
    pub dispatched: Vec<String>,
    pub reconciliations: Vec<String>,
    pub revoked: BTreeSet<String>,
    pub cancellation_responses: BTreeMap<String, Vec<CancellationResult>>,
    pub cancelled: Vec<String>,
}
impl Host for FakeHost {
    fn authorize(&self, a: &Authority, c: &CommandContract) -> bool {
        c.capabilities.is_subset(&a.capabilities)
            && c.scopes.contains(&a.scope)
            && c.capabilities.is_disjoint(&self.revoked)
    }
    fn dispatch(&mut self, i: &Intent) -> DispatchResult {
        self.dispatched.push(i.id.clone());
        self.responses
            .get_mut(&i.command.name)
            .filter(|v| !v.is_empty())
            .map(|v| v.remove(0))
            .unwrap_or_else(|| DispatchResult::Unknown {
                reason: format!("missing mock for {}", i.command.name),
            })
    }
    fn reconcile(&mut self, i: &Intent) -> DispatchResult {
        self.reconciliations.push(i.id.clone());
        self.responses
            .get_mut(&format!("reconcile:{}", i.command.name))
            .filter(|v| !v.is_empty())
            .map(|v| v.remove(0))
            .unwrap_or_else(|| DispatchResult::Unknown {
                reason: "missing reconciliation mock".into(),
            })
    }
    fn cancel(&mut self, i: &Intent) -> CancellationResult {
        self.cancelled.push(i.id.clone());
        self.cancellation_responses
            .get_mut(&i.command.name)
            .filter(|v| !v.is_empty())
            .map(|v| v.remove(0))
            .unwrap_or(CancellationResult::Unknown)
    }
}
pub struct Simulation {
    pub engine: Engine<MemoryStorage>,
    pub host: FakeHost,
    pub now_ms: i64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SimulationReport {
    pub state: State,
    pub transitions: usize,
    pub incomplete: bool,
    pub unknown_operations: Vec<String>,
}
impl Simulation {
    pub fn new(host: FakeHost) -> Result<Self> {
        Ok(Self {
            engine: Engine::new(MemoryStorage::default(), "simulation", "fake-runtime")?,
            host,
            now_ms: 0,
        })
    }
    pub fn advance_clock(&mut self, now_ms: i64) -> Result<()> {
        if now_ms < self.now_ms {
            return Err(Error::Refused("virtual clock cannot move backwards".into()));
        }
        self.now_ms = now_ms;
        Ok(())
    }
    pub fn run(&mut self, max_transitions: usize) -> Result<SimulationReport> {
        let transitions = self
            .engine
            .drive(&mut self.host, self.now_ms, max_transitions)?;
        let state = self.engine.snapshot()?;
        let unknown_operations = state
            .intents
            .values()
            .filter(|i| i.status == IntentStatus::Unknown)
            .map(|i| i.id.clone())
            .collect();
        let incomplete = state.runs.values().any(|r| !r.status.terminal());
        Ok(SimulationReport {
            state,
            transitions,
            incomplete,
            unknown_operations,
        })
    }
}
struct Participant {
    id: String,
    sequence: u64,
    accepted_at_ms: i64,
    success: bool,
    outcome: Option<String>,
    output: Option<Value>,
}
fn participant(s: &State, id: &str) -> Option<Participant> {
    if let Some(c) = s.runs.get(id)
        && c.status.terminal()
    {
        return Some(Participant {
            id: id.into(),
            sequence: c.terminal_sequence.unwrap_or(u64::MAX),
            accepted_at_ms: s
                .receipts
                .iter()
                .find(|r| Some(r.sequence) == c.terminal_sequence)
                .map(|r| r.at_ms)
                .unwrap_or(i64::MAX),
            success: c.status == RunStatus::Completed,
            outcome: c.outcome.clone(),
            output: c.output.clone(),
        });
    }
    if let Some(i) = s.intents.get(id)
        && matches!(
            i.status,
            IntentStatus::Completed | IntentStatus::Rejected | IntentStatus::Cancelled
        )
    {
        let sequence = s
            .receipts
            .iter()
            .rev()
            .find(|r| {
                r.detail.get("operation").and_then(Value::as_str) == Some(id)
                    && matches!(
                        r.kind.as_str(),
                        "command_result" | "operation_rejected" | "cancel_response"
                    )
            })
            .map(|r| r.sequence)
            .unwrap_or(u64::MAX);
        return Some(Participant {
            id: id.into(),
            sequence,
            accepted_at_ms: s
                .receipts
                .iter()
                .find(|r| r.sequence == sequence)
                .map(|r| r.at_ms)
                .unwrap_or(i64::MAX),
            success: i.status == IntentStatus::Completed,
            outcome: i.result.as_ref().map(|r| r.0.clone()),
            output: i.result.as_ref().map(|r| r.1.clone()),
        });
    }
    None
}
fn process_completions(s: &mut State, _now: i64) -> Result<()> {
    let ids = s
        .indexes
        .completion_ready
        .iter()
        .take(64)
        .cloned()
        .collect::<Vec<_>>();
    let pending = ids
        .iter()
        .filter_map(|id| s.intents.get(id))
        .filter(|intent| intent.completion_event.is_none() && intent.handle.is_some())
        .cloned()
        .collect::<Vec<_>>();
    for id in ids {
        s.indexes.completion_ready.remove(&id);
    }
    for i in pending {
        let Some(asynchronous) = &i.command.asynchronous else {
            continue;
        };
        let artifact = &s.artifacts[&s.runs[&i.run].artifact];
        let expected = artifact
            .content
            .registry
            .events
            .get(&asynchronous.completion_event)
            .ok_or_else(|| Error::Refused("missing pinned completion contract".into()))?;
        let fingerprint = expected.fingerprint()?;
        let correlation = indexes::field_key(
            &ContractRef {
                name: expected.name.clone(),
                fingerprint: fingerprint.clone(),
            },
            &i.authority.scope,
            &asynchronous.correlation_field,
            i.handle.as_deref().unwrap_or_default(),
        )?;
        let keys = s
            .indexes
            .events
            .get(&correlation)
            .cloned()
            .unwrap_or_default();
        let event = keys
            .values()
            .filter_map(|id| s.events.get(id))
            .filter(|e| {
                e.envelope.contract.name == expected.name
                    && e.envelope.contract.fingerprint == fingerprint
                    && e.envelope.scope == i.authority.scope
                    && expected.trusted_producers.contains(&e.envelope.producer)
                    && e.envelope.correlation.get(&asynchronous.correlation_field)
                        == i.handle.as_ref()
            })
            .min_by_key(|e| e.sequence)
            .cloned();
        if let Some(event) = event {
            if asynchronous
                .completion
                .validate(&event.envelope.payload)
                .is_err()
            {
                let current = s.intents.get_mut(&i.id).unwrap();
                if current.status != IntentStatus::Cancelled {
                    current.status = IntentStatus::Unknown;
                }
                current.reason =
                    Some("invalid asynchronous completion requires reconciliation".into());
                current.completion_event = Some(event.key);
                indexes::mark_intent(s, &i.id);
                let run = s.runs.get_mut(&i.run).unwrap();
                if !run.status.terminal()
                    && !matches!(run.status, RunStatus::Finishing | RunStatus::Cancelling)
                {
                    run.status = RunStatus::Blocked;
                }
                if !run.status.terminal() {
                    run.reason =
                        Some("invalid asynchronous completion requires reconciliation".into());
                }
                receipt(
                    s,
                    &i.run,
                    &i.step,
                    event.accepted_at_ms,
                    "invalid_output",
                    json!({"operation":i.id}),
                );
                continue;
            }
            let outcome = event
                .envelope
                .payload
                .get("outcome")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::Refused("async completion missing outcome".into()))?;
            let output = event
                .envelope
                .payload
                .get("output")
                .cloned()
                .ok_or_else(|| Error::Refused("async completion missing output".into()))?;
            record_result(
                s,
                &i.id,
                DispatchResult::Completed {
                    outcome: outcome.into(),
                    output,
                },
                event.accepted_at_ms,
            )?;
            s.intents.get_mut(&i.id).unwrap().completion_event = Some(event.key);
        }
    }
    Ok(())
}
fn fail_run(s: &mut State, r: &mut Run, reason: String, now: i64) -> Result<()> {
    r.pending = None;
    r.reason = Some(reason.clone());
    r.pending_failure = true;
    for child in r.children.clone() {
        request_cancel(s, &child, now)?;
    }
    for intent in s.intents.values_mut().filter(|i| i.run == r.id) {
        if intent.status == IntentStatus::Prepared {
            intent.status = IntentStatus::Cancelled;
        } else if !matches!(
            intent.status,
            IntentStatus::Completed | IntentStatus::Rejected | IntentStatus::Cancelled
        ) {
            intent.cancel_requested = true;
        }
    }
    let intents = s
        .intents
        .values()
        .filter(|i| i.run == r.id)
        .map(|i| i.id.clone())
        .collect::<Vec<_>>();
    for intent in intents {
        indexes::mark_intent(s, &intent);
    }
    receipt(
        s,
        &r.id,
        &r.step,
        now,
        "engine_failure",
        json!({"reason":reason}),
    );
    if settled(s, r) {
        finish(s, r, RunStatus::Failed, None, None, now);
    } else {
        r.status = RunStatus::Finishing;
    }
    Ok(())
}

fn process_operation_deadlines(s: &mut State, now: i64) {
    let timers = s
        .indexes
        .query(
            s,
            &IndexQuery::DueTimers {
                now_ms: now,
                limit: 64,
            },
        )
        .items;
    let expired = timers
        .iter()
        .filter_map(|item| s.indexes.timers.get(&item.key))
        .filter_map(|timer| timer.intent.as_ref())
        .filter_map(|id| s.intents.get(id))
        .filter(|intent| {
            matches!(
                intent.status,
                IntentStatus::Dispatched | IntentStatus::Accepted
            )
        })
        .cloned()
        .collect::<Vec<_>>();
    for intent in expired {
        indexes::remove_timer(s, &format!("effect:{}", intent.id));
        indexes::mark_intent(s, &intent.id);
        let current = s.intents.get_mut(&intent.id).unwrap();
        current.status = IntentStatus::Unknown;
        current.reason = Some("operation deadline elapsed with unresolved external outcome".into());
        receipt(
            s,
            &intent.run,
            &intent.step,
            now,
            "command_deadline_expired",
            json!({"operation":intent.id,"issued":true}),
        );
    }
}

fn attempt_key(run: &Run) -> Result<String> {
    Ok(digest(&(
        &run.step,
        run.loops
            .iter()
            .map(|frame| (&frame.site, frame.index))
            .collect::<Vec<_>>(),
    ))?)
}

impl<S: Storage> Engine<S> {
    pub fn set_activation_controls(
        &self,
        binding: &str,
        controls: ActivationControls,
        now_ms: i64,
    ) -> Result<()> {
        self.transaction(|s| {
            let a = s
                .bindings
                .get_mut(binding)
                .ok_or_else(|| Error::NotFound(binding.into()))?;
            a.controls = controls.clone();
            a.revision += 1;
            let artifact = a.artifact.clone();
            if controls.execution_enabled {
                s.execution_denied.remove(&artifact);
            } else {
                s.execution_denied.insert(artifact.clone());
            }
            s.catalog_revision += 1;
            s.catalog_history
                .insert(s.catalog_revision, s.bindings.clone());
            s.catalog_history_at_ms.insert(s.catalog_revision, now_ms);
            indexes::index_activation(s, binding)?;
            let runs = s
                .runs
                .values()
                .filter(|r| r.artifact == artifact && !r.status.terminal())
                .map(|r| r.id.clone())
                .collect::<Vec<_>>();
            for id in runs {
                if controls.execution_enabled {
                    let r = s.runs.get_mut(&id).unwrap();
                    if r.status == RunStatus::Blocked
                        && r.reason.as_deref() == Some("execution authority revoked")
                    {
                        r.status = if r.pending.is_some() {
                            RunStatus::Waiting
                        } else {
                            RunStatus::Running
                        };
                        r.reason = None;
                    }
                }
                indexes::mark_run(s, &id);
            }
            receipt(
                s,
                "admin",
                binding,
                now_ms,
                "activation_controls",
                json!({"artifact":artifact,"controls":controls}),
            );
            Ok(())
        })
    }
    pub fn set_execution_authority(
        &self,
        actor: &str,
        scope: &str,
        revoked_capabilities: BTreeSet<String>,
        now_ms: i64,
    ) -> Result<()> {
        self.transaction(|s| {
            s.authority_revocations
                .insert(digest(&(actor, scope))?, revoked_capabilities.clone());
            let ids = s
                .runs
                .values()
                .filter(|r| {
                    r.authority.actor == actor && r.authority.scope == scope && !r.status.terminal()
                })
                .map(|r| r.id.clone())
                .collect::<Vec<_>>();
            for id in ids {
                let r = s.runs.get_mut(&id).unwrap();
                if r.status == RunStatus::Blocked
                    && r.reason.as_deref() == Some("execution authority revoked")
                {
                    r.status = if r.pending.is_some() {
                        RunStatus::Waiting
                    } else {
                        RunStatus::Running
                    };
                    r.reason = None;
                }
                indexes::mark_run(s, &id);
            }
            let intents = s
                .intents
                .values()
                .filter(|i| i.authority.actor == actor && i.authority.scope == scope)
                .map(|i| i.id.clone())
                .collect::<Vec<_>>();
            for id in intents {
                indexes::mark_intent(s, &id);
            }
            receipt(
                s,
                "admin",
                "authority",
                now_ms,
                "authority_changed",
                json!({"actor":actor,"scope":scope,"revoked_capabilities":revoked_capabilities}),
            );
            Ok(())
        })
    }
    pub fn set_retention_policy(&self, policy: RetentionPolicy) -> Result<()> {
        if policy.event_replay_ms == 0
            || policy.terminal_run_ms == 0
            || policy.audit_ms == 0
            || policy.max_tombstones == 0
        {
            return Err(Error::Refused(
                "retention horizons and tombstone quota must be positive".into(),
            ));
        }
        self.transaction(|s| {
            if policy.max_tombstones < s.retention.event_tombstones.len() {
                return Err(Error::Refused(
                    "retention policy cannot evict existing replay tombstones".into(),
                ));
            }
            s.retention.policy = policy;
            Ok(())
        })
    }
    pub fn collect_garbage(&self, now_ms: i64, limit: usize) -> Result<GcReport> {
        self.transaction(|s| retention::gc(s, now_ms, limit))
    }
    pub fn advance_producer_epoch(
        &self,
        producer: &str,
        new_epoch: u64,
        actor: &str,
        reason: &str,
        now_ms: i64,
    ) -> Result<()> {
        if actor.is_empty() || reason.is_empty() {
            return Err(Error::Refused(
                "epoch retirement requires actor and reason".into(),
            ));
        }
        self.transaction(|s|{let old=s.retention.producer_epochs.get(producer).copied().unwrap_or(0);if new_epoch<=old{return Err(Error::Refused("producer epoch must increase".into()))}s.retention.producer_epochs.insert(producer.into(),new_epoch);s.retention.event_tombstones.retain(|_,t|t.producer!=producer||t.epoch>=new_epoch);if s.indexes.root_active.values().all(BTreeSet::is_empty){s.retention.expired_correlations.retain(|_,(p,epoch)|p!=producer||*epoch>=new_epoch);}receipt(s,"admin","replay",now_ms,"producer_epoch_retired",json!({"producer":producer,"old_epoch":old,"new_epoch":new_epoch,"actor":actor,"reason":reason}));Ok(())})
    }
    pub fn request_recovery(
        &self,
        intent_id: &str,
        actor: &str,
        reason: &str,
        evidence_refs: Vec<String>,
        now_ms: i64,
    ) -> Result<String> {
        if actor.is_empty()
            || reason.is_empty()
            || reason.len() > 4096
            || evidence_refs.is_empty()
            || evidence_refs.len() > 64
            || evidence_refs.iter().any(|e| e.is_empty() || e.len() > 1024)
        {
            return Err(Error::Refused(
                "recovery requires bounded actor, reason and evidence references".into(),
            ));
        }
        self.transaction(|s| {
            let i = s
                .intents
                .get(intent_id)
                .ok_or_else(|| Error::NotFound(intent_id.into()))?
                .clone();
            let id = digest(&(intent_id, actor, reason, &evidence_refs))?;
            if !s.recovery_requests.contains_key(&id) {
                s.recovery_requests.insert(
                    id.clone(),
                    RecoveryRequest {
                        id: id.clone(),
                        intent: intent_id.into(),
                        actor: actor.into(),
                        reason: reason.into(),
                        evidence_refs,
                        requested_at_ms: now_ms,
                        consumed: false,
                    },
                );
                receipt(
                    s,
                    &i.run,
                    &i.step,
                    now_ms,
                    "recovery_requested",
                    json!({"request":id,"operation":intent_id,"actor":actor}),
                );
            }
            Ok(id)
        })
    }
    pub fn query(&self, query: &IndexQuery) -> Result<IndexPage> {
        Ok(self.store.query(&self.namespace, query)?)
    }
}
fn execution_allowed(
    s: &State,
    artifact: &str,
    authority: &Authority,
    capabilities: &BTreeSet<String>,
) -> Result<bool> {
    Ok(!s.execution_denied.contains(artifact)
        && s.authority_revocations
            .get(&digest(&(&authority.actor, &authority.scope))?)
            .is_none_or(|revoked| revoked.is_disjoint(capabilities)))
}

/// Determine protection from the exact pinned execution closure before any input
/// is persisted. Catalog provenance that is not selected does not grant or deny
/// execution; known exact/transitive selections are followed even for blocked
/// event destinations whose immutable bytes are only partially installed.
fn ensure_admission_protection(
    s: &State,
    artifact: &str,
    manifest: &BTreeMap<String, String>,
    revision: u64,
) -> Result<()> {
    if s.protection_available {
        return Ok(());
    }
    let mut pending = vec![artifact.to_string()];
    pending.extend(manifest.values().cloned());
    let mut visited = BTreeSet::new();
    while let Some(hash) = pending.pop() {
        if !visited.insert(hash.clone()) {
            continue;
        }
        let Some(artifact) = s.artifacts.get(&hash) else {
            continue;
        };
        if artifact.content.registry.commands.values().any(|command| {
            !command.sensitive_input_fields.is_empty()
                || !command.sensitive_output_fields.is_empty()
        }) {
            return Err(Error::Refused(
                "protected-value storage unavailable for admitted workflow inputs/results".into(),
            ));
        }
        for (name, dependency) in &artifact.content.dependencies {
            let selected = manifest
                .get(&format!("{hash}/{name}"))
                .cloned()
                .unwrap_or_else(|| match &dependency.selection {
                    DependencySelection::Exact => dependency.artifact.clone(),
                    DependencySelection::Catalog { .. } => s
                        .catalog_history
                        .get(&revision)
                        .unwrap_or(&s.bindings)
                        .get(&dependency.binding)
                        .map(|binding| binding.artifact.clone())
                        .unwrap_or_else(|| dependency.artifact.clone()),
                });
            pending.push(selected);
        }
    }
    Ok(())
}
