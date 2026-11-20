//! Host-owned calendar occurrences and domain-event collection.
use super::{domain::*, review_domain::CatalogProfile, service::*};
use aq_playbook_host::model::{ContractRef, EventEnvelope, canonical_bytes};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

use aq_playbook_domain::daily_schedules;
pub use aq_playbook_domain::daily_schedules::{DailySchedule, Misfire};

impl Service {
    pub(super) fn monitoring(&self) -> Result<()> {
        if self.profile != CatalogProfile::Monitoring6 {
            return Err(Error::Forbidden);
        }
        Ok(())
    }
    pub(super) fn interval_event(&self) -> Result<Option<ContractRef>> {
        self.profile
            .registry()
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
        let rows = sqlx::query("SELECT r.task_id,r.project_id,r.wanted_since_ms,r.wanted_version,r.task_version,r.emitted_at_ms,r.outcome,r.artifact_digest,r.options_digest,EXISTS(SELECT 1 FROM playbook_effects e WHERE e.namespace_id=$1 AND e.generation=$2 AND e.command_name='work.read' AND e.receipt->'subject'->>'id'=r.task_id::text AND e.created_at_ms>=r.emitted_at_ms AND e.created_at_ms>$8 AND NOT playbook_run_finished($1,$2,e.run_id)) AS run_unfinished FROM route_needed_emissions r WHERE r.wanted_at_ms>=$3 AND ($4 OR r.project_id=$5) AND (r.emitted_at_ms=0 OR r.wanted_version<>r.task_version OR GREATEST(r.emitted_at_ms,r.checked_at_ms)<=$6) ORDER BY r.wanted_at_ms,r.task_id LIMIT $7 FOR UPDATE OF r SKIP LOCKED")
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(now - host9::ROUTE_WANTED_FRESH_MS)
            .bind(self.installation_routing)
            .bind(self.namespace.project_id)
            .bind(now - host9::ROUTE_NEEDED_REPEAT_MS)
            .bind(i64::from(limit))
            .bind(now - host9::ROUTE_RUN_BOUND_MS)
            .fetch_all(&mut *tx)
            .await?;
        let settled = |row: &sqlx::postgres::PgRow| row.get::<Option<String>, _>("outcome");
        let offers = match rows
            .iter()
            .any(|r| settled(r).as_deref() == Some("not_offered"))
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
            let (task, version): (Uuid, i64) = (row.get("task_id"), row.get("wanted_version"));
            let emission = host9::Emission {
                wanted_version: version,
                emitted_version: row.get("task_version"),
                emitted_at_ms: row.get("emitted_at_ms"),
                outcome: settled(&row),
                artifact_digest: row.get("artifact_digest"),
                options_digest: row.get("options_digest"),
                run_unfinished: row.get("run_unfinished"),
            };
            // W6: ten minutes ready without a route (and not already raised as
            // not_offered or unroutable) is raised, once per task version.
            let since: Option<i64> = row.get("wanted_since_ms");
            let project: Option<Uuid> = row.get("project_id");
            if let (None, Some(since), Some(project)) = (&emission.outcome, since, project)
                && now - since >= host9::UNROUTED_RAISE_MS
            {
                let at = (task, project);
                self.routing_failure(at, "unrouted", version, &serde_json::Value::Null)
                    .await?;
            }
            if !host9::route_needed_due(&emission, now, &artifact, offers.as_deref()) {
                sqlx::query("UPDATE route_needed_emissions SET checked_at_ms=$2 WHERE task_id=$1")
                    .bind(task)
                    .bind(now)
                    .execute(&mut *tx)
                    .await?;
                continue;
            }
            let (namespace, generation) = (self.namespace.namespace_id, self.namespace.generation);
            let key = format!("{task}/{version}/{now}");
            let id = stable_id("aq.work.route_needed", namespace, generation, &key)?;
            let event = host9::route_needed_event(id, task, version, now, &contract);
            self.queue_occurrence(&mut tx, id, &event.map_err(|_| Error::Invalid)?)
                .await?;
            sqlx::query("UPDATE route_needed_emissions SET task_version=$2,emitted_at_ms=$3,checked_at_ms=0,outcome=NULL,reasons=NULL,artifact_digest=$4,options_digest=NULL WHERE task_id=$1")
                .bind(task)
                .bind(version)
                .bind(now)
                .bind(&artifact)
                .execute(&mut *tx)
                .await?;
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
        let hash = digest(event)?;
        let bytes = canonical_bytes(event).map_err(|_| Error::Invalid)?;
        let sealed = self
            .keys
            .seal(&self.aad("aq.playbooks.outbox", id, &hash)?, &bytes)
            .map_err(|_| Error::Unknown)?;
        sqlx::query("INSERT INTO playbook_outbox(namespace_id,generation,producer,epoch,event_id,event_digest,sealed_version,key_id,nonce,ciphertext,created_at_ms,status) VALUES($1,$2,$3,0,$4,$5,$6,$7,$8,$9,$10,'pending')")
            .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&event.producer).bind(id).bind(hash).bind(sealed.version as i32).bind(sealed.key_id).bind(sealed.nonce).bind(sealed.ciphertext).bind(self.now()?).execute(&mut **tx).await?;
        Ok(())
    }
}
