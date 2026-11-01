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
struct MailHost {
    service: Service,
    runtime: RuntimeOwner,
    handle: tokio::runtime::Handle,
    mode: HostMode,
    #[cfg(feature = "test-support")]
    lose_decision_reply: Arc<std::sync::atomic::AtomicBool>,
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
    #[cfg(feature = "test-support")]
    lose_decision_reply: Arc<std::sync::atomic::AtomicBool>,
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
                })
            },
        )
        .await
        .map_err(|_| Error::Unknown)?;
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
            #[cfg(feature = "test-support")]
            lose_decision_reply,
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
        self.owner
            .call(
                Request::Install {
                    artifact: Box::new(artifact),
                },
                CALL,
            )
            .await
            .map_err(|_| Error::Unknown)?;
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
            .map_err(|_| Error::Unknown)?;
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
            .map_err(|_| Error::Unknown)?;
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
    /// At most2 manual admissions,2 outbox events,64 fanout destinations/event and
    /// one8-transition engine tick. try_lock refuses overlap without waiter growth.
    pub async fn quantum(&self) -> Result<serde_json::Value> {
        let mut cursor = self.gate.try_lock().map_err(|_| Error::Overloaded)?;
        let pending = self.service.pending_admissions(&self.runtime, 2).await?;
        for id in &pending {
            if self
                .admission(self.service.namespace.owner_user_id, *id, false)
                .await
                .is_err()
            {
                // Retain failed/uncertain immutable work visibly. A poison or
                // expired first page must not starve every later admission.
                self.service.hold_admission(&self.runtime, *id).await?;
            }
        }
        self.service.expire_decisions(&self.runtime, 2).await?;
        self.service
            .collect_decision_events(&self.runtime, 2)
            .await?;
        let page = self.service.outbox(&self.runtime, *cursor, 2).await?;
        if page.is_empty() {
            *cursor = 0;
        }
        for (sequence, id, event) in &page {
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
                .map_err(|_| Error::Unknown)?;
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
                .map_err(|_| Error::Unknown)?;
            let status = self
                .owner
                .call(
                    Request::EventStatus {
                        event_key: receipt.key.clone(),
                    },
                    CALL,
                )
                .await
                .map_err(|_| Error::Unknown)?;
            let complete = status["pending"].as_u64().ok_or(Error::Unknown)? == 0;
            self.service
                .acknowledge_event(&self.runtime, *id, receipt, complete)
                .await?;
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
            .map_err(|_| Error::Unknown)?;
        let tick = self
            .owner
            .call(
                Request::Tick {
                    now_ms: self.service_clock(),
                },
                CALL,
            )
            .await
            .map_err(|_| Error::Unknown)?;
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
            .map_err(|_| Error::Unknown)?;
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
    /// Component-only lost callback response after the actual question SQL
    /// transaction. Never compiled into ordinary release builds or HTTP routes.
    pub fn test_lose_next_decision_reply(&self) {
        self.lose_decision_reply
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
    pub async fn test_collect_decision_events(&self) -> Result<usize> {
        self.service.collect_decision_events(&self.runtime, 2).await
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
            .map_err(|_| Error::Unknown)?;
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
