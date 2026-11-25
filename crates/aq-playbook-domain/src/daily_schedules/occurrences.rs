use super::{DailySchedule, Misfire};
use crate::{Error, Namespace, Result, stable_id};
use aq_playbook_host::model::{ContractRef, EventEnvelope};
use chrono::NaiveDate;
use serde_json::json;
use sqlx::{Postgres, Transaction};
use std::collections::BTreeMap;
use uuid::Uuid;

pub(super) struct Collection<'a> {
    pub namespace: &'a Namespace,
    pub now: i64,
    pub contract: &'a ContractRef,
}

pub(super) struct StoredSchedule {
    schedule: DailySchedule,
    revision: i64,
    cursor: NaiveDate,
}

struct CalendarWindow {
    today: NaiveDate,
    due_today: i64,
    latest: NaiveDate,
}

impl StoredSchedule {
    pub(super) fn new(schedule: DailySchedule, revision: i64, cursor: NaiveDate) -> Self {
        Self {
            schedule,
            revision,
            cursor,
        }
    }

    pub(super) async fn collect(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        page: &Collection<'_>,
        events: &mut Vec<(Uuid, EventEnvelope)>,
    ) -> Result<()> {
        let window = self.window(tx, page.now).await?;
        if window.latest <= self.cursor {
            return Ok(());
        }
        let (dates, next) = window.occurrences(&self.schedule.misfire, self.cursor, page.now)?;
        for date in dates {
            events.push(self.event(tx, page, date).await?);
        }
        sqlx::query("UPDATE playbook_schedules SET cursor_date=$4 WHERE namespace_id=$1 AND generation=$2 AND schedule_id=$3")
            .bind(page.namespace.namespace_id)
            .bind(page.namespace.generation)
            .bind(self.schedule.schedule_id)
            .bind(next)
            .execute(&mut **tx)
            .await?;

        Ok(())
    }

    async fn window(&self, tx: &mut Transaction<'_, Postgres>, now: i64) -> Result<CalendarWindow> {
        let today: NaiveDate = sqlx::query_scalar(
            "SELECT (to_timestamp($1::bigint::double precision/1000) AT TIME ZONE $2)::date",
        )
        .bind(now)
        .bind(&self.schedule.timezone)
        .fetch_one(&mut **tx)
        .await?;
        let due_today = calendar_due(tx, &self.schedule, today).await?;
        let latest = if due_today <= now {
            today
        } else {
            today.pred_opt().ok_or(Error::Invalid)?
        };
        Ok(CalendarWindow {
            today,
            due_today,
            latest,
        })
    }

    async fn event(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        page: &Collection<'_>,
        date: NaiveDate,
    ) -> Result<(Uuid, EventEnvelope)> {
        let revision = self.revision;
        let key = format!("{}/{revision}/{date}", self.schedule.schedule_id);
        let id = stable_id(
            "aq.schedule.occurrence",
            page.namespace.namespace_id,
            page.namespace.generation,
            &key,
        )?;
        let event = EventEnvelope {
            producer: "aq.host.bridge@1".into(),
            event_id: id.to_string(),
            contract: page.contract.clone(),
            scope: "project".into(),
            correlation: BTreeMap::from([("operation".into(), id.to_string())]),
            subject: Some(self.schedule.schedule_id.to_string()),
            revision: Some(revision as u64),
            causation: None,
            occurred_at_ms: Some(calendar_due(tx, &self.schedule, date).await?),
            observed_at_ms: Some(page.now),
            payload: json!({"outcome":"reported","reference":format!("aq.schedule/{key}"),"subject":{"id":self.schedule.schedule_id,"revision":revision}}),
        };
        Ok((id, event))
    }
}

impl CalendarWindow {
    fn occurrences(
        &self,
        policy: &Misfire,
        cursor: NaiveDate,
        now: i64,
    ) -> Result<(Vec<NaiveDate>, NaiveDate)> {
        let first = cursor.succ_opt().ok_or(Error::Invalid)?;
        Ok(match *policy {
            Misfire::Skip => {
                let timely =
                    self.latest == self.today && now.saturating_sub(self.due_today) < 60_000;
                (
                    timely.then_some(self.today).into_iter().collect(),
                    self.latest,
                )
            }
            Misfire::CoalesceOnce => (vec![self.latest], self.latest),
            Misfire::CatchUp { limit } => {
                let dates: Vec<_> = (0..limit)
                    .filter_map(|n| first.checked_add_days(chrono::Days::new(n.into())))
                    .take_while(|date| *date <= self.latest)
                    .collect();
                let next = dates.last().copied().unwrap_or(cursor);
                (dates, next)
            }
        })
    }
}

async fn calendar_due(
    tx: &mut Transaction<'_, Postgres>,
    schedule: &DailySchedule,
    date: NaiveDate,
) -> Result<i64> {
    Ok(sqlx::query_scalar("SELECT (extract(epoch FROM (($1::date + make_time($2,$3,0)) AT TIME ZONE $4))*1000)::bigint")
        .bind(date)
        .bind(schedule.hour as i32)
        .bind(schedule.minute as i32)
        .bind(&schedule.timezone)
        .fetch_one(&mut **tx)
        .await?)
}
