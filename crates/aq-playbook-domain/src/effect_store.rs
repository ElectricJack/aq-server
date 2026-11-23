//! Retained playbook effects within the caller's locked namespace transaction.
use crate::{Error, Namespace, Result, host_values::HostResult, stable_id};
use aq_playbook_host::runner::Intent;
use serde_json::Value;
use sqlx::{PgConnection, Row};
use uuid::Uuid;

#[derive(Clone, Copy)]
pub struct EffectIdentity<'a> {
    pub grant: Uuid,
    pub revision: i64,
    pub intent: &'a Intent,
    pub identity: &'a str,
}
pub struct Receipt<'a> {
    pub status: &'a str,
    pub value: &'a Value,
}
impl<'a> Receipt<'a> {
    pub fn new(status: &'a str, value: &'a Value) -> Self {
        Self { status, value }
    }
}
pub struct EffectRecord<'a> {
    pub effect: EffectIdentity<'a>,
    pub receipt: Receipt<'a>,
}

pub struct Retained {
    pub command_name: String,
    pub request_digest: String,
    pub status: String,
    pub receipt: Value,
}
pub struct Effects<'a>(pub &'a Namespace);
impl Effects<'_> {
    pub async fn lock(&self, db: &mut PgConnection, intent: &Intent) -> Result<Option<Retained>> {
        let row = sqlx::query("SELECT command_name,request_digest,status,receipt FROM playbook_effects WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3 FOR UPDATE")
            .bind(self.0.namespace_id)
            .bind(self.0.generation)
            .bind(&intent.id)
            .fetch_optional(db)
            .await?;
        Ok(row.map(|row| Retained {
            command_name: row.get("command_name"),
            request_digest: row.get("request_digest"),
            status: row.get("status"),
            receipt: row.get("receipt"),
        }))
    }
    pub async fn insert(
        &self,
        db: &mut PgConnection,
        record: EffectRecord<'_>,
        clock: impl FnOnce() -> Result<i64>,
    ) -> Result<()> {
        let EffectRecord { effect, receipt } = record;
        sqlx::query("INSERT INTO playbook_effects(namespace_id,generation,intent_id,run_id,command_name,request_digest,command_fingerprint,input_digest,grant_id,grant_revision,operation_id,status,receipt,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14)")
            .bind(self.0.namespace_id)
            .bind(self.0.generation)
            .bind(effect.intent.id.as_str())
            .bind(&effect.intent.run)
            .bind(&effect.intent.command.name)
            .bind(effect.identity)
            .bind(effect.intent.command.fingerprint().map_err(|_| Error::Invalid)?)
            .bind(&effect.intent.input_digest)
            .bind(effect.grant)
            .bind(effect.revision)
            .bind(self.operation(effect.intent)?)
            .bind(receipt.status)
            .bind(receipt.value)
            .bind(clock()?)
            .execute(db)
            .await?;
        Ok(())
    }
    pub async fn completed(
        &self,
        db: &mut PgConnection,
        intent: &Intent,
        identity: &str,
    ) -> Result<Option<HostResult>> {
        let row=sqlx::query("SELECT request_digest,status,receipt FROM playbook_effects WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3 FOR UPDATE")
            .bind(self.0.namespace_id)
            .bind(self.0.generation)
            .bind(&intent.id)
            .fetch_optional(db)
            .await?.ok_or(Error::Unknown)?;
        if row.get::<String, _>("request_digest") != identity {
            return Err(Error::Conflict);
        }
        let status: String = row.get("status");
        Ok((status != "pending").then(|| HostResult {
            outcome: status,
            output: row.get("receipt"),
        }))
    }
    pub async fn complete(
        &self,
        db: &mut PgConnection,
        intent: &Intent,
        result: &HostResult,
    ) -> Result<()> {
        sqlx::query("UPDATE playbook_effects SET status=$4,receipt=$5 WHERE namespace_id=$1 AND generation=$2 AND intent_id=$3 AND status='pending'")
            .bind(self.0.namespace_id)
            .bind(self.0.generation)
            .bind(&intent.id)
            .bind(&result.outcome)
            .bind(&result.output)
            .execute(db)
            .await?;
        Ok(())
    }

    pub fn operation(&self, intent: &Intent) -> Result<Uuid> {
        stable_id(
            &format!("aq.playbook.{}.operation", intent.command.name),
            self.0.namespace_id,
            self.0.generation,
            &intent.id,
        )
    }
}
