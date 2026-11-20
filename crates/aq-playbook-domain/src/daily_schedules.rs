//! Host-owned zoned daily calendars. PostgreSQL owns IANA DST rules:
//! gaps shift forward, folds choose standard time.

use crate::{Error, Namespace, Result, stable_id};
use aq_playbook_host::model::{ContractRef, EventEnvelope};
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{Postgres, Row, Transaction};
use std::collections::BTreeMap;
use uuid::Uuid;

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
        if self.schedule_id.is_nil()
            || self.timezone.is_empty()
            || self.timezone.len() > 128
            || self.hour > 23
            || self.minute > 59
            || expected_revision < 0
            || matches!(self.misfire, Misfire::CatchUp{limit} if !(1..=32).contains(&limit))
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}

pub async fn put(
    tx: &mut Transaction<'_, Postgres>,
    namespace: &Namespace,
    schedule: &DailySchedule,
    expected_revision: i64,
    now_ms: i64,
) -> Result<i64> {
    let zone: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_timezone_names WHERE name=$1)")
            .bind(&schedule.timezone)
            .fetch_one(&mut **tx)
            .await?;
    if !zone {
        return Err(Error::Invalid);
    }
    let old:Option<i64>=sqlx::query_scalar("SELECT revision FROM playbook_schedules WHERE namespace_id=$1 AND generation=$2 AND schedule_id=$3")
        .bind(namespace.namespace_id).bind(namespace.generation).bind(schedule.schedule_id).fetch_optional(&mut **tx).await?;
    if old.unwrap_or(0) != expected_revision {
        return Err(Error::Conflict);
    }
    let revision = expected_revision.checked_add(1).ok_or(Error::Overloaded)?;
    let definition = serde_json::to_value(schedule)?;
    if old.is_none() {
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
    }
    let cursor = schedule.start_date.pred_opt().ok_or(Error::Invalid)?;
    sqlx::query("INSERT INTO playbook_schedules(namespace_id,generation,schedule_id,revision,definition,cursor_date,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(namespace_id,generation,schedule_id) DO UPDATE SET revision=EXCLUDED.revision,definition=EXCLUDED.definition,cursor_date=EXCLUDED.cursor_date")
        .bind(namespace.namespace_id).bind(namespace.generation).bind(schedule.schedule_id).bind(revision).bind(&definition).bind(cursor).bind(now_ms).execute(&mut **tx).await?;
    sqlx::query("INSERT INTO playbook_schedule_revisions(namespace_id,generation,schedule_id,revision,definition) VALUES($1,$2,$3,$4,$5)")
        .bind(namespace.namespace_id).bind(namespace.generation).bind(schedule.schedule_id).bind(revision).bind(definition).execute(&mut **tx).await?;
    Ok(revision)
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
    let rows=sqlx::query("SELECT * FROM playbook_schedules WHERE namespace_id=$1 AND generation=$2 AND (((cursor_date + 1) + make_time((definition->>'hour')::int,(definition->>'minute')::int,0)) AT TIME ZONE (definition->>'timezone')) <= to_timestamp($4::bigint::double precision/1000) ORDER BY cursor_date,schedule_id LIMIT $3")
        .bind(namespace.namespace_id).bind(namespace.generation).bind(limit as i64).bind(now).fetch_all(&mut **tx).await?;
    let mut events = vec![];
    for row in rows {
        let schedule: DailySchedule = serde_json::from_value(row.get("definition"))?;
        let revision: i64 = row.get("revision");
        let cursor: NaiveDate = row.get("cursor_date");
        let today: NaiveDate = sqlx::query_scalar(
            "SELECT (to_timestamp($1::bigint::double precision/1000) AT TIME ZONE $2)::date",
        )
        .bind(now)
        .bind(&schedule.timezone)
        .fetch_one(&mut **tx)
        .await?;
        let due_today = calendar_due(tx, &schedule, today).await?;
        let latest = if due_today <= now {
            today
        } else {
            today.pred_opt().ok_or(Error::Invalid)?
        };
        if latest <= cursor {
            continue;
        }
        let first = cursor.succ_opt().ok_or(Error::Invalid)?;
        let dates: Vec<NaiveDate> = match schedule.misfire {
            Misfire::Skip => {
                if latest == today && now.saturating_sub(due_today) < 60_000 {
                    vec![today]
                } else {
                    vec![]
                }
            }
            Misfire::CoalesceOnce => vec![latest],
            Misfire::CatchUp { limit } => (0..limit)
                .filter_map(|n| first.checked_add_days(chrono::Days::new(n.into())))
                .take_while(|d| *d <= latest)
                .collect(),
        };
        let next = if matches!(schedule.misfire, Misfire::CatchUp { .. }) {
            dates.last().copied().unwrap_or(cursor)
        } else {
            latest
        };
        for date in dates {
            let key = format!("{}/{revision}/{date}", schedule.schedule_id);
            let id = stable_id(
                "aq.schedule.occurrence",
                namespace.namespace_id,
                namespace.generation,
                &key,
            )?;
            let event = EventEnvelope {
                producer: "aq.host.bridge@1".into(),
                event_id: id.to_string(),
                contract: contract.clone(),
                scope: "project".into(),
                correlation: BTreeMap::from([("operation".into(), id.to_string())]),
                subject: Some(schedule.schedule_id.to_string()),
                revision: Some(revision as u64),
                causation: None,
                occurred_at_ms: Some(calendar_due(tx, &schedule, date).await?),
                observed_at_ms: Some(now),
                payload: json!({"outcome":"reported","reference":format!("aq.schedule/{key}"),"subject":{"id":schedule.schedule_id,"revision":revision}}),
            };
            events.push((id, event));
        }
        sqlx::query("UPDATE playbook_schedules SET cursor_date=$4 WHERE namespace_id=$1 AND generation=$2 AND schedule_id=$3")
            .bind(namespace.namespace_id).bind(namespace.generation).bind(schedule.schedule_id).bind(next).execute(&mut **tx).await?;
    }
    Ok(events)
}

async fn calendar_due(
    tx: &mut Transaction<'_, Postgres>,
    schedule: &DailySchedule,
    date: NaiveDate,
) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT (extract(epoch FROM (($1::date + make_time($2,$3,0)) AT TIME ZONE $4))*1000)::bigint")
        .bind(date).bind(schedule.hour as i32).bind(schedule.minute as i32).bind(&schedule.timezone).fetch_one(&mut **tx).await?)
}
