//! Cancellation records for durable check attempts.
use crate::{Error, Namespace, Result};
use aq_playbook_host::runner::CancellationResult;
use sqlx::{PgConnection, Row};

pub enum CancelStage {
    Retained(CancellationResult),
    Settled(CancellationResult),
    Admitted,
}
pub struct Cancellations<'a> {
    namespace: &'a Namespace,
    intent_id: &'a str,
    identity: &'a str,
}
impl<'a> Cancellations<'a> {
    pub fn new(namespace: &'a Namespace, intent_id: &'a str, identity: &'a str) -> Self {
        Self {
            namespace,
            intent_id,
            identity,
        }
    }
    pub async fn require_open(&self, db: &mut PgConnection) -> Result<()> {
        // A runner cancellation confirmed before admission admits no attempt.
        if let Some(cancel) = sqlx::query_scalar::<_, String>("SELECT request_digest FROM playbook_cancellations WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3")
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(self.intent_id)
            .fetch_optional(db)
            .await?
        {
            return Err(if cancel == self.identity {
                Error::Forbidden
            } else {
                Error::Conflict
            });
        }
        Ok(())
    }
    pub async fn stage(
        &self,
        db: &mut PgConnection,
        key: &str,
        clock: impl FnOnce() -> Result<i64>,
    ) -> Result<CancelStage> {
        if let Some(row) = sqlx::query("SELECT request_digest,disposition FROM playbook_cancellations WHERE namespace_id=$1 AND generation=$2 AND cancellation_key=$3")
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(key)
            .fetch_optional(&mut *db)
            .await?
        {
            if row.get::<String, _>("request_digest") != self.identity {
                return Err(Error::Conflict);
            }
            return Ok(CancelStage::Retained(if row.get::<String, _>("disposition") == "confirmed" {
                CancellationResult::Confirmed
            } else {
                CancellationResult::TooLate
            }));
        }
        let effect = sqlx::query("SELECT request_digest,status FROM playbook_effects WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3")
            .bind(self.namespace.namespace_id)
            .bind(self.namespace.generation)
            .bind(self.intent_id)
            .fetch_optional(&mut *db)
            .await?;
        let Some(effect) = effect else {
            sqlx::query("INSERT INTO playbook_cancellations(namespace_id,generation,cancellation_key,intent_id,request_digest,disposition,created_at_ms) VALUES($1,$2,$3,$4,$5,'confirmed',$6)")
                .bind(self.namespace.namespace_id)
                .bind(self.namespace.generation)
                .bind(key)
                .bind(self.intent_id)
                .bind(self.identity)
                .bind(clock()?)
                .execute(&mut *db)
                .await?;
            return Ok(CancelStage::Settled(CancellationResult::Confirmed));
        };
        if effect.get::<String, _>("request_digest") != self.identity {
            return Err(Error::Conflict);
        }
        // A synchronously settled request (a typed refusal) has no operation.
        if effect.get::<String, _>("status") != "accepted" {
            return Ok(CancelStage::Settled(CancellationResult::TooLate));
        }
        Ok(CancelStage::Admitted)
    }
}
