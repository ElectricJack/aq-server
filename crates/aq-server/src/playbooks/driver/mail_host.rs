//! Dispatch and recover one engine intent through its owning bridge service.
use super::*;
pub(super) struct MailHost {
    pub(super) service: Service,
    pub(super) runtime: RuntimeOwner,
    pub(super) handle: tokio::runtime::Handle,
    pub(super) mode: HostMode,
    #[cfg(feature = "test-support")]
    pub(super) lose_decision_reply: Arc<std::sync::atomic::AtomicBool>,
    #[cfg(feature = "test-support")]
    pub(super) lose_host_reply: Arc<Mutex<Option<String>>>,
}
impl DomainHost for MailHost {
    fn transition_time(&self, _: i64) -> i64 {
        self.service.current_time()
    }
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
                ContractAuthorization {
                    grant: GrantRevision::new(id, revision),
                    contract,
                    authority,
                    live: self.mode == HostMode::Dispatch,
                },
            ))
            .is_ok()
    }
    fn dispatch(&mut self, intent: &Intent) -> DispatchResult {
        if self.host_command(intent) {
            return self.dispatch_adapter(intent, true);
        }
        let Ok((id, revision)) = grant_actor(&intent.authority) else {
            return DispatchResult::Rejected {
                reason: "grant binding refused".into(),
            };
        };
        if intent.command.name == "decision.request" {
            let result = self.handle.block_on(self.service.request_decision(
                &self.runtime,
                GrantRevision::new(id, revision),
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
            return decision_dispatch(self.handle.block_on(self.service.operation_get(
                &self.runtime,
                GrantRevision::new(id, revision),
                intent,
            )));
        }
        let sent =
            self.handle.block_on(
                self.service
                    .send_message(&self.runtime, id, revision, intent),
            );
        message_dispatch(sent)
    }
    fn reconcile(&mut self, intent: &Intent) -> DispatchResult {
        if self.mode != HostMode::Reconcile {
            return DispatchResult::Unknown {
                reason: "explicit recovery mode required".into(),
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
        if intent.command.name == "decision.request" {
            let retained = self.handle.block_on(
                self.service
                    .reconcile_decision(GrantRevision::new(id, revision), intent),
            );
            return retained_dispatch(
                retained,
                |result| decision_dispatch(Ok(result)),
                "no exact retained decision owner result",
            );
        }
        if self.host_command(intent) {
            let retained = self
                .handle
                .block_on(self.service.host_retained(id, revision, intent));
            return retained_dispatch(
                retained,
                |result| host_result(result, intent),
                "no retained host result; no new effect admitted",
            );
        }
        let retained = self
            .handle
            .block_on(self.service.reconcile(id, revision, intent));
        retained_dispatch(
            retained,
            message_result,
            "no verified retained owner result; no new write admitted",
        )
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
                .block_on(self.service.cancel_decision(
                    &self.runtime,
                    GrantRevision::new(id, revision),
                    intent,
                ))
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
                    .plugin_dispatch(
                        &self.runtime,
                        crate::playbooks::host_adapters::HostIntent::new(id, revision, intent),
                        false,
                    )
                    .await
            }
        });
        // D11 GATE-05 fixture: the AQ effect and its final row committed,
        // then the reply is lost before the runner records it.
        #[cfg(feature = "test-support")]
        if result.is_ok() && self.take_host_reply_fault(intent) {
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
    #[cfg(feature = "test-support")]
    fn take_host_reply_fault(&self, intent: &Intent) -> bool {
        let Ok(mut armed) = self.lose_host_reply.lock() else {
            return false;
        };
        if armed.as_deref() != Some(intent.command.name.as_str()) {
            return false;
        }
        *armed = None;
        true
    }
    /// Legacy review and mailbox profiles own their base adapters separately.
    fn host_command(&self, intent: &Intent) -> bool {
        let name = &intent.command.name;
        command_plugin(name).is_some()
            || (self.service.profile.is_host()
                && self.service.registry().commands.contains_key(name))
    }
}
