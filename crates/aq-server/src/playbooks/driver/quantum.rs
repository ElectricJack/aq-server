//! Admit, collect, deliver and tick in the fixed order of one bounded owner quantum.
use super::*;
impl Driver {
    /// Admits, collects, delivers and ticks within the bounds of the driver
    /// Settings ([`DriverLimits`]), read at the start of each quantum. The reply's
    /// `more` says the tick reached a bound with work left, so the next
    /// quantum should follow without waiting an interval. Overlap refuses without a
    /// waiter queue.
    pub async fn quantum(&self) -> Result<serde_json::Value> {
        let mut partial = Partial::default();
        let result = self.quantum_with(&mut partial).await;
        *self.reason.lock().map_err(|_| Error::Unknown)? = partial.reason(&result);
        result
    }
    /// The drive's reason for its last failed or partial quantum.
    pub(crate) fn drive_reason(&self) -> Option<String> {
        self.reason.lock().ok().and_then(|reason| reason.clone())
    }
    /// The driver's limits Settings; the defaults if they cannot be read.
    async fn limits(&self, partial: &mut Partial) -> DriverLimits {
        let read = aq_role_settings::driver_limits(&self.service.pool).await;
        match read {
            Ok(limits) if limits.validate().is_ok() => limits,
            _ => {
                partial
                    .reasons
                    .push("reading the driver limits; the defaults apply".into());
                DriverLimits::default()
            }
        }
    }
    async fn quantum_with(&self, partial: &mut Partial) -> Result<serde_json::Value> {
        let mut cursor = self.gate.try_lock().map_err(|_| Error::Overloaded)?;
        let limits = self.limits(partial).await;
        let admissions = self.deliver_admissions(partial, limits.admissions).await?;
        self.collect_events(partial, &limits).await;
        let page = self.deliver_page(&mut cursor, partial, limits.outbox_events);
        let (events, undecryptable) = page.await?;
        let tick = self.tick_runs(partial, &limits).await?;
        // Only the tick promises progress with each pass: an outbox page can
        // hold rows that wait on other work, so a full page is not "more".
        let more = tick["more"] == true;
        self.finish_tick(partial).await;
        if partial.overloaded {
            return Err(Error::Overloaded);
        }
        if let Some(reason) = undecryptable {
            partial.reasons.push(reason);
            return Err(Error::Unknown);
        }
        Ok(
            serde_json::json!({"admissions_checked":admissions,"events_checked":events,"tick":tick,"more":more}),
        )
    }
    async fn deliver_admissions(&self, partial: &mut Partial, limit: u32) -> Result<usize> {
        let pending = self
            .service
            .pending_admissions(&self.runtime, limit)
            .await?;
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
                let hold = (self.service).hold(
                    &self.runtime,
                    HeldItem::Admission(*id),
                    reason.unwrap_or("refused"),
                );
                hold.await?;
            }
        }
        Ok(pending.len())
    }
    async fn collect_events(&self, partial: &mut Partial, limits: &DriverLimits) {
        let events = limits.events;
        partial.defer(
            "expiring decisions",
            self.service.expire_decisions(&self.runtime, events).await,
        );
        partial.defer(
            "collecting decision events",
            self.service
                .collect_decision_events(&self.runtime, events)
                .await,
        );
        if self
            .graph_consumer
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            partial.defer(
                "collecting graph events",
                self.service
                    .collect_graph_events(&self.runtime, events)
                    .await,
            );
        }
        if self
            .settlement_consumer
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            partial.defer(
                "collecting settlement events",
                self.service
                    .collect_settlement_events(&self.runtime, events)
                    .await,
            );
        }
        partial.defer(
            "asking for routes",
            self.service
                .collect_route_needed(&self.runtime, limits.routing_asks)
                .await,
        );
        partial.defer(
            "announcing results",
            self.service
                .collect_result_ready(&self.runtime, limits.announcements)
                .await,
        );
        #[cfg(feature = "project_environments")]
        partial.defer(
            "announcing base changes",
            self.service
                .collect_base_changes(&self.runtime, limits.announcements)
                .await,
        );
        partial.defer(
            "running delivery operations",
            self.service
                .collect_delivery_operations(&self.runtime, limits.delivery_operations)
                .await,
        );
        partial.defer(
            "expiring judgments",
            self.service.expire_assessments(&self.runtime, events).await,
        );
        // Catalog profile 7 work.ensure completion (aq.operation.updated).
        partial.defer(
            "collecting work completions",
            self.service
                .collect_work_completions(&self.runtime, events)
                .await,
        );
        self.collect_timed_events(partial, events).await;
    }

    async fn collect_timed_events(&self, partial: &mut Partial, events: u32) {
        if self.service.profile.is_host() {
            partial.defer(
                "collecting interval events",
                self.owner
                    .call(
                        Request::CollectIntervalEvents {
                            now_ms: self.service.current_time(),
                        },
                        self.deadline,
                    )
                    .await
                    .map_err(host_error),
            );
        }
        if self.service.profile == crate::playbooks::review_domain::CatalogProfile::Monitoring6
            && self
                .schedule_consumer
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            partial.defer(
                "collecting schedule events",
                self.service
                    .collect_schedule_events(&self.runtime, events)
                    .await,
            );
        }
    }
    async fn deliver_page(
        &self,
        cursor: &mut i64,
        partial: &mut Partial,
        limit: u32,
    ) -> Result<(usize, Option<String>)> {
        // A row that cannot be decrypted ends the page before it: event
        // delivery waits there (until the keyring is fixed or the owner holds
        // the row) while effects are still cancelled and runs still tick.
        let (page, undecryptable) = self
            .service
            .outbox_page(&self.runtime, *cursor, limit)
            .await?;
        if page.is_empty() && undecryptable.is_none() {
            *cursor = 0;
        }
        for (sequence, id, event, accepted) in &page {
            if self.can_retire_event(event, *accepted) {
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
                    let held = self
                        .service
                        .hold(&self.runtime, HeldItem::Event(*id), "refused");
                    held.await?
                }
            }
            *cursor = *sequence;
        }
        Ok((page.len(), undecryptable))
    }
    fn can_retire_event(&self, event: &EventEnvelope, accepted: bool) -> bool {
        let owned =
            self.service.profile.is_host2() || extension_capability(&event.producer).is_some();
        !accepted
            && owned
            && !self
                .owner
                .declares_event(&event.contract.name, &event.contract.fingerprint)
    }

    async fn tick_runs(
        &self,
        partial: &mut Partial,
        limits: &DriverLimits,
    ) -> Result<serde_json::Value> {
        let now_ms = self.service.current_time();
        let cancel = (self.owner).call(Request::CancelEffects { now_ms }, self.deadline);
        let cancelled = cancel.await.map_err(host_error);
        partial.defer("cancelling effects", cancelled.clone());
        cancelled?;
        if self.service.profile.is_delivery() {
            let recovery = self.delivery_recovery.tick(
                &self.service.pool,
                &self.service.namespace,
                &self.owner,
                self.tick_deadline,
                (self.service.current_time(), limits.route_error_retries),
            );
            partial.defer("reconciling delivery decisions", recovery.await);
        }
        let now_ms = self.service.current_time();
        let drive = Request::Drive {
            now_ms,
            transitions: limits.tick_transitions as usize,
            budget_ms: limits.tick_budget_ms,
        };
        let ticked = (self.owner).call(drive, self.tick_deadline);
        let tick = ticked.await.map_err(host_error);
        partial.defer("ticking runs", tick.clone());
        tick
    }
    async fn finish_tick(&self, partial: &mut Partial) {
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
                now_ms: self.service.current_time(),
            };
            let receipt = match self.owner.call(request, self.deadline).await {
                Err(aq_playbook_host::Error::Stale(_)) => {
                    return self
                        .service
                        .hold(&self.runtime, HeldItem::Event(id), "stale")
                        .await;
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
            now_ms: self.service.current_time(),
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
            Ok(status) => {
                if event.contract.name == "aq.work.result_ready" {
                    self.service
                        .record_delivery_runs(&self.runtime, id, event, &status["runs"])
                        .await?;
                }
                status["pending"].as_u64().ok_or(Error::Unknown)? == 0
            }
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
}
