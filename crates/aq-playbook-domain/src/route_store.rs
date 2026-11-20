//! Retained routing emissions used by the host's level-triggered route scan.
use crate::{Error, Namespace, Result, host9, stable_id};
use aq_playbook_host::model::{EventContract, EventEnvelope};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

pub struct RouteCandidate {
    task: Uuid,
    pub emission: host9::Emission,
    since: Option<i64>,
    project: Option<Uuid>,
}

impl RouteCandidate {
    pub fn escalation(&self, now: i64) -> Option<(Uuid, Uuid)> {
        let (None, Some(since), Some(project)) = (&self.emission.outcome, self.since, self.project)
        else {
            return None;
        };
        (now - since >= host9::UNROUTED_RAISE_MS).then_some((self.task, project))
    }

    pub fn event(
        &self,
        namespace: &Namespace,
        now: i64,
        contract: &EventContract,
    ) -> Result<(Uuid, EventEnvelope)> {
        let task = self.task;
        let version = self.emission.wanted_version;
        let key = format!("{task}/{version}/{now}");
        let id = stable_id(
            "aq.work.route_needed",
            namespace.namespace_id,
            namespace.generation,
            &key,
        )?;
        let event = host9::route_needed_event(id, task, version, now, contract)
            .map_err(|_| Error::Invalid)?;
        Ok((id, event))
    }

    pub async fn checked(&self, db: &mut PgConnection, now: i64) -> Result<()> {
        sqlx::query("UPDATE route_needed_emissions SET checked_at_ms=$2 WHERE task_id=$1")
            .bind(self.task)
            .bind(now)
            .execute(db)
            .await?;
        Ok(())
    }

    pub async fn sent(&self, db: &mut PgConnection, now: i64, artifact: &str) -> Result<()> {
        sqlx::query("UPDATE route_needed_emissions SET task_version=$2,emitted_at_ms=$3,checked_at_ms=0,outcome=NULL,reasons=NULL,artifact_digest=$4,options_digest=NULL WHERE task_id=$1")
            .bind(self.task).bind(self.emission.wanted_version).bind(now).bind(artifact)
            .execute(db).await?;
        Ok(())
    }
}

pub async fn candidates(
    db: &mut PgConnection,
    namespace: &Namespace,
    installation_routing: bool,
    now: i64,
    limit: u32,
) -> Result<Vec<RouteCandidate>> {
    let rows = sqlx::query("SELECT r.task_id,r.project_id,r.wanted_since_ms,r.wanted_version,r.task_version,r.emitted_at_ms,r.outcome,r.artifact_digest,r.options_digest,EXISTS(SELECT 1 FROM playbook_effects e WHERE e.namespace_id=$1 AND e.generation=$2 AND e.command_name='work.read' AND e.receipt->'subject'->>'id'=r.task_id::text AND e.created_at_ms>=r.emitted_at_ms AND e.created_at_ms>$8 AND NOT playbook_run_finished($1,$2,e.run_id)) AS run_unfinished FROM route_needed_emissions r WHERE r.wanted_at_ms>=$3 AND ($4 OR r.project_id=$5) AND (r.emitted_at_ms=0 OR r.wanted_version<>r.task_version OR GREATEST(r.emitted_at_ms,r.checked_at_ms)<=$6) ORDER BY r.wanted_at_ms,r.task_id LIMIT $7 FOR UPDATE OF r SKIP LOCKED")
        .bind(namespace.namespace_id)
        .bind(namespace.generation)
        .bind(now - host9::ROUTE_WANTED_FRESH_MS)
        .bind(installation_routing)
        .bind(namespace.project_id)
        .bind(now - host9::ROUTE_NEEDED_REPEAT_MS)
        .bind(i64::from(limit))
        .bind(now - host9::ROUTE_RUN_BOUND_MS)
        .fetch_all(db)
        .await?;
    Ok(rows
        .into_iter()
        .map(|row| RouteCandidate {
            task: row.get("task_id"),
            since: row.get("wanted_since_ms"),
            project: row.get("project_id"),
            emission: host9::Emission {
                wanted_version: row.get("wanted_version"),
                emitted_version: row.get("task_version"),
                emitted_at_ms: row.get("emitted_at_ms"),
                outcome: row.get("outcome"),
                artifact_digest: row.get("artifact_digest"),
                options_digest: row.get("options_digest"),
                run_unfinished: row.get("run_unfinished"),
            },
        })
        .collect())
}
