//! One explicitly started owner and bounded service quanta. Never start another
//! owner because a request timed out.
use super::{domain::*, service::*};
use aq_playbook_domain::driver_outcomes::{
    Partial, decision_dispatch, delivery_error, host_error, host_result, message_dispatch,
    message_result,
};
use aq_playbook_host::{
    DomainHost, HostMode, Limits, Owner, Request,
    model::{CommandContract, EventEnvelope},
    plugins::{command_plugin, extension_capability},
    runner::{
        Authority, CancellationResult, DispatchResult, EventDisposition, Host, Intent,
        KeyedAdmissionDisposition, KeyedAdmissionReceipt, Storage,
    },
};
use aq_role_settings::driver::DriverLimits;
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use uuid::Uuid;
mod bindings;
mod mail_host;
mod quantum;
use aq_playbook_domain::driver_outcomes::retained_dispatch;
use mail_host::MailHost;
pub struct Driver {
    pub(super) owner: Owner,
    pub service: Service,
    pub(super) runtime: RuntimeOwner,
    pub(super) loaded_definitions:
        std::sync::OnceLock<super::inspection::definitions::LoadedDefinitions>,
    gate: tokio::sync::Mutex<i64>,
    delivery_recovery: aq_playbook_domain::delivery_decisions::Recovery,
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
struct DriverSettings {
    deadline: Duration,
    tick_deadline: Duration,
    retention: aq_playbook_host::retention::Retention,
    #[cfg(feature = "test-support")]
    lose_decision_reply: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(feature = "test-support")]
    lose_host_reply: Arc<Mutex<Option<String>>>,
}
impl DriverSettings {
    fn new(limits: &Limits) -> Self {
        Self {
            deadline: limits.timeouts.call_deadline(1),
            tick_deadline: limits.timeouts.call_deadline(limits.transitions as u32),
            retention: limits.retention,
            #[cfg(feature = "test-support")]
            lose_decision_reply: Arc::default(),
            #[cfg(feature = "test-support")]
            lose_host_reply: Arc::default(),
        }
    }
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
        let settings = DriverSettings::new(&limits);
        #[cfg(feature = "test-support")]
        let host_reply_fault = settings.lose_decision_reply.clone();
        #[cfg(feature = "test-support")]
        let host_command_fault = settings.lose_host_reply.clone();
        let expected = (service.runtime_revision().await)
            .map_err(|e| (e, "reading the runtime revision".into()))?;
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
            aq_playbook_host::Registration::new(namespace, limits, service.registry().clone()),
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
        .map_err(|e| (e.to_string(), Error::from(e)))
        .map_err(|(why, error)| (error, why))?;
        let taken = proof.lock().map_err(|_| Error::Unknown);
        let runtime = taken.and_then(|mut proof| proof.take().ok_or(Error::Unknown));
        let runtime = runtime.map_err(lost)?;
        Ok(Self::from_runtime(owner, service, runtime, settings))
    }
    fn from_runtime(
        owner: Owner,
        service: Service,
        runtime: RuntimeOwner,
        settings: DriverSettings,
    ) -> Self {
        Self {
            owner,
            service,
            runtime,
            loaded_definitions: std::sync::OnceLock::new(),
            gate: tokio::sync::Mutex::new(0),
            delivery_recovery: Default::default(),
            binding_gate: tokio::sync::Mutex::new(()),
            deadline: settings.deadline,
            tick_deadline: settings.tick_deadline,
            retention: settings.retention,
            pruned: Default::default(),
            reason: Mutex::default(),
            automatic: std::sync::atomic::AtomicU8::new(0),
            stopped_ack: std::sync::atomic::AtomicBool::new(false),
            graph_consumer: std::sync::atomic::AtomicBool::new(false),
            settlement_consumer: std::sync::atomic::AtomicBool::new(false),
            schedule_consumer: std::sync::atomic::AtomicBool::new(false),
            #[cfg(feature = "test-support")]
            lose_decision_reply: settings.lose_decision_reply,
            #[cfg(feature = "test-support")]
            lose_host_reply: settings.lose_host_reply,
        }
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
            .map_err(Error::from)?;
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
                        now_ms: self.service.current_time(),
                    },
                    self.deadline,
                )
                .await
            {
                Ok(value) => value,
                Err(aq_playbook_host::Error::Stale(_)) => {
                    let _ = self
                        .service
                        .hold(&self.runtime, HeldItem::Admission(id), "stale")
                        .await;
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
    pub async fn recover(&self) -> Result<serde_json::Value> {
        self.owner
            .call(
                Request::Recover {
                    now_ms: self.service.current_time(),
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
                    now_ms: self.service.current_time(),
                },
                self.deadline,
            )
            .await
            .map_err(Error::from)?;
        serde_json::from_value(value).map_err(|_| Error::Unknown)
    }
}
