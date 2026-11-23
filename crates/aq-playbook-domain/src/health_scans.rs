//! Baselines and terminal coverage attestations for trusted health observations.
use crate::{Error, Namespace, Result};
use sqlx::{PgConnection, Row};
use std::collections::BTreeMap;

pub fn bounded(value: &str) -> bool {
    !value.is_empty() && value.len() <= 512 && !value.contains('\0')
}
pub struct ScanStart<'a> {
    pub observation: &'a str,
    pub partition: &'a str,
    pub ttl_ms: i64,
}
pub struct ScanEvaluation<'a> {
    pub observation: &'a str,
    pub evaluation: &'a str,
    pub complete: bool,
}
pub struct HealthScans<'a>(pub &'a Namespace);
impl HealthScans<'_> {
    pub async fn capture(
        &self,
        db: &mut PgConnection,
        request: ScanStart<'_>,
        clock: impl FnOnce() -> Result<i64>,
    ) -> Result<BTreeMap<String, i64>> {
        let ScanStart {
            observation,
            partition,
            ttl_ms,
        } = request;
        if ![observation, partition].into_iter().all(bounded) || !(1..=60_000).contains(&ttl_ms) {
            return Err(Error::Invalid);
        }
        if let Some(row) = sqlx::query("SELECT partition,baseline FROM playbook_health_observations WHERE namespace_id=$1 AND generation=$2 AND observation=$3")
            .bind(self.0.namespace_id).bind(self.0.generation).bind(observation).fetch_optional(&mut *db).await? {
            if row.get::<String,_>("partition") != partition { return Err(Error::Conflict); }
            return Ok(serde_json::from_value(row.get("baseline"))?);
        }
        let rows:Vec<(String,i64)> = sqlx::query_as("SELECT finding_key,revision FROM playbook_finding_episodes WHERE namespace_id=$1 AND generation=$2 AND partition=$3 ORDER BY finding_key")
            .bind(self.0.namespace_id).bind(self.0.generation).bind(partition).fetch_all(&mut *db).await?;
        let baseline: BTreeMap<_, _> = rows.into_iter().collect();
        let value = serde_json::to_value(&baseline)?;
        sqlx::query("INSERT INTO playbook_health_observations(namespace_id,generation,observation,partition,baseline,expires_at_ms) VALUES($1,$2,$3,$4,$5,$6)")
            .bind(self.0.namespace_id).bind(self.0.generation).bind(observation).bind(partition).bind(value).bind(clock()?.checked_add(ttl_ms).ok_or(Error::Invalid)?).execute(&mut *db).await?;
        Ok(baseline)
    }
    pub async fn complete(
        &self,
        db: &mut PgConnection,
        request: ScanEvaluation<'_>,
        clock: impl FnOnce() -> Result<i64>,
    ) -> Result<()> {
        let ScanEvaluation {
            observation,
            evaluation,
            complete,
        } = request;
        let row=sqlx::query("SELECT evaluation,complete,expires_at_ms FROM playbook_health_observations WHERE namespace_id=$1 AND generation=$2 AND observation=$3")
            .bind(self.0.namespace_id).bind(self.0.generation).bind(observation).fetch_optional(&mut *db).await?.ok_or(Error::Forbidden)?;
        if let Some(old) = row.get::<Option<String>, _>("evaluation") {
            if old != evaluation || row.get::<bool, _>("complete") != complete {
                return Err(Error::Conflict);
            }
        } else {
            if row.get::<i64, _>("expires_at_ms") <= clock()? {
                return Err(Error::Forbidden);
            }
            sqlx::query("UPDATE playbook_health_observations SET evaluation=$4,complete=$5 WHERE namespace_id=$1 AND generation=$2 AND observation=$3")
                .bind(self.0.namespace_id).bind(self.0.generation).bind(observation).bind(evaluation).bind(complete).execute(&mut *db).await?;
        }
        Ok(())
    }
}
