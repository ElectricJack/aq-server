//! Host-owned calendar occurrences and domain-event collection.
use super::{domain::*, review_domain::CatalogProfile, service::*};
use aq_playbook_host::model::{ContractRef, EventEnvelope};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

pub use aq_playbook_domain::daily_schedules::{DailySchedule, Misfire};
use aq_playbook_domain::{daily_schedules, route_store};

impl Service {
    pub(super) fn monitoring(&self) -> Result<()> {
        if self.profile != CatalogProfile::Monitoring6 {
            return Err(Error::Forbidden);
        }
        Ok(())
    }
    pub(super) fn interval_event(&self) -> Result<Option<ContractRef>> {
        self.registry()
            .events
            .get("aq.signal.reported")
            .map(|event| {
                Ok(ContractRef {
                    name: event.name.clone(),
                    fingerprint: event.fingerprint().map_err(|_| Error::Invalid)?,
                })
            })
            .transpose()
    }
    /// Trusted host API, requiring the actual acquired namespace owner; there
    /// is no worker HTTP schedule-registration endpoint.
    pub async fn put_schedule(
        &self,
        owner: &RuntimeOwner,
        schedule: DailySchedule,
        expected_revision: i64,
    ) -> Result<i64> {
        self.monitoring()?;
        schedule.validate_request(expected_revision)?;
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        Self::owner(&mut tx, self.namespace.owner_user_id).await?;
        let revision = daily_schedules::put(
            &mut tx,
            &self.namespace,
            &schedule,
            expected_revision,
            self.now()?,
        )
        .await?;
        tx.commit().await?;
        Ok(revision)
    }
    pub async fn collect_domain_events(&self, owner: &RuntimeOwner, limit: u32) -> Result<usize> {
        self.monitoring()?;
        Ok(self.collect_graph_events(owner, limit).await?
            + self.collect_settlement_events(owner, limit).await?)
    }
    /// At most `limit` schedules and 32 catch-up dates per schedule. Refusal
    /// rolls back both cursor and outgoing work; no occurrence is dropped.
    pub async fn collect_schedule_events(&self, owner: &RuntimeOwner, limit: u32) -> Result<usize> {
        self.monitoring()?;
        if !(1..=2).contains(&limit) {
            return Err(Error::Invalid);
        }
        let now = self.now()?;
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        Self::owner(&mut tx, self.namespace.owner_user_id).await?;
        let contract = self.interval_event()?.ok_or(Error::Invalid)?;
        let events =
            daily_schedules::collect(&mut tx, &self.namespace, now, limit, &contract).await?;
        let count = events.len();
        for (id, event) in events {
            self.queue_occurrence(&mut tx, id, &event).await?;
        }
        tx.commit().await?;
        Ok(count)
    }
    /// B5: the server asks for a route. One `aq.work.route_needed` per cause
    /// for each ready task placement recorded as waiting
    /// (`routing::placements`), while a routing artifact is active; see
    /// `host9::route_needed_due`. R1: every project with
    /// `installation_routing`, else the namespace's.
    pub async fn collect_route_needed(&self, owner: &RuntimeOwner, limit: u32) -> Result<usize> {
        use aq_playbook_domain::host9;
        if !self.profile.has_routing() || self.host.is_none() {
            return Ok(0);
        }
        let now = self.now()?;
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        let (Some(artifact), _) = self.routing_artifact(&mut tx).await? else {
            return Ok(0);
        };
        let rows = route_store::candidates(
            &mut tx,
            &self.namespace,
            self.installation_routing,
            now,
            limit,
        )
        .await?;
        let offers = match rows
            .iter()
            .any(|row| row.emission.outcome.as_deref() == Some("not_offered"))
        {
            true => Some(host9::options_digest(
                &self
                    .host
                    .as_deref()
                    .ok_or(Error::Forbidden)?
                    .routing
                    .options()
                    .await?,
            )),
            false => None,
        };
        let contract = self.registry().events["aq.work.route_needed"].clone();
        let mut count = 0;
        for row in rows {
            // Raise waiting tasks once per version before deciding whether to ask again.
            if let Some(at) = row.escalation(now) {
                self.routing_failure(
                    at,
                    "unrouted",
                    row.emission.wanted_version,
                    &serde_json::Value::Null,
                )
                .await?;
            }
            if !host9::route_needed_due(&row.emission, now, &artifact, offers.as_deref()) {
                row.checked(&mut tx, now).await?;
                continue;
            }
            let (id, event) = row.event(&self.namespace, now, &contract)?;
            self.queue_occurrence(&mut tx, id, &event).await?;
            row.sent(&mut tx, now, &artifact).await?;
            count += 1;
        }
        tx.commit().await?;
        Ok(count)
    }
    /// Epic 0 (spec 0016 section 6.1): `aq.work.result_ready`, level-triggered
    /// like `collect_route_needed`. One event for each task whose current
    /// attempt has stopped and recorded a result that no playbook decision
    /// settled, while a delivery artifact is active. It is repeated every
    /// `decision_repeat_ms` (Settings), at once when main or the pull request
    /// moves, and never while a run that read the task is unfinished.
    #[cfg(feature = "delivery_playbook")]
    pub async fn collect_result_ready(&self, owner: &RuntimeOwner, limit: u32) -> Result<usize> {
        use aq_playbook_domain::delivery_store as store;
        let (Some(host), true) = (self.host.as_deref(), self.profile.is_delivery()) else {
            return Ok(0);
        };
        let (limits, _) = aq_role_settings::delivery_limits(&self.pool)
            .await
            .map_err(|_| Error::Unknown)?;
        let repeat = limits.decision_repeat_ms as i64;
        let bounds = (
            repeat,
            (limits.longest_check_wait_ms as i64).saturating_add(repeat),
        );
        let (now, ns_key) = (
            self.now()?,
            (self.namespace.namespace_id, self.namespace.generation),
        );
        let mut tx = self.tx().await?;
        self.runtime(&self.namespace_locked(&mut tx).await?, owner)?;
        let project = (!self.installation_routing).then_some(self.namespace.project_id);
        let due = store::plan(
            &mut tx,
            ns_key,
            &*self.delivery,
            project,
            limit,
            now,
            bounds,
        )
        .await?;
        let contract = self.registry().events["aq.work.result_ready"].clone();
        let mut count = 0;
        for d in due {
            let Some(node) = host
                .graph
                .node(d.pending.workspace_id, d.pending.task_id)
                .await?
            else {
                continue;
            };
            let (id, event) = d.event(
                node["version"].as_i64().ok_or(Error::Unknown)?,
                ns_key,
                now,
                &contract,
            )?;
            self.queue_occurrence(&mut tx, id, &event).await?;
            store::sent(&mut tx, &d, now).await?;
            count += 1;
        }
        tx.commit().await?;
        Ok(count)
    }
    async fn queue_occurrence(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        id: Uuid,
        event: &EventEnvelope,
    ) -> Result<()> {
        aq_playbook_host::model::validate_event(event, self.registry())
            .map_err(|_| Error::Invalid)?;
        self.seal_outbox(tx, id, event, self.now()?, None).await
    }
}
