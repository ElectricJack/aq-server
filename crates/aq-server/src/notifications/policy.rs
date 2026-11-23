//! F4 automatic owner notifications. Producers report attention episodes; the
//! policy decides whether a logical message is due and submits it through the
//! notification store. Lock order: policy advisory lock -> policy tables; the
//! store's own transaction runs only after the policy transaction commits.
//!
//! Episodes are keyed by kind + project + subject: one notification when an
//! episode opens, none again until it resolves and reopens. Message and
//! operation IDs derive from the episode generation, and the exact request is
//! frozen before submission, so a replayed source event re-submits the same
//! request and the store answers from its receipt instead of creating a second
//! intent. Delivery outcomes never feed back into this policy (N4).
use super::{Error, Result, persistence::Service};
use aq_lifecycle_api::Scope;
use aq_notifications::SendRequest;
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

/// Flood window length and the individual notifications allowed per window and
/// project; later episodes in the window collapse into one counted summary.
pub const FLOOD_WINDOW_MS: u64 = 60_000;
pub const FLOOD_INDIVIDUAL_LIMIT: i32 = 3;
const MAX_EPISODES: i64 = 4096;
const MAX_EVENTS: i64 = 8192;
const MAX_WINDOWS: i64 = 1024;
/// Receipts and resolved episodes retain a seven-day replay horizon. Open
/// episodes and unsubmitted intents are never collected.
pub const RETENTION_MS: u64 = 7 * 86_400_000;

pub use aq_notifications::policy::{Event, Kind, MILESTONE_GATE_LABEL, Transition};

#[derive(Clone)]
pub struct Policy {
    service: Service,
}

mod episode;
mod flood;
async fn lock(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(x'61716e6f74696679'::bigint)")
        .execute(&mut **tx)
        .await?;
    Ok(())
}
async fn owner(tx: &mut Transaction<'_, Postgres>) -> Result<Uuid> {
    sqlx::query_scalar("SELECT id FROM users WHERE role='owner' ORDER BY created_at,id LIMIT 1")
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(Error::NotFound)
}

async fn count(tx: &mut Transaction<'_, Postgres>, table: &str) -> Result<i64> {
    Ok(sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
        .fetch_one(&mut **tx)
        .await?)
}

impl Policy {
    /// Recover committed intents even when a polling producer already observes
    /// the episode as open. No network is performed by enqueue.
    pub async fn retry_pending(&self) -> Result<usize> {
        let rows = sqlx::query("SELECT source_event_id,request,response FROM notification_policy_events WHERE NOT submitted ORDER BY created_at_ms,source_event_id LIMIT 32")
            .fetch_all(&self.service.pool).await?;
        let mut submitted = 0;
        for row in rows {
            self.submit(row.get("request"), row.get("response")).await?;
            sqlx::query(
                "UPDATE notification_policy_events SET submitted=TRUE WHERE source_event_id=$1",
            )
            .bind(row.get::<Uuid, _>("source_event_id"))
            .execute(&self.service.pool)
            .await?;
            submitted += 1;
        }
        Ok(submitted)
    }

    /// Bounded retention under the same lock as observe/flush. Never remove an
    /// open episode, a pending intent, or an unsubmitted counted summary.
    pub async fn garbage_collect(&self) -> Result<u64> {
        let cutoff = self.service.now()?.saturating_sub(RETENTION_MS) as i64;
        let mut tx = self.service.pool.begin().await?;
        lock(&mut tx).await?;
        let mut removed = sqlx::query("DELETE FROM notification_policy_events WHERE source_event_id IN (SELECT source_event_id FROM notification_policy_events WHERE submitted AND created_at_ms<$1 ORDER BY created_at_ms LIMIT 512)")
            .bind(cutoff).execute(&mut *tx).await?.rows_affected();
        removed += sqlx::query("DELETE FROM notification_episodes WHERE episode_key IN (SELECT e.episode_key FROM notification_episodes e WHERE NOT e.open AND e.resolved_at_ms<$1 AND NOT EXISTS(SELECT 1 FROM notification_policy_events p WHERE NOT p.submitted AND p.response->>'episode_key'=e.episode_key) ORDER BY e.resolved_at_ms LIMIT 512)")
            .bind(cutoff).execute(&mut *tx).await?.rows_affected();
        removed += sqlx::query("DELETE FROM notification_flood_windows WHERE (project_id,workspace_id,window_start_ms) IN (SELECT project_id,workspace_id,window_start_ms FROM notification_flood_windows WHERE window_start_ms+$2<$1 AND (collapsed=0 OR submitted) ORDER BY window_start_ms LIMIT 512)")
            .bind(cutoff).bind(FLOOD_WINDOW_MS as i64).execute(&mut *tx).await?.rows_affected();
        tx.commit().await?;
        Ok(removed)
    }

    pub fn new(service: Service) -> Self {
        Self { service }
    }
    pub fn service(&self) -> &Service {
        &self.service
    }

    async fn submit(&self, request: Option<Value>, mut response: Value) -> Result<Value> {
        let Some(request) = request else {
            return Ok(response);
        };
        let request: SendRequest = serde_json::from_value(request)?;
        let actor = {
            let mut tx = self.service.pool.begin().await?;
            let id = owner(&mut tx).await?;
            tx.commit().await?;
            id
        };
        let stored = self.service.enqueue(actor, request).await?;
        response["external_state"] = stored["external_state"].clone();
        Ok(response)
    }
}
