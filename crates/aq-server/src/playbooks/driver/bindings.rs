//! Install, activate and inspect artifacts and their owned bindings.
use super::*;
fn valid_binding(value: &str) -> bool {
    (1..=128).contains(&value.len()) && !value.contains('\0')
}
impl Driver {
    pub async fn install(&self, artifact: aq_playbook_host::model::Artifact) -> Result<()> {
        let event = crate::playbooks::review_domain::graph_operation_event();
        let consumer = artifact.content.registry.events.get(&event.name) == Some(&event);
        let completed = crate::playbooks::review_domain::task_completed_event();
        let settlement_consumer =
            artifact.content.registry.events.get(&completed.name) == Some(&completed);
        let schedule_consumer = self.service.profile
            == crate::playbooks::review_domain::CatalogProfile::Monitoring6
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
                    now_ms: self.service.current_time(),
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
    pub async fn inspect_binding(&self, binding: String) -> Result<serde_json::Value> {
        if !valid_binding(&binding) {
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
            now_ms: self.service.current_time(),
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
        scope: &crate::playbooks::domain::GrantScope,
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
                    now_ms: self.service.current_time(),
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
        if !valid_binding(&binding) || revision == 0 {
            return Err(Error::Invalid);
        }
        let _guard = self.binding_gate.lock().await;
        let result = self
            .owner
            .call(
                Request::DeactivateBinding {
                    binding: binding.clone(),
                    revision,
                    now_ms: self.service.current_time(),
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
        if !valid_binding(&binding) {
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
}
