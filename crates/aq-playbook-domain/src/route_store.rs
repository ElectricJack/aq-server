//! Retained routing emissions used by the host's level-triggered route scan.
use crate::{Error, Namespace, Result, host9, stable_id};
use aq_playbook_host::model::{EventContract, EventEnvelope};
use sqlx::{PgConnection, Row};
use uuid::Uuid;

pub struct RouteCandidate {
    task: Uuid,
    pub emission: host9::Emission,
    since: Option<i64>,
    pub project: Option<Uuid>,
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
        sqlx::query("UPDATE route_needed_emissions SET error_retries=CASE WHEN task_version=$2 THEN error_retries ELSE 0 END,task_version=$2,emitted_at_ms=$3,checked_at_ms=0,outcome=NULL,reasons=NULL,artifact_digest=$4,options_digest=NULL WHERE task_id=$1")
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

/// Routing runs for these tasks were retired because a command of theirs was
/// refused for an expired deadline, with nothing done. Each task that still
/// waits for a route (placement saw it ready and unrouted) and has had fewer
/// than `retries` such retirements for the version last asked about is due to
/// be asked again at once. A task over the bound is left to the usual repeat
/// and to the raise to the owner of a task that stays unrouted. Returns how
/// many were made due.
pub async fn retry_after_error(
    db: &mut PgConnection,
    tasks: &[Uuid],
    retries: u32,
    now: i64,
) -> Result<u64> {
    if tasks.is_empty() || retries == 0 {
        return Ok(0);
    }
    let made_due = sqlx::query("UPDATE route_needed_emissions SET error_retries=error_retries+1,emitted_at_ms=LEAST(emitted_at_ms,$2),checked_at_ms=0 WHERE task_id=ANY($1) AND wanted_at_ms IS NOT NULL AND outcome IS NULL AND emitted_at_ms>0 AND error_retries<$3")
        .bind(tasks)
        .bind(now - host9::ROUTE_NEEDED_REPEAT_MS)
        .bind(i32::try_from(retries).unwrap_or(i32::MAX))
        .execute(db)
        .await?;
    Ok(made_due.rows_affected())
}
