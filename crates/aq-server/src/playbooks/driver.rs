//! One explicitly started owner and bounded service quanta. Never start another
//! owner because a request timed out. No automatic external mutation retries.
use super::{domain::*, service::*};
use aq_playbook_host::{
    DomainHost, HostMode, Limits, Owner, Request,
    model::CommandContract,
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
const CALL: Duration = Duration::from_secs(5);
fn host_error(error: aq_playbook_host::Error) -> Error {
    match error {
        aq_playbook_host::Error::Overloaded => Error::Overloaded,
        _ => Error::Unknown,
    }
}
// Capacity refuses new work without preventing already retained work from
// reaching its bounded drain and tick. Other failures keep their semantics.
fn defer_capacity<T>(result: Result<T>, overloaded: &mut bool) -> Result<()> {
    match result {
        Ok(_) => Ok(()),
        Err(Error::Overloaded) => {
            *overloaded = true;
            Ok(())
        }
        Err(error) => Err(error),
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
}
impl Host for MailHost {
    fn authorize(&self, authority: &Authority, contract: &CommandContract) -> bool {
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
            let Ok((id, revision)) = grant_actor(&intent.authority) else {
                return DispatchResult::Rejected {
                    reason: "grant binding refused".into(),
                };
            };
            let result = self.handle.block_on(self.service.host_dispatch(
                &self.runtime,
                id,
                revision,
                intent,
            ));
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
            return match result {
                Ok(result) => host_result(result),
                Err(Error::Unknown) => DispatchResult::Unknown {
                    reason: "host effect outcome unknown; retained lookup required".into(),
                },
                Err(error) => DispatchResult::Rejected {
                    reason: error.to_string(),
                },
            };
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
                Ok(Some(result)) => host_result(result),
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
    /// Host-adapter commands of the pinned profile (5: D4 catalog 1.0.0;
    /// 7: AQ host catalog 2.0.0, which also owns operation.get).
    fn host_command(&self, intent: &Intent) -> bool {
        let name = intent.command.name.as_str();
        super::host_adapters::HOST5_COMMANDS.contains(&name)
            || (self.service.profile.is_host2()
                && super::host_adapters::HOST7_COMMANDS.contains(&name))
    }
}
/// An asynchronous host command's `accepted` outcome is its durable handle.
fn host_result(result: super::host_adapters::HostResult) -> DispatchResult {
    if result.outcome == "accepted"
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
    automatic: std::sync::atomic::AtomicU8,
    stopped_ack: std::sync::atomic::AtomicBool,
    /// Set once an installed artifact consumes the exact graph event contract.
    /// The engine refuses events no installed artifact declares, so collection
    /// before a consumer exists would wedge the outbox page.
    graph_consumer: std::sync::atomic::AtomicBool,
    settlement_consumer: std::sync::atomic::AtomicBool,
    schedule_consumer: std::sync::atomic::AtomicBool,
    /// Host7 (D11): event contracts (name, fingerprint) some installed
    /// artifact declares. The engine refuses any other contract, so a Host7
    /// bridge event nothing consumes (aq.work.changed after an accepted
    /// work.ensure, aq.integration.changed, ...) would wedge the outbox page.
    declared_events: Mutex<std::collections::BTreeSet<(String, String)>>,
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
        #[cfg(feature = "test-support")]
        let lose_decision_reply = Arc::new(std::sync::atomic::AtomicBool::new(false));
        #[cfg(feature = "test-support")]
        let host_reply_fault = lose_decision_reply.clone();
        #[cfg(feature = "test-support")]
        let lose_host_reply: Arc<Mutex<Option<String>>> = Arc::default();
        #[cfg(feature = "test-support")]
        let host_command_fault = lose_host_reply.clone();
        let expected = service.runtime_revision().await?;
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
            service.profile.registry(),
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
        .map_err(host_error)?;
        let runtime = proof
            .lock()
            .map_err(|_| Error::Unknown)?
            .take()
            .ok_or(Error::Unknown)?;
        Ok(Self {
            owner,
            service,
            runtime,
            loaded_definitions: std::sync::OnceLock::new(),
            gate: tokio::sync::Mutex::new(0),
            automatic: std::sync::atomic::AtomicU8::new(0),
            stopped_ack: std::sync::atomic::AtomicBool::new(false),
            graph_consumer: std::sync::atomic::AtomicBool::new(false),
            settlement_consumer: std::sync::atomic::AtomicBool::new(false),
            schedule_consumer: std::sync::atomic::AtomicBool::new(false),
            declared_events: Mutex::default(),
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
    pub(crate) fn automatic_state(&self) -> &'static str {
        match self.automatic.load(std::sync::atomic::Ordering::SeqCst) {
            1 => "active",
            2 => "held",
            3 => "stopping",
            4 => "stopped",
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
                == self
                    .service
                    .profile
                    .registry()
                    .events
                    .get("aq.signal.reported");
        let registered = artifact.clone();
        let declared: Vec<(String, String)> = artifact
            .content
            .registry
            .events
            .values()
            .filter_map(|e| Some((e.name.clone(), e.fingerprint().ok()?)))
            .collect();
        self.owner
            .call(
                Request::Install {
                    artifact: Box::new(artifact),
                },
                CALL,
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
        self.declared_events
            .lock()
            .map_err(|_| Error::Unknown)?
            .extend(declared);
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
        let active_authority = self
            .service
            .active_authority(&self.runtime, grant, revision)
            .await?;
        self.owner
            .call(
                Request::Activate {
                    binding: binding.clone(),
                    artifact,
                    authority: active_authority,
                },
                CALL,
            )
            .await
            .map_err(host_error)?;
        self.owner
            .call(Request::Binding { binding }, CALL)
            .await
            .map_err(|_| Error::Unknown)
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
                CALL,
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
                        now_ms: (self.service_clock()),
                    },
                    CALL,
                )
                .await
            {
                Ok(value) => value,
                Err(aq_playbook_host::Error::Overloaded) => return Err(Error::Overloaded),
                Err(_) => {
                    let _ = self.service.hold_admission(&self.runtime, id).await;
                    return Err(Error::Unknown);
                }
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
                CALL,
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
    /// At most two admissions and events, 64 destinations/event, and one
    /// eight-transition tick. Overlap refuses without growing a waiter queue.
    pub async fn quantum(&self) -> Result<serde_json::Value> {
        let mut cursor = self.gate.try_lock().map_err(|_| Error::Overloaded)?;
        let mut overloaded = false;
        let pending = self.service.pending_admissions(&self.runtime, 2).await?;
        for id in &pending {
            let result = self
                .admission(self.service.namespace.owner_user_id, *id, false)
                .await;
            if matches!(result, Err(Error::Overloaded)) {
                overloaded = true;
                continue;
            }
            if result.is_err() {
                // Retain failed/uncertain immutable work visibly. A poison or
                // expired first page must not starve every later admission.
                self.service.hold_admission(&self.runtime, *id).await?;
            }
        }
        defer_capacity(
            self.service.expire_decisions(&self.runtime, 2).await,
            &mut overloaded,
        )?;
        defer_capacity(
            self.service.collect_decision_events(&self.runtime, 2).await,
            &mut overloaded,
        )?;
        if self
            .graph_consumer
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            defer_capacity(
                self.service.collect_graph_events(&self.runtime, 2).await,
                &mut overloaded,
            )?;
        }
        if self
            .settlement_consumer
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            defer_capacity(
                self.service
                    .collect_settlement_events(&self.runtime, 2)
                    .await,
                &mut overloaded,
            )?;
        }
        // Catalog profile 7 work.ensure completion (aq.operation.updated).
        defer_capacity(
            self.service
                .collect_work_completions(&self.runtime, 2)
                .await,
            &mut overloaded,
        )?;
        if self.service.profile == super::review_domain::CatalogProfile::Monitoring6
            && self
                .schedule_consumer
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            defer_capacity(
                self.service.collect_schedule_events(&self.runtime, 2).await,
                &mut overloaded,
            )?;
        }
        let page = self.service.outbox(&self.runtime, *cursor, 2).await?;
        if page.is_empty() {
            *cursor = 0;
        }
        for (sequence, id, event) in &page {
            if self.service.profile.is_host2()
                && !self
                    .declared_events
                    .lock()
                    .map_err(|_| Error::Unknown)?
                    .contains(&(
                        event.contract.name.clone(),
                        event.contract.fingerprint.clone(),
                    ))
            {
                // No installed artifact consumes this contract: retire it
                // with zero destinations instead of wedging the page.
                let key = digest(&(&event.producer, 0_u64, id.to_string()))?;
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
            let delivery: Result<()> = async {
                let result = self
                    .owner
                    .call(
                        Request::Event {
                            authenticated_producer: event.producer.clone(),
                            epoch: 0,
                            event: Box::new(event.clone()),
                            now_ms: self.service_clock(),
                        },
                        CALL,
                    )
                    .await
                    .map_err(host_error)?;
                let receipt: EventDisposition = serde_json::from_value(result)?;
                self.owner
                    .call(
                        Request::Fanout {
                            event_key: receipt.key.clone(),
                            limit: 64,
                            now_ms: self.service_clock(),
                        },
                        CALL,
                    )
                    .await
                    .map_err(host_error)?;
                let status = self
                    .owner
                    .call(
                        Request::EventStatus {
                            event_key: receipt.key.clone(),
                        },
                        CALL,
                    )
                    .await
                    .map_err(host_error)?;
                let complete = status["pending"].as_u64().ok_or(Error::Unknown)? == 0;
                self.service
                    .acknowledge_event(&self.runtime, *id, receipt, complete)
                    .await?;
                Ok(())
            }
            .await;
            if matches!(delivery, Err(Error::Overloaded)) {
                overloaded = true;
                break;
            }
            delivery?;
            *cursor = *sequence;
        }
        self.owner
            .call(
                Request::CancelEffects {
                    now_ms: self.service_clock(),
                },
                CALL,
            )
            .await
            .map_err(host_error)?;
        let tick = self
            .owner
            .call(
                Request::Tick {
                    now_ms: self.service_clock(),
                },
                CALL,
            )
            .await
            .map_err(host_error)?;
        if matches!(
            self.service.profile,
            super::review_domain::CatalogProfile::Monitoring6
                | super::review_domain::CatalogProfile::Host7
                | super::review_domain::CatalogProfile::Host8
        ) {
            defer_capacity(
                self.service
                    .flush_finding_notifications(&self.runtime, 2)
                    .await,
                &mut overloaded,
            )?;
        }
        if overloaded {
            return Err(Error::Overloaded);
        }
        Ok(
            serde_json::json!({"admissions_checked":pending.len(),"events_checked":page.len(),"tick":tick}),
        )
    }
    pub async fn inspect_binding(&self, binding: String) -> Result<serde_json::Value> {
        if binding.is_empty() || binding.len() > 128 || binding.contains('\0') {
            return Err(Error::Invalid);
        }
        self.owner
            .call(Request::Binding { binding }, CALL)
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
                CALL,
            )
            .await
            .map_err(|_| Error::Unknown)
    }
    pub async fn cancel_run(&self, run: String) -> Result<()> {
        if run.is_empty() || run.len() > 512 {
            return Err(Error::Invalid);
        }
        self.owner
            .call(
                Request::Cancel {
                    run,
                    now_ms: self.service_clock(),
                },
                CALL,
            )
            .await
            .map_err(host_error)?;
        Ok(())
    }
    /// Startup replay never changes an existing activation or renews authority.
    pub(crate) async fn ensure_binding(
        &self,
        binding: String,
        artifact: String,
        grant: Uuid,
        scope: &super::domain::GrantScope,
    ) -> Result<()> {
        let expected = self.service.profile.authority(grant, 1, scope)?;
        match self
            .owner
            .call(
                Request::Binding {
                    binding: binding.clone(),
                },
                CALL,
            )
            .await
        {
            Ok(value) => {
                let old: aq_playbook_host::runner::Activation = serde_json::from_value(value)?;
                if old.artifact != artifact || old.authority != expected {
                    return Err(Error::Conflict);
                }
                Ok(())
            }
            Err(aq_playbook_host::Error::Invalid) => {
                self.activate(binding, artifact, grant, 1).await?;
                Ok(())
            }
            Err(_) => Err(Error::Unknown),
        }
    }
    /// D9: the binding's active artifact digest; `None` if never activated.
    pub(crate) async fn binding_artifact(&self, binding: String) -> Result<Option<String>> {
        match self.owner.call(Request::Binding { binding }, CALL).await {
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
        match self.owner.call(Request::Binding { binding }, CALL).await {
            Ok(value) => Ok(Some(serde_json::from_value(value)?)),
            Err(aq_playbook_host::Error::Invalid) => Ok(None),
            Err(_) => Err(Error::Unknown),
        }
    }
    /// Internal shutdown only: closing retained ownership needs no fresh user
    /// business grant. Stop is attempted even if central fencing is uncertain.
    pub(crate) async fn shutdown(&self) -> Result<()> {
        let fence = self.service.retire_for_shutdown(&self.runtime).await;
        let stop = self.owner.call(Request::Stop, CALL).await;
        stop_acknowledged(&self.stopped_ack, stop)?;
        fence
    }
    pub async fn stop(&self) -> Result<()> {
        self.service.retire_runtime(&self.runtime).await?;
        match self.owner.call(Request::Stop, CALL).await {
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
    pub async fn test_collect_work_completions(&self) -> Result<usize> {
        self.service
            .collect_work_completions(&self.runtime, 2)
            .await
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
                    now_ms: self.service_clock(),
                },
                CALL,
            )
            .await
            .map_err(host_error)?;
        serde_json::from_value(value).map_err(|_| Error::Unknown)
    }
}

// A closed channel is not engine release evidence. Only an exact successful
// Stop response, retained for this Driver, permits idempotent closed replay.
fn stop_acknowledged(
    known: &std::sync::atomic::AtomicBool,
    response: std::result::Result<serde_json::Value, aq_playbook_host::Error>,
) -> Result<()> {
    match response {
        Ok(_) => {
            known.store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        Err(aq_playbook_host::Error::Stopped)
            if known.load(std::sync::atomic::Ordering::SeqCst) =>
        {
            Ok(())
        }
        Err(_) => Err(Error::Unknown),
    }
}
#[cfg(test)]
mod shutdown_tests {
    use super::*;
    #[test]
    fn closed_queue_is_unknown_until_exact_stop_ack_for_this_driver() {
        let known = std::sync::atomic::AtomicBool::new(false);
        assert_eq!(
            stop_acknowledged(&known, Err(aq_playbook_host::Error::Stopped)),
            Err(Error::Unknown)
        );
        stop_acknowledged(&known, Ok(serde_json::Value::Null)).unwrap();
        stop_acknowledged(&known, Err(aq_playbook_host::Error::Stopped)).unwrap();
        assert_eq!(
            stop_acknowledged(&known, Err(aq_playbook_host::Error::Unknown)),
            Err(Error::Unknown)
        );
    }
}
