//! One explicitly started owner and bounded service quanta. Never start another
//! owner because a request timed out. No automatic external mutation retries.
use super::{domain::*, service::*};
use aq_playbook_host::{
    DomainHost, HostMode, Limits, Owner, Request,
    model::{CommandContract, EventEnvelope},
    plugins::{command_plugin, extension_capability},
    runner::{
        Authority, CancellationResult, DispatchResult, EventDisposition, Host, Intent,
        KeyedAdmissionDisposition, KeyedAdmissionReceipt, Storage,
    },
};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;
fn host_error(error: aq_playbook_host::Error) -> Error {
    match error {
        aq_playbook_host::Error::Overloaded => Error::Overloaded,
        _ => Error::Unknown,
    }
}
/// An engine answer to a new event or admission: an unknown outcome or a
/// storage failure is `Unknown` (left pending, delivered again); a refusal
/// is `Invalid` (held).
fn delivery_error(error: aq_playbook_host::Error) -> Error {
    match error {
        aq_playbook_host::Error::Overloaded => Error::Overloaded,
        aq_playbook_host::Error::Unknown | aq_playbook_host::Error::Stopped => Error::Unknown,
        _ => Error::Invalid,
    }
}
/// What a quantum deferred. Capacity defers new work without preventing
/// retained work from reaching its drain and tick; any other failure of one
/// stage is kept, one reason per stage, while the rest of the quantum runs.
#[derive(Default)]
struct Partial {
    overloaded: bool,
    reasons: Vec<String>,
}
impl Partial {
    fn defer<T>(&mut self, what: &str, result: Result<T>) {
        match result {
            Ok(_) => {}
            Err(Error::Overloaded) => self.overloaded = true,
            Err(error) => self.reasons.push(format!("{what}: {error}")),
        }
    }
}
struct MailHost {
    service: Service,
    runtime: RuntimeOwner,
    handle: tokio::runtime::Handle,
    mode: HostMode,
    #[cfg(feature = "test-support")]
    lose_decision_reply: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(feature = "test-support")]
    lose_host_reply: Arc<Mutex<Option<String>>>,
}
impl DomainHost for MailHost {
    fn set_mode(&mut self, mode: HostMode) {
        self.mode = mode
    }
    fn dispatch_unavailable_plugin(&mut self, intent: &Intent) -> DispatchResult {
        self.dispatch_adapter(intent, false)
    }
}
impl Host for MailHost {
    fn authorize(&self, authority: &Authority, contract: &CommandContract) -> bool {
        if self.mode == HostMode::RetryPlugins && command_plugin(&contract.name).is_none() {
            return false;
        }
        let Ok((id, revision)) = grant_actor(authority) else {
            return false;
        };
        self.handle
            .block_on(self.service.authorize_contract(
                &self.runtime,
                id,
                revision,
                contract,
                authority,
                self.mode == HostMode::Dispatch,
            ))
            .is_ok()
    }
    fn dispatch(&mut self, intent: &Intent) -> DispatchResult {
        if self.host_command(intent) {
            return self.dispatch_adapter(intent, true);
        }
        if intent.command.name == "decision.request" {
            let Ok((id, revision)) = grant_actor(&intent.authority) else {
                return DispatchResult::Rejected {
                    reason: "grant binding refused".into(),
                };
            };
            let result = self.handle.block_on(self.service.request_decision(
                &self.runtime,
                id,
                revision,
                intent,
            ));
            #[cfg(feature = "test-support")]
            if result.is_ok()
                && self
                    .lose_decision_reply
                    .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                return DispatchResult::Unknown {
                    reason: "synthetic lost host reply after owner commit".into(),
                };
            }
            return decision_dispatch(result);
        }
        if intent.command.name == "operation.get" {
            let Ok((id, revision)) = grant_actor(&intent.authority) else {
                return DispatchResult::Rejected {
                    reason: "grant binding refused".into(),
                };
            };
            return decision_dispatch(self.handle.block_on(self.service.operation_get(
                &self.runtime,
                id,
                revision,
                intent,
            )));
        }
        let Ok((id, revision)) = grant_actor(&intent.authority) else {
            return DispatchResult::Rejected {
                reason: "grant binding refused".into(),
            };
        };
        match self.handle.block_on(
            self.service
                .send_message(&self.runtime, id, revision, intent),
        ) {
            Ok(MessageResult::Queued(receipt)) => DispatchResult::Completed {
                outcome: "queued".into(),
                output: receipt,
            },
            Ok(MessageResult::Cancelled) => DispatchResult::Rejected {
                reason: "cancelled before mailbox admission".into(),
            },
            Err(Error::Unknown) => DispatchResult::Unknown {
                reason: "mailbox admission outcome unknown; retained lookup required".into(),
            },
            Err(error) => DispatchResult::Rejected {
                reason: error.to_string(),
            },
        }
    }
    fn reconcile(&mut self, intent: &Intent) -> DispatchResult {
        if self.mode != HostMode::Reconcile {
            return DispatchResult::Unknown {
                reason: "explicit recovery mode required".into(),
            };
        }
        if intent.command.name == "decision.request" {
            let Ok((id, revision)) = grant_actor(&intent.authority) else {
                return DispatchResult::Unknown {
                    reason: "grant binding refused".into(),
                };
            };
            return match self
                .handle
                .block_on(self.service.reconcile_decision(id, revision, intent))
            {
                Ok(Some(result)) => decision_dispatch(Ok(result)),
                _ => DispatchResult::Unknown {
                    reason: "no exact retained decision owner result".into(),
                },
            };
        }
        if intent.command.name == "operation.get" {
            return self.dispatch(intent);
        }
        if self.host_command(intent) {
            let Ok((id, revision)) = grant_actor(&intent.authority) else {
                return DispatchResult::Unknown {
                    reason: "grant binding refused".into(),
                };
            };
            return match self
                .handle
                .block_on(self.service.host_retained(id, revision, intent))
            {
                Ok(Some(result)) => host_result(result, intent),
                _ => DispatchResult::Unknown {
                    reason: "no retained host result; no new effect admitted".into(),
                },
            };
        }
        let Ok((id, revision)) = grant_actor(&intent.authority) else {
            return DispatchResult::Unknown {
                reason: "grant binding refused".into(),
            };
        };
        match self
            .handle
            .block_on(self.service.reconcile(id, revision, intent))
        {
            Ok(Some(MessageResult::Queued(receipt))) => DispatchResult::Completed {
                outcome: "queued".into(),
                output: receipt,
            },
            Ok(Some(MessageResult::Cancelled)) => DispatchResult::Rejected {
                reason: "cancelled before mailbox admission".into(),
            },
            _ => DispatchResult::Unknown {
                reason: "no verified retained owner result; no new write admitted".into(),
            },
        }
    }
    fn cancel(&mut self, intent: &Intent) -> CancellationResult {
        if self.mode != HostMode::Cancel {
            return CancellationResult::Unknown;
        }
        let Ok((id, revision)) = grant_actor(&intent.authority) else {
            return CancellationResult::Refused;
        };
        if self.service.profile.is_host2() && intent.command.name == "check.request" {
            // Catalog 2.0.0 check.request is cancellable: confirmed only by an
            // executor-reported cancelled stop, never by the request alone.
            return self
                .handle
                .block_on(
                    self.service
                        .check_cancel(&self.runtime, id, revision, intent),
                )
                .unwrap_or(CancellationResult::Unknown);
        }
        if self.host_command(intent) {
            // Other host effects are cancelled through operation.cancel, never
            // by the runner withdrawing an admitted effect.
            return CancellationResult::Refused;
        }
        if intent.command.name == "decision.request" {
            return self
                .handle
                .block_on(
                    self.service
                        .cancel_decision(&self.runtime, id, revision, intent),
                )
                .unwrap_or(CancellationResult::Unknown);
        }
        self.handle
            .block_on(self.service.cancel(&self.runtime, id, revision, intent))
            .unwrap_or(CancellationResult::Unknown)
    }
}
impl MailHost {
    fn dispatch_adapter(&mut self, intent: &Intent, plugin_available: bool) -> DispatchResult {
        let Ok((id, revision)) = grant_actor(&intent.authority) else {
            return DispatchResult::Rejected {
                reason: "grant binding refused".into(),
            };
        };
        let result = self.handle.block_on(async {
            if plugin_available {
                self.service
                    .host_dispatch(&self.runtime, id, revision, intent)
                    .await
            } else {
                self.service
                    .plugin_dispatch(&self.runtime, id, revision, intent, false)
                    .await
            }
        });
        // D11 GATE-05 fixture: the AQ effect and its final row committed,
        // then the reply is lost before the runner records it.
        #[cfg(feature = "test-support")]
        if result.is_ok()
            && let Ok(mut armed) = self.lose_host_reply.lock()
            && armed.as_deref() == Some(intent.command.name.as_str())
        {
            *armed = None;
            return DispatchResult::Unknown {
                reason: "synthetic lost host reply after owner commit".into(),
            };
        }
        match result {
            Ok(result) => host_result(result, intent),
            Err(Error::Unknown) => DispatchResult::Unknown {
                reason: "host effect outcome unknown; retained lookup required".into(),
            },
            Err(error) => DispatchResult::Rejected {
                reason: error.to_string(),
            },
        }
    }
    /// Legacy review and mailbox profiles own their base adapters separately.
    fn host_command(&self, intent: &Intent) -> bool {
        let name = &intent.command.name;
        command_plugin(name).is_some()
            || (self.service.profile.is_host()
                && self.service.registry().commands.contains_key(name))
    }
}
/// An asynchronous host command's `accepted` outcome is its durable handle.
fn host_result(result: super::host_adapters::HostResult, intent: &Intent) -> DispatchResult {
    if command_plugin(&intent.command.name).is_none()
        && result.outcome == "accepted"
        && let Some(handle) = result.output["handle"].as_str()
    {
        return DispatchResult::Accepted {
            handle: handle.to_string(),
        };
    }
    DispatchResult::Completed {
        outcome: result.outcome,
        output: result.output,
    }
}
fn decision_dispatch(result: Result<DecisionResult>) -> DispatchResult {
    match result {
        Ok(DecisionResult::Accepted { handle }) => DispatchResult::Accepted { handle },
        Ok(DecisionResult::Completed { outcome, output }) => {
            DispatchResult::Completed { outcome, output }
        }
        Ok(DecisionResult::Cancelled) => DispatchResult::Rejected {
            reason: "cancelled before question admission".into(),
        },
        Err(Error::Unknown) => DispatchResult::Unknown {
            reason: "decision owner result unknown; retained lookup required".into(),
        },
        Err(e) => DispatchResult::Rejected {
            reason: e.to_string(),
        },
    }
}
pub struct Driver {
    pub(super) owner: Owner,
    pub service: Service,
    pub(super) runtime: RuntimeOwner,
    pub(super) loaded_definitions:
        std::sync::OnceLock<super::inspection::definitions::LoadedDefinitions>,
    gate: tokio::sync::Mutex<i64>,
    /// Order binding mutations with their bridge projections.
    binding_gate: tokio::sync::Mutex<()>,
    /// Owner-call deadlines, from the engine timeouts: one commit, and a tick.
    pub(super) deadline: Duration,
    tick_deadline: Duration,
    /// Rolling deletion: one bridge table per quantum, in turn.
    retention: aq_playbook_host::retention::Retention,
    pruned: std::sync::atomic::AtomicUsize,
    /// Why the last quantum failed or skipped a source; `None` when clean.
    reason: Mutex<Option<String>>,
    automatic: std::sync::atomic::AtomicU8,
    stopped_ack: std::sync::atomic::AtomicBool,
    /// Set once an installed artifact consumes the exact graph event contract.
    /// The engine refuses events no installed artifact declares, so collection
    /// before a consumer exists would wedge the outbox page.
    graph_consumer: std::sync::atomic::AtomicBool,
    settlement_consumer: std::sync::atomic::AtomicBool,
    schedule_consumer: std::sync::atomic::AtomicBool,
    #[cfg(feature = "test-support")]
    lose_decision_reply: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(feature = "test-support")]
    lose_host_reply: Arc<Mutex<Option<String>>>,
}
impl Driver {
    pub async fn start<S, F>(service: Service, limits: Limits, factory: F) -> Result<Self>
    where
        S: Storage + 'static,
        F: FnOnce() -> std::result::Result<S, aq_playbook_host::Error> + Send + 'static,
    {
        let opened = Self::open(service, limits, factory).await;
        opened.map_err(|(error, _)| error)
    }
    /// [`Self::start`]; a failure comes with its cause in words (no secrets).
    pub(super) async fn open<S, F>(
        service: Service,
        limits: Limits,
        factory: F,
    ) -> std::result::Result<Self, (Error, String)>
    where
        S: Storage + 'static,
        F: FnOnce() -> std::result::Result<S, aq_playbook_host::Error> + Send + 'static,
    {
        let lost = |error: Error| (error, "the acquired runtime owner was lost".to_string());
        #[cfg(feature = "test-support")]
        let lose_decision_reply = Arc::new(std::sync::atomic::AtomicBool::new(false));
        #[cfg(feature = "test-support")]
        let host_reply_fault = lose_decision_reply.clone();
        #[cfg(feature = "test-support")]
        let lose_host_reply: Arc<Mutex<Option<String>>> = Arc::default();
        #[cfg(feature = "test-support")]
        let host_command_fault = lose_host_reply.clone();
        let expected = (service.runtime_revision().await)
            .map_err(|e| (e, "reading the runtime revision".into()))?;
        let deadline = limits.timeouts.call_deadline(1);
        let tick_deadline = (limits.timeouts).call_deadline(limits.transitions as u32);
        let retention = limits.retention;
        let handle = tokio::runtime::Handle::current();
        let host_service = service.clone();
        let namespace = service.namespace.engine_namespace();
        let expected_namespace = namespace.clone();
        let proof = Arc::new(Mutex::new(None));
        let captured = proof.clone();
        let guarded_factory = move || {
            let store = factory()?;
            if !store.protection_available() {
                return Err(aq_playbook_host::Error::Invalid);
            }
            Ok(store)
        };
        let owner = Owner::start_registered(
            namespace,
            limits,
            service.registry().clone(),
            guarded_factory,
            move |acquired| {
                if acquired.namespace != expected_namespace {
                    return Err(aq_playbook_host::Error::Invalid);
                }
                let incarnation: Uuid = acquired
                    .incarnation
                    .parse()
                    .map_err(|_| aq_playbook_host::Error::Invalid)?;
                let runtime = handle
                    .block_on(host_service.adopt_runtime(incarnation, expected))
                    .map_err(|_| aq_playbook_host::Error::Unknown)?;
                *captured
                    .lock()
                    .map_err(|_| aq_playbook_host::Error::Unknown)? = Some(runtime.clone());
                Ok(MailHost {
                    service: host_service,
                    runtime,
                    handle,
                    mode: HostMode::Dispatch,
                    #[cfg(feature = "test-support")]
                    lose_decision_reply: host_reply_fault,
                    #[cfg(feature = "test-support")]
                    lose_host_reply: host_command_fault,
                })
            },
        )
        .await
        .map_err(|e| (e.to_string(), host_error(e)))
        .map_err(|(why, error)| (error, why))?;
        let taken = proof.lock().map_err(|_| Error::Unknown);
        let runtime = taken.and_then(|mut proof| proof.take().ok_or(Error::Unknown));
        let runtime = runtime.map_err(lost)?;
        Ok(Self {
            owner,
            service,
            runtime,
            loaded_definitions: std::sync::OnceLock::new(),
            gate: tokio::sync::Mutex::new(0),
            binding_gate: tokio::sync::Mutex::new(()),
            deadline,
            tick_deadline,
            retention,
            pruned: Default::default(),
            reason: Mutex::default(),
            automatic: std::sync::atomic::AtomicU8::new(0),
            stopped_ack: std::sync::atomic::AtomicBool::new(false),
            graph_consumer: std::sync::atomic::AtomicBool::new(false),
            settlement_consumer: std::sync::atomic::AtomicBool::new(false),
            schedule_consumer: std::sync::atomic::AtomicBool::new(false),
            #[cfg(feature = "test-support")]
            lose_decision_reply,
            #[cfg(feature = "test-support")]
            lose_host_reply,
        })
    }
    pub(crate) fn set_automatic_state(&self, state: u8) {
        self.automatic
            .store(state, std::sync::atomic::Ordering::SeqCst);
    }
    /// The automatic drive's last quantum failed (5) or succeeded again (1).
    pub(crate) fn drive_failed(&self, failed: bool) {
        let (from, to) = if failed { (1, 5) } else { (5, 1) };
        let ordering = std::sync::atomic::Ordering::SeqCst;
        let _ = self
            .automatic
            .compare_exchange(from, to, ordering, ordering);
    }
    pub(crate) fn automatic_state(&self) -> &'static str {
        match self.automatic.load(std::sync::atomic::Ordering::SeqCst) {
            1 => "active",
            2 => "held",
            3 => "stopping",
            4 => "stopped",
            5 => "retrying",
            _ => "manual",
        }
    }
    pub async fn install(&self, artifact: aq_playbook_host::model::Artifact) -> Result<()> {
        let event = super::review_domain::graph_operation_event();
        let consumer = artifact.content.registry.events.get(&event.name) == Some(&event);
        let completed = super::review_domain::task_completed_event();
        let settlement_consumer =
            artifact.content.registry.events.get(&completed.name) == Some(&completed);
        let schedule_consumer = self.service.profile
            == super::review_domain::CatalogProfile::Monitoring6
            && artifact.content.registry.events.get("aq.signal.reported")
                == self.service.registry().events.get("aq.signal.reported");
        let registered = artifact.clone();
        self.owner
            .call(
                Request::Install {
                    artifact: Box::new(artifact),
                },
                self.deadline,
            )
            .await
            .map_err(host_error)?;
        // G11 X30/X32 read model only: a registry failure leaves the run's
        // definition view null and never refuses an owner-committed install.
        if self.service.register_definition(&registered).await.is_err() {
            tracing::warn!(
                "playbook definition registry write failed; definition view unavailable"
            );
        }
        if schedule_consumer {
            self.schedule_consumer
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        if consumer {
            self.graph_consumer
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        if settlement_consumer {
            self.settlement_consumer
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
        Ok(())
    }
    pub async fn activate(
        &self,
        binding: String,
        artifact: String,
        grant: Uuid,
        revision: i64,
    ) -> Result<serde_json::Value> {
        let _guard = self.binding_gate.lock().await;
        let active_authority = self
            .service
            .active_authority(&self.runtime, grant, revision)
            .await?;
        self.owner
            .call(
                Request::ActivateAt {
                    binding: binding.clone(),
                    artifact,
                    authority: active_authority,
                    now_ms: self.service_clock(),
                },
                self.deadline,
            )
            .await
            .map_err(|error| match error {
                aq_playbook_host::Error::Invalid => Error::Invalid,
                other => host_error(other),
            })?;
        let current = Request::Binding {
            binding: binding.clone(),
        };
        let active = (self.owner.call(current, self.deadline).await).map_err(|_| Error::Unknown)?;
        // The server's record of the engine's binding (every activation
        // path ends here), from the engine's own answer.
        if let Some(artifact) = active["artifact"].as_str() {
            self.service.binding_activated(&binding, artifact).await?;
        }
        Ok(active)
    }
    /// Recovery=false permits one exact retained first admission only under current
    /// grant. Recovery=true is lookup-only and cannot create a missing engine run.
    pub async fn admission(
        &self,
        actor: Uuid,
        id: Uuid,
        recovery: bool,
    ) -> Result<AdmissionStatus> {
        let retained = self.service.admission(actor, id).await?;
        if retained.status.receipt.is_some() {
            return Ok(retained.status);
        }
        let value = self
            .owner
            .call(
                Request::Admission {
                    key: retained.status.engine_key.clone(),
                },
                self.deadline,
            )
            .await
            .map_err(host_error)?;
        let known: Option<KeyedAdmissionReceipt> = serde_json::from_value(value)?;
        let receipt = if let Some(receipt) = known {
            receipt
        } else {
            if recovery {
                return Err(Error::Unknown);
            }
            self.service
                .authorize(&self.runtime, retained.grant_id, retained.grant_revision)
                .await?;
            let value = match self
                .owner
                .call(
                    Request::AdmitOnce {
                        request: Box::new(retained.request),
                        requested_at_ms: None,
                        now_ms: (self.service_clock()),
                    },
                    self.deadline,
                )
                .await
            {
                Ok(value) => value,
                Err(aq_playbook_host::Error::Stale(_)) => {
                    let _ = self.service.hold(&self.runtime, false, id, "stale").await;
                    return Err(Error::Unknown);
                }
                Err(error) => return Err(delivery_error(error)),
            };
            let result: KeyedAdmissionDisposition = serde_json::from_value(value)?;
            result.receipt
        };
        self.service
            .acknowledge_admission(&self.runtime, id, receipt)
            .await
    }
    fn service_clock(&self) -> i64 {
        self.service.current_time()
    }
    pub async fn recover(&self) -> Result<serde_json::Value> {
        self.owner
            .call(
                Request::Recover {
                    now_ms: self.service_clock(),
                },
                self.deadline,
            )
            .await
            .map_err(|_| Error::Unknown)
    }
    /// Trusted host registration; runtime ownership never crosses worker APIs.
    pub async fn put_schedule(
        &self,
        schedule: super::triggers::DailySchedule,
        expected_revision: i64,
    ) -> Result<i64> {
        let _guard = self.gate.try_lock().map_err(|_| Error::Overloaded)?;
        self.service
            .put_schedule(&self.runtime, schedule, expected_revision)
            .await
    }
    pub async fn start_health_scan(
        &self,
        observation: &str,
        partition: &str,
        ttl_ms: i64,
    ) -> Result<std::collections::BTreeMap<String, i64>> {
        self.service
            .start_health_scan(&self.runtime, observation, partition, ttl_ms)
            .await
    }
    pub async fn complete_health_scan(
        &self,
        observation: &str,
        evaluation: &str,
        complete: bool,
    ) -> Result<()> {
        self.service
            .complete_health_scan(&self.runtime, observation, evaluation, complete)
            .await
    }
    /// At most two admissions, outbox events and interval occurrences each,
    /// plus one eight-transition tick. Overlap refuses without a waiter queue.
    pub async fn quantum(&self) -> Result<serde_json::Value> {
        let mut partial = Partial::default();
        let result = self.quantum_with(&mut partial).await;
        if let Err(error) = &result
            && *error != Error::Overloaded
            && partial.reasons.is_empty()
        {
            partial.reasons.push(format!("quantum failed: {error}"));
        }
        let reasons = partial.reasons.join("; ");
        *self.reason.lock().map_err(|_| Error::Unknown)? = (!reasons.is_empty()).then_some(reasons);
        result
    }
    /// The drive's reason for its last failed or partial quantum.
    pub(crate) fn drive_reason(&self) -> Option<String> {
        self.reason.lock().ok().and_then(|reason| reason.clone())
    }
    async fn quantum_with(&self, partial: &mut Partial) -> Result<serde_json::Value> {
        let mut cursor = self.gate.try_lock().map_err(|_| Error::Overloaded)?;
        let pending = self.service.pending_admissions(&self.runtime, 2).await?;
        for id in &pending {
            let result = self
                .admission(self.service.namespace.owner_user_id, *id, false)
                .await;
            if matches!(result, Err(Error::Overloaded)) {
                partial.overloaded = true;
                continue;
            }
            // A refused admission is held visibly, so a poison or expired first
            // page cannot starve later ones. An unknown outcome stays pending
            // with the reason `uncertain`, behind the other pending ones: the
            // engine answers duplicate when it is delivered again.
            if let Err(error) = result {
                let reason = (error == Error::Unknown).then_some("uncertain");
                let hold =
                    (self.service).hold(&self.runtime, false, *id, reason.unwrap_or("refused"));
                hold.await?;
            }
        }
        partial.defer(
            "expiring decisions",
            self.service.expire_decisions(&self.runtime, 2).await,
        );
        partial.defer(
            "collecting decision events",
            self.service.collect_decision_events(&self.runtime, 2).await,
        );
        if self
            .graph_consumer
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            partial.defer(
                "collecting graph events",
                self.service.collect_graph_events(&self.runtime, 2).await,
            );
        }
        if self
            .settlement_consumer
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            partial.defer(
                "collecting settlement events",
                self.service
                    .collect_settlement_events(&self.runtime, 2)
                    .await,
            );
        }
        partial.defer(
            "asking for routes",
            self.service.collect_route_needed(&self.runtime, 8).await,
        );
        #[cfg(feature = "delivery_playbook")]
        partial.defer(
            "announcing results",
            self.service.collect_result_ready(&self.runtime, 8).await,
        );
        #[cfg(feature = "delivery_playbook")]
        partial.defer(
            "running delivery operations",
            self.service
                .collect_delivery_operations(&self.runtime, 4)
                .await,
        );
        partial.defer(
            "expiring judgments",
            self.service.expire_assessments(&self.runtime, 2).await,
        );
        // Catalog profile 7 work.ensure completion (aq.operation.updated).
        partial.defer(
            "collecting work completions",
            self.service
                .collect_work_completions(&self.runtime, 2)
                .await,
        );
        if self.service.profile.is_host() {
            partial.defer(
                "collecting interval events",
                self.owner
                    .call(
                        Request::CollectIntervalEvents {
                            now_ms: self.service_clock(),
                        },
                        self.deadline,
                    )
                    .await
                    .map_err(host_error),
            );
        }
        if self.service.profile == super::review_domain::CatalogProfile::Monitoring6
            && self
                .schedule_consumer
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            partial.defer(
                "collecting schedule events",
                self.service.collect_schedule_events(&self.runtime, 2).await,
            );
        }
        // A row that cannot be decrypted ends the page before it: event
        // delivery waits there (until the keyring is fixed or the owner holds
        // the row) while effects are still cancelled and runs still tick.
        let (page, undecryptable) = self.service.outbox_page(&self.runtime, *cursor, 2).await?;
        if page.is_empty() && undecryptable.is_none() {
            *cursor = 0;
        }
        for (sequence, id, event, accepted) in &page {
            if !accepted
                && (self.service.profile.is_host2()
                    || extension_capability(&event.producer).is_some())
                && !self
                    .owner
                    .declares_event(&event.contract.name, &event.contract.fingerprint)
            {
                // No installed artifact consumes this contract: retire it
                // with zero destinations instead of wedging the page.
                let key = digest(&(&event.producer, 0_u64, &event.event_id))?;
                let receipt = EventDisposition {
                    key,
                    duplicate: false,
                    destinations: 0,
                };
                self.service
                    .acknowledge_event(&self.runtime, *id, receipt, true)
                    .await?;
                *cursor = *sequence;
                continue;
            }
            match self.deliver(*id, event, *accepted).await {
                Err(Error::Overloaded) => {
                    partial.overloaded = true;
                    break;
                }
                // An unknown outcome leaves the row as it is (delivered again);
                // a refusal is that row's outcome: hold it and go on.
                Err(Error::Unknown) | Ok(()) => {}
                Err(_) => {
                    let held = self.service.hold(&self.runtime, true, *id, "refused");
                    held.await?
                }
            }
            *cursor = *sequence;
        }
        let now_ms = self.service_clock();
        let cancel = (self.owner).call(Request::CancelEffects { now_ms }, self.deadline);
        let cancelled = cancel.await.map_err(host_error);
        partial.defer("cancelling effects", cancelled.clone());
        cancelled?;
        let now_ms = self.service_clock();
        let ticked = (self.owner).call(Request::Tick { now_ms }, self.tick_deadline);
        let tick = ticked.await.map_err(host_error);
        partial.defer("ticking runs", tick.clone());
        let tick = tick?;
        if self.owner.has_plugin_artifacts() {
            partial.defer(
                "sending plugin notifications",
                self.service.flush_plugin_notices(&self.runtime).await,
            );
        }
        if self.service.profile.has_findings() {
            partial.defer(
                "sending finding notifications",
                self.service
                    .flush_finding_notifications(&self.runtime, 2)
                    .await,
            );
        }
        let table = self
            .pruned
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let pruned = self.service.prune(&self.runtime, &self.retention, table);
        partial.defer("deleting finished rows", pruned.await);
        if partial.overloaded {
            return Err(Error::Overloaded);
        }
        if let Some(reason) = undecryptable {
            partial.reasons.push(reason);
            return Err(Error::Unknown);
        }
        Ok(
            serde_json::json!({"admissions_checked":pending.len(),"events_checked":page.len(),"tick":tick}),
        )
    }
    /// One outbox row. A row the engine has accepted is never sent as a new
    /// event again; it only continues its fan-out. An accepted event the
    /// engine no longer remembers has nothing queued that holds it (what is
    /// left is blocked by its input), so its row is complete.
    async fn deliver(&self, id: Uuid, event: &EventEnvelope, accepted: bool) -> Result<()> {
        if !accepted {
            let request = Request::Event {
                authenticated_producer: event.producer.clone(),
                epoch: 0,
                event: Box::new(event.clone()),
                requested_at_ms: None,
                now_ms: self.service_clock(),
            };
            let receipt = match self.owner.call(request, self.deadline).await {
                Err(aq_playbook_host::Error::Stale(_)) => {
                    return self.service.hold(&self.runtime, true, id, "stale").await;
                }
                result => serde_json::from_value(result.map_err(delivery_error)?)?,
            };
            self.service
                .acknowledge_event(&self.runtime, id, receipt, false)
                .await?;
        }
        let key = digest(&(&event.producer, 0_u64, &event.event_id))?;
        let fanout = Request::Fanout {
            event_key: key.clone(),
            limit: 64,
            now_ms: self.service_clock(),
        };
        let status = match self.owner.call(fanout, self.deadline).await {
            Ok(_) => {
                self.owner
                    .call(
                        Request::EventStatus {
                            event_key: key.clone(),
                        },
                        self.deadline,
                    )
                    .await
            }
            Err(error) => Err(error),
        };
        let complete = match status {
            Ok(status) => status["pending"].as_u64().ok_or(Error::Unknown)? == 0,
            // Forgotten: `Fanout` does not find the event itself (another
            // missing record fails the fan-out), or `EventStatus` does not.
            Err(aq_playbook_host::Error::NotFound(what)) if what == key => true,
            Err(aq_playbook_host::Error::Invalid) => true,
            Err(error) => return Err(host_error(error)),
        };
        if complete {
            self.service.complete_event(&self.runtime, id).await?;
        }
        Ok(())
    }
    pub async fn inspect_binding(&self, binding: String) -> Result<serde_json::Value> {
        if binding.is_empty() || binding.len() > 128 || binding.contains('\0') {
            return Err(Error::Invalid);
        }
        self.owner
            .call(Request::Binding { binding }, self.deadline)
            .await
            .map_err(|_| Error::Unknown)
    }
    pub async fn inspect_run(&self, run: String) -> Result<serde_json::Value> {
        if run.is_empty() || run.len() > 256 {
            return Err(Error::Invalid);
        }
        self.owner
            .call(
                Request::Inspect {
                    query: aq_playbook_host::runner::InspectionQuery {
                        kind: aq_playbook_host::runner::InspectionKind::Runs,
                        run: Some(run),
                        after: None,
                        limit: 1,
                    },
                },
                self.deadline,
            )
            .await
            .map_err(|_| Error::Unknown)
    }
    pub async fn cancel_run(&self, run: String) -> Result<()> {
        if run.is_empty() || run.len() > 512 {
            return Err(Error::Invalid);
        }
        let cancel = Request::Cancel {
            run: run.clone(),
            now_ms: self.service_clock(),
        };
        match self.owner.call(cancel, self.deadline).await {
            Ok(_) => Ok(()),
            // A finished run has left the engine State (it is history), so the
            // engine no longer finds it. Cancelling it stays the no-op it is
            // for a terminal run.
            Err(aq_playbook_host::Error::Engine(_)) if self.finished(run).await? => Ok(()),
            Err(error) => Err(host_error(error)),
        }
    }
    async fn finished(&self, run: String) -> Result<bool> {
        let page = self.inspect_run(run.clone()).await?;
        let Some(status) = page["items"][0]["status"].as_str() else {
            // Rolled out of history too: finished if AQ admitted it.
            return self.service.admitted_run(&run).await;
        };
        Ok(matches!(status, "completed" | "failed" | "cancelled"))
    }
    /// Startup reconciliation preserves the current activation and its
    /// unfinished interval cursors when the configured binding is unchanged.
    pub async fn ensure_binding(
        &self,
        binding: String,
        artifact: String,
        grant: Uuid,
        scope: &super::domain::GrantScope,
        schedules: &[aq_playbook_host::model::schedules::BindingSchedule],
    ) -> Result<()> {
        let _guard = self.binding_gate.lock().await;
        let authority = self.service.grant_authority(grant, 1, scope)?;
        let may_activate = match self.service.active_authority(&self.runtime, grant, 1).await {
            Ok(current) if current == authority => true,
            Ok(_) => return Err(Error::Conflict),
            Err(Error::Forbidden) => false,
            Err(error) => return Err(error),
        };
        let interval_event = if schedules.is_empty() {
            None
        } else {
            Some(self.service.interval_event()?.ok_or(Error::Invalid)?)
        };
        let active = self
            .owner
            .call(
                Request::ConfigureBinding {
                    binding: binding.clone(),
                    artifact: artifact.clone(),
                    authority,
                    may_activate,
                    schedules: schedules.to_vec(),
                    interval_event,
                    now_ms: self.service_clock(),
                },
                self.deadline,
            )
            .await
            .map_err(host_error)?;
        match active.as_bool() {
            Some(true) => self.service.binding_activated(&binding, &artifact).await,
            Some(false) => self.service.binding_deactivated(&binding).await,
            None => Err(Error::Unknown),
        }
    }

    pub async fn deactivate_binding(&self, binding: String, revision: u64) -> Result<()> {
        if binding.is_empty() || binding.len() > 128 || binding.contains('\0') || revision == 0 {
            return Err(Error::Invalid);
        }
        let _guard = self.binding_gate.lock().await;
        let result = self
            .owner
            .call(
                Request::DeactivateBinding {
                    binding: binding.clone(),
                    revision,
                    now_ms: self.service_clock(),
                },
                self.deadline,
            )
            .await
            .map_err(host_error)?;
        match result.as_bool() {
            Some(true) => self.service.binding_deactivated(&binding).await,
            Some(false) => Err(Error::Conflict),
            None => Err(Error::Unknown),
        }
    }

    /// D9: the binding's active artifact digest; `None` if never activated.
    pub(crate) async fn binding_artifact(&self, binding: String) -> Result<Option<String>> {
        match self
            .owner
            .call(Request::Binding { binding }, self.deadline)
            .await
        {
            Ok(value) => {
                let a: aq_playbook_host::runner::Activation = serde_json::from_value(value)?;
                Ok(Some(a.artifact))
            }
            Err(aq_playbook_host::Error::Invalid) => Ok(None),
            Err(_) => Err(Error::Unknown),
        }
    }
    /// The binding's full current activation; `None` if never activated.
    pub(crate) async fn binding_activation(
        &self,
        binding: String,
    ) -> Result<Option<aq_playbook_host::runner::Activation>> {
        if binding.is_empty() || binding.len() > 128 || binding.contains('\0') {
            return Err(Error::Invalid);
        }
        match self
            .owner
            .call(Request::Binding { binding }, self.deadline)
            .await
        {
            Ok(value) => Ok(Some(serde_json::from_value(value)?)),
            Err(aq_playbook_host::Error::Invalid) => Ok(None),
            Err(_) => Err(Error::Unknown),
        }
    }
    /// Internal shutdown only: closing retained ownership needs no fresh user
    /// business grant. Stop is attempted even if central fencing is uncertain.
    pub(crate) async fn shutdown(&self) -> Result<()> {
        let fence = self.service.retire_for_shutdown(&self.runtime).await;
        let stop = self.owner.call(Request::Stop, self.deadline).await;
        if !aq_playbook_host::drive::stop_acknowledged(&self.stopped_ack, stop) {
            return Err(Error::Unknown);
        }
        fence
    }
    pub async fn stop(&self) -> Result<()> {
        self.service.retire_runtime(&self.runtime).await?;
        match self.owner.call(Request::Stop, self.deadline).await {
            Ok(_) => {
                self.stopped_ack
                    .store(true, std::sync::atomic::Ordering::SeqCst);
            }
            Err(aq_playbook_host::Error::Stopped) => {}
            Err(_) => return Err(Error::Unknown),
        }
        Ok(())
    }
}

#[cfg(feature = "test-support")]
impl Driver {
    /// D11 GATE-05: the next `command` host-adapter dispatch commits its AQ
    /// effect and final row, then its reply is lost before the runner
    /// records it. Never compiled into ordinary release builds.
    pub fn test_lose_next_host_reply(&self, command: &str) {
        if let Ok(mut armed) = self.lose_host_reply.lock() {
            *armed = Some(command.to_string());
        }
    }
    /// Component-only lost callback response after the actual question SQL
    /// transaction. Never compiled into ordinary release builds or HTTP routes.
    pub fn test_lose_next_decision_reply(&self) {
        self.lose_decision_reply
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    pub async fn test_collect_decision_events(&self) -> Result<usize> {
        self.service.collect_decision_events(&self.runtime, 2).await
    }
    pub async fn test_collect_graph_events(&self) -> Result<usize> {
        self.service.collect_graph_events(&self.runtime, 2).await
    }
    pub async fn test_collect_settlement_events(&self) -> Result<usize> {
        self.service
            .collect_settlement_events(&self.runtime, 2)
            .await
    }
    /// Exact retained-event cut fixture: engine commits, AQ ACK is omitted.
    /// Not mounted as an HTTP route or present in ordinary release builds.
    pub async fn test_commit_event_without_aq_ack(&self) -> Result<EventDisposition> {
        let page = self.service.outbox(&self.runtime, 0, 1).await?;
        let (_, _, event) = page.into_iter().next().ok_or(Error::Invalid)?;
        let value = self
            .owner
            .call(
                Request::Event {
                    authenticated_producer: event.producer.clone(),
                    epoch: 0,
                    event: Box::new(event),
                    requested_at_ms: None,
                    now_ms: self.service_clock(),
                },
                self.deadline,
            )
            .await
            .map_err(host_error)?;
        serde_json::from_value(value).map_err(|_| Error::Unknown)
    }
}
