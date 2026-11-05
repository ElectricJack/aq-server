//! Host-owned zoned daily calendars. Cursor + sealed occurrence commit before
//! delivery. PostgreSQL owns IANA DST rules: gaps shift forward, folds choose
//! standard time. Each local date fires at most once for a schedule revision.
use super::{domain::*, review_domain::CatalogProfile, service::*};
use aq_playbook_host::model::{ContractRef, EventEnvelope, canonical_bytes};
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
impl Service {
    pub(super) fn monitoring(&self) -> Result<()> {
        if self.profile != CatalogProfile::Monitoring6 {
            return Err(Error::Forbidden);
        }
        Ok(())
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
        if schedule.schedule_id.is_nil()
            || schedule.timezone.is_empty()
            || schedule.timezone.len() > 128
            || schedule.hour > 23
            || schedule.minute > 59
            || expected_revision < 0
            || matches!(schedule.misfire, Misfire::CatchUp{limit} if !(1..=32).contains(&limit))
        {
            return Err(Error::Invalid);
        }
        let mut tx = self.tx().await?;
        let ns = self.namespace_locked(&mut tx).await?;
        self.runtime(&ns, owner)?;
        Self::owner(&mut tx, self.namespace.owner_user_id).await?;
        let zone: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_timezone_names WHERE name=$1)")
                .bind(&schedule.timezone)
                .fetch_one(&mut *tx)
                .await?;
        if !zone {
            return Err(Error::Invalid);
        }
        let old:Option<i64>=sqlx::query_scalar("SELECT revision FROM playbook_schedules WHERE namespace_id=$1 AND generation=$2 AND schedule_id=$3")
            .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(schedule.schedule_id).fetch_optional(&mut *tx).await?;
        if old.unwrap_or(0) != expected_revision {
            return Err(Error::Conflict);
        }
        let revision = expected_revision.checked_add(1).ok_or(Error::Overloaded)?;
        let definition = serde_json::to_value(&schedule)?;
        self.budget(&mut tx, 0, 0, 0, 0, 8192).await?;
        if old.is_none() {
            let count: i64 = sqlx::query_scalar(
                "SELECT count(*) FROM playbook_schedules WHERE namespace_id=$1 AND generation=$2",
            )
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .fetch_one(&mut *tx)
            .await?;
            if count >= 128 {
                return Err(Error::Overloaded);
            }
        }
        let cursor = schedule.start_date.pred_opt().ok_or(Error::Invalid)?;
        sqlx::query("INSERT INTO playbook_schedules(namespace_id,generation,schedule_id,revision,definition,cursor_date,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT(namespace_id,generation,schedule_id) DO UPDATE SET revision=EXCLUDED.revision,definition=EXCLUDED.definition,cursor_date=EXCLUDED.cursor_date")
            .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(schedule.schedule_id).bind(revision).bind(&definition).bind(cursor).bind(self.now()?).execute(&mut *tx).await?;
        sqlx::query("INSERT INTO playbook_schedule_revisions(namespace_id,generation,schedule_id,revision,definition) VALUES($1,$2,$3,$4,$5)")
            .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(schedule.schedule_id).bind(revision).bind(definition).execute(&mut *tx).await?;
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
        // Filter due rows before the bounded page so idle schedules cannot
        // starve another timezone. Oldest cursors get catch-up turns first.
        let rows=sqlx::query("SELECT * FROM playbook_schedules WHERE namespace_id=$1 AND generation=$2 AND (((cursor_date + 1) + make_time((definition->>'hour')::int,(definition->>'minute')::int,0)) AT TIME ZONE (definition->>'timezone')) <= to_timestamp($4::bigint::double precision/1000) ORDER BY cursor_date,schedule_id LIMIT $3")
            .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(limit as i64).bind(now).fetch_all(&mut *tx).await?;
        let mut count = 0;
        for row in rows {
            let schedule: DailySchedule = serde_json::from_value(row.get("definition"))?;
            let revision: i64 = row.get("revision");
            let cursor: NaiveDate = row.get("cursor_date");
            let today: NaiveDate = sqlx::query_scalar(
                "SELECT (to_timestamp($1::bigint::double precision/1000) AT TIME ZONE $2)::date",
            )
            .bind(now)
            .bind(&schedule.timezone)
            .fetch_one(&mut *tx)
            .await?;
            let due_today = Self::calendar_due(&mut tx, &schedule, today).await?;
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
                    self.namespace.namespace_id,
                    self.namespace.generation,
                    &key,
                )?;
                let contract = self.profile.registry().events["aq.signal.reported"].clone();
                let event = EventEnvelope {
                    producer: "aq.host.bridge@1".into(),
                    event_id: id.to_string(),
                    contract: ContractRef {
                        name: contract.name.clone(),
                        fingerprint: contract.fingerprint().map_err(|_| Error::Invalid)?,
                    },
                    scope: "project".into(),
                    correlation: BTreeMap::from([("operation".into(), id.to_string())]),
                    subject: Some(schedule.schedule_id.to_string()),
                    revision: Some(revision as u64),
                    causation: None,
                    occurred_at_ms: Some(Self::calendar_due(&mut tx, &schedule, date).await?),
                    observed_at_ms: Some(now),
                    payload: json!({"outcome":"reported","reference":format!("aq.schedule/{key}"),"subject":{"id":schedule.schedule_id,"revision":revision}}),
                };
                self.queue_occurrence(&mut tx, id, &event).await?;
                count += 1;
            }
            sqlx::query("UPDATE playbook_schedules SET cursor_date=$4 WHERE namespace_id=$1 AND generation=$2 AND schedule_id=$3")
                .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(schedule.schedule_id).bind(next).execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok(count)
    }
    async fn calendar_due(
        tx: &mut Transaction<'_, Postgres>,
        schedule: &DailySchedule,
        date: NaiveDate,
    ) -> Result<i64> {
        Ok(sqlx::query_scalar("SELECT (extract(epoch FROM (($1::date + make_time($2,$3,0)) AT TIME ZONE $4))*1000)::bigint")
            .bind(date).bind(schedule.hour as i32).bind(schedule.minute as i32).bind(&schedule.timezone).fetch_one(&mut **tx).await?)
    }
    async fn queue_occurrence(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        id: Uuid,
        event: &EventEnvelope,
    ) -> Result<()> {
        aq_playbook_host::model::validate_event(event, &self.profile.registry())
            .map_err(|_| Error::Invalid)?;
        let hash = digest(event)?;
        let bytes = canonical_bytes(event).map_err(|_| Error::Invalid)?;
        let sealed = self
            .keys
            .seal(&self.aad("aq.playbooks.outbox", id, &hash)?, &bytes)
            .map_err(|_| Error::Unknown)?;
        self.budget(tx, 0, 0, 0, 1, sealed.ciphertext.len() as i64 + 4096)
            .await?;
        sqlx::query("INSERT INTO playbook_outbox(namespace_id,generation,producer,epoch,event_id,event_digest,sealed_version,key_id,nonce,ciphertext,created_at_ms,status) VALUES($1,$2,$3,0,$4,$5,$6,$7,$8,$9,$10,'pending')")
            .bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&event.producer).bind(id).bind(hash).bind(sealed.version as i32).bind(sealed.key_id).bind(sealed.nonce).bind(sealed.ciphertext).bind(self.now()?).execute(&mut **tx).await?;
        Ok(())
    }
}
