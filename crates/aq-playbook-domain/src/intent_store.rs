//! Durable intent receipts and cancellations shared by mail and review commands.
use crate::{Error, Namespace, Result};
use aq_playbook_host::model::{ContractRef, EventEnvelope};
use aq_playbook_host::runner::{CancellationResult, Intent};
use serde_json::{Value, json};
use sqlx::{PgConnection, Row, postgres::PgRow};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub enum MessageResult {
    Queued(Value),
    Cancelled,
}

pub struct StoredIntent<'a> {
    pub(crate) namespace: &'a Namespace,
    pub intent: &'a Intent,
    pub digest: &'a str,
}
impl<'a> StoredIntent<'a> {
    pub fn new(namespace: &'a Namespace, intent: &'a Intent, digest: &'a str) -> Self {
        Self {
            namespace,
            intent,
            digest,
        }
    }
    pub async fn effect(&self, db: &mut PgConnection) -> Result<Option<PgRow>> {
        Ok(sqlx::query("SELECT request_digest,status,receipt FROM playbook_effects WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&self.intent.id).fetch_optional(db).await?)
    }
    pub fn response(&self, row: PgRow) -> Result<MessageResult> {
        self.check_digest(&row.get::<String, _>("request_digest"))?;
        Ok(if row.get::<String, _>("status") == "queued" {
            MessageResult::Queued(row.get("receipt"))
        } else {
            MessageResult::Cancelled
        })
    }
    fn check_digest(&self, known: &str) -> Result<()> {
        if known != self.digest {
            return Err(Error::Conflict);
        }
        Ok(())
    }
    pub async fn cancelled(&self, db: &mut PgConnection) -> Result<bool> {
        let stored=sqlx::query_scalar::<_,String>("SELECT request_digest FROM playbook_cancellations WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&self.intent.id).fetch_optional(db).await?;
        let Some(stored) = stored else {
            return Ok(false);
        };
        self.check_digest(&stored)?;
        Ok(true)
    }
    pub async fn confirmed_cancellation(&self, db: &mut PgConnection) -> Result<bool> {
        let row=sqlx::query("SELECT request_digest,disposition FROM playbook_cancellations WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&self.intent.id).fetch_optional(db).await?;
        let Some(row) = row else {
            return Ok(false);
        };
        self.check_digest(&row.get::<String, _>("request_digest"))?;
        if row.get::<String, _>("disposition") != "confirmed" {
            return Err(Error::Unknown);
        }
        Ok(true)
    }
    pub async fn retained_cancel(
        &self,
        db: &mut PgConnection,
        key: &str,
    ) -> Result<Option<CancellationResult>> {
        let row=sqlx::query("SELECT request_digest,disposition FROM playbook_cancellations WHERE namespace_id=$1 AND generation=$2 AND cancellation_key=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(key).fetch_optional(db).await?;
        let Some(row) = row else {
            return Ok(None);
        };
        self.check_digest(&row.get::<String, _>("request_digest"))?;
        Ok(Some(
            if row.get::<String, _>("disposition") == "confirmed" {
                CancellationResult::Confirmed
            } else {
                CancellationResult::TooLate
            },
        ))
    }
    pub async fn cancel(
        &self,
        db: &mut PgConnection,
        key: &str,
        clock: impl FnOnce() -> Result<i64>,
    ) -> Result<CancellationResult> {
        let existing=sqlx::query_scalar::<_,String>("SELECT request_digest FROM playbook_effects WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(&self.intent.id).fetch_optional(&mut *db).await?;
        let (disposition, result) = match existing {
            Some(digest) => {
                self.check_digest(&digest)?;
                ("too_late", CancellationResult::TooLate)
            }
            None => ("confirmed", CancellationResult::Confirmed),
        };
        self.record_cancel(db, key, (disposition, clock()?)).await?;
        Ok(result)
    }
    pub async fn record_cancel(
        &self,
        db: &mut PgConnection,
        key: &str,
        outcome: (&str, i64),
    ) -> Result<()> {
        let (disposition, at) = outcome;
        sqlx::query("INSERT INTO playbook_cancellations(namespace_id,generation,cancellation_key,intent_id,request_digest,disposition,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(self.namespace.namespace_id).bind(self.namespace.generation).bind(key).bind(&self.intent.id).bind(self.digest).bind(disposition).bind(at).execute(db).await?;
        Ok(())
    }
}

pub struct QueuedMail {
    pub op: Uuid,
    pub message: Uuid,
    pub subject: String,
    pub response: Value,
}

impl QueuedMail {
    pub fn event(&self, event_id: Uuid, intent_id: &str, at: i64) -> Result<EventEnvelope> {
        let c = crate::operation_event();
        let Self {
            op,
            message,
            subject,
            ..
        } = self;
        Ok(EventEnvelope {
            producer: "aq.host.bridge@1".into(),
            event_id: event_id.to_string(),
            contract: ContractRef {
                name: c.name.clone(),
                fingerprint: c.fingerprint().map_err(|_| Error::Invalid)?,
            },
            scope: "project".into(),
            correlation: BTreeMap::from([("operation".into(), op.to_string())]),
            subject: Some(subject.clone()),
            revision: None,
            causation: Some(intent_id.to_string()),
            occurred_at_ms: Some(at),
            observed_at_ms: Some(at),
            payload: json!({"operation_id":op,"message_id":message,"disposition":"queued"}),
        })
    }
}
