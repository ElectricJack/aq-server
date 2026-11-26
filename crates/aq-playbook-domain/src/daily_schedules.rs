//! Host-owned zoned daily calendars. PostgreSQL owns IANA DST rules:
//! gaps shift forward, folds choose standard time.

use crate::{Error, Namespace, Result};
use aq_playbook_host::model::{ContractRef, EventEnvelope};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

mod occurrences;
use occurrences::{Collection, StoredSchedule};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Misfire {
    Skip,
    CoalesceOnce,
    CatchUp { limit: u32 },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DailySchedule {
    pub schedule_id: Uuid,
    pub timezone: String,
    pub hour: u8,
    pub minute: u8,
    pub start_date: NaiveDate,
    pub misfire: Misfire,
}
impl DailySchedule {
    pub fn validate_request(&self, expected_revision: i64) -> Result<()> {
        if self.schedule_id.is_nil() || expected_revision < 0 {
            return Err(Error::Invalid);
        }
        if !self.valid_wall_time() || !self.misfire.valid_limit() {
            return Err(Error::Invalid);
        }
        Ok(())
    }

    fn valid_wall_time(&self) -> bool {
        (1..=128).contains(&self.timezone.len()) && self.hour < 24 && self.minute < 60
    }
}
impl Misfire {
    fn valid_limit(&self) -> bool {
        match self {
            Self::CatchUp { limit } => (1..=32).contains(limit),
            _ => true,
        }
    }
}

pub async fn put(
    tx: &mut Transaction<'_, Postgres>,
    namespace: &Namespace,
    schedule: &DailySchedule,
    expected_revision: i64,
    now_ms: i64,
) -> Result<i64> {
    validate_timezone(tx, &schedule.timezone).await?;
    let old: Option<i64> = sqlx::query_scalar("SELECT revision FROM playbook_schedules WHERE namespace_id=$1 AND generation=$2 AND schedule_id=$3")
        .bind(namespace.namespace_id)
        .bind(namespace.generation)
        .bind(schedule.schedule_id)
        .fetch_optional(&mut **tx)
        .await?;
    if old.unwrap_or(0) != expected_revision {
        return Err(Error::Conflict);
    }
    let revision = expected_revision.checked_add(1).ok_or(Error::Overloaded)?;
    let definition = serde_json::to_value(schedule)?;
    if old.is_none() {
        check_capacity(tx, namespace).await?;
    }
    let cursor = schedule.start_date.pred_opt().ok_or(Error::Invalid)?;
    sqlx::query("INSERT INTO playbook_schedules(namespace_id,generation,schedule_id,revision,definition,cursor_date,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(namespace_id,generation,schedule_id) DO UPDATE SET revision=EXCLUDED.revision,definition=EXCLUDED.definition,cursor_date=EXCLUDED.cursor_date")
        .bind(namespace.namespace_id)
        .bind(namespace.generation)
        .bind(schedule.schedule_id)
        .bind(revision)
        .bind(&definition)
        .bind(cursor)
        .bind(now_ms)
        .execute(&mut **tx)
        .await?;
    sqlx::query("INSERT INTO playbook_schedule_revisions(namespace_id,generation,schedule_id,revision,definition) VALUES($1,$2,$3,$4,$5)")
        .bind(namespace.namespace_id)
        .bind(namespace.generation)
        .bind(schedule.schedule_id)
        .bind(revision)
        .bind(definition)
        .execute(&mut **tx)
        .await?;
    Ok(revision)
}

async fn validate_timezone(tx: &mut Transaction<'_, Postgres>, timezone: &str) -> Result<()> {
    let zone: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_timezone_names WHERE name=$1)")
            .bind(timezone)
            .fetch_one(&mut **tx)
            .await?;
    if !zone {
        return Err(Error::Invalid);
    }
    Ok(())
}

async fn check_capacity(tx: &mut Transaction<'_, Postgres>, namespace: &Namespace) -> Result<()> {
    let count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM playbook_schedules WHERE namespace_id=$1 AND generation=$2",
    )
    .bind(namespace.namespace_id)
    .bind(namespace.generation)
    .fetch_one(&mut **tx)
    .await?;
    if count >= 128 {
        return Err(Error::Overloaded);
    }
    Ok(())
}

/// Update due cursors and return their occurrences in the caller's
/// transaction, which also seals and queues the entire page for delivery.
pub async fn collect(
    tx: &mut Transaction<'_, Postgres>,
    namespace: &Namespace,
    now: i64,
    limit: u32,
    contract: &ContractRef,
) -> Result<Vec<(Uuid, EventEnvelope)>> {
    // Filter due rows before the bounded page so idle schedules cannot
    // starve another timezone. Oldest cursors get catch-up turns first.
    let rows = sqlx::query("SELECT * FROM playbook_schedules WHERE namespace_id=$1 AND generation=$2 AND (((cursor_date + 1) + make_time((definition->>'hour')::int,(definition->>'minute')::int,0)) AT TIME ZONE (definition->>'timezone')) <= to_timestamp($4::bigint::double precision/1000) ORDER BY cursor_date,schedule_id LIMIT $3")
        .bind(namespace.namespace_id)
        .bind(namespace.generation)
        .bind(limit as i64)
        .bind(now)
        .fetch_all(&mut **tx)
        .await?;
    let page = Collection {
        namespace,
        now,
        contract,
    };
    let mut events = vec![];
    for row in rows {
        let stored = StoredSchedule::new(
            serde_json::from_value(row.get("definition"))?,
            row.get("revision"),
            row.get("cursor_date"),
        );
        stored.collect(tx, &page, &mut events).await?;
    }
    Ok(events)
}
