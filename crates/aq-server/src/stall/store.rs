//! Durable ladder state. An episode advances only by compare-and-set on its
//! revision; an intent is inserted in the same transaction, before its effect,
//! and its receipt folds the outcome in exactly once.
use super::engine::Target;
use super::ladder::{self, Episode, Intent, Outcome};
use super::policy::ExecutionPolicy;
use crate::error::AppError;
use serde_json::json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::db;
fn decode<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Result<T, AppError> {
    serde_json::from_value(v).map_err(|_| AppError::Internal)
}
fn rung(ep: &Episode) -> serde_json::Value {
    json!(ep.rung)
}
fn ms(v: u64) -> Result<i64, AppError> {
    i64::try_from(v).map_err(|_| AppError::Internal)
}

/// The episode for `target`, created on first sight, with the lineage's spent
/// replacement count loaded from the lineage-wide budget.
pub async fn open(
    pool: &PgPool,
    target: &Target,
    policy: &ExecutionPolicy,
    now: u64,
) -> Result<Episode, AppError> {
    let fresh = Episode::open(target.attempt_id, target.task_id, policy, now);
    sqlx::query("INSERT INTO stall_episodes(episode_id,attempt_id,lineage_id,policy_version,revision,rung,state,updated_at_ms) VALUES($1,$2,$3,$4,0,$5,$6,$7) ON CONFLICT(attempt_id) DO NOTHING")
        .bind(fresh.episode_id)
        .bind(fresh.attempt_id)
        .bind(fresh.lineage_id)
        .bind(i32::try_from(policy.version).map_err(|_| AppError::Internal)?)
        .bind(rung(&fresh).as_str().unwrap_or_default())
        .bind(json!(fresh))
        .bind(ms(now)?)
        .execute(pool)
        .await
        .map_err(db)?;
    let row = sqlx::query("SELECT e.revision,e.state,COALESCE(b.replacements,0) AS spent FROM stall_episodes e LEFT JOIN stall_lineage_budgets b USING(lineage_id) WHERE e.attempt_id=$1")
        .bind(target.attempt_id)
        .fetch_one(pool)
        .await
        .map_err(db)?;
    let mut ep: Episode = decode(row.get("state"))?;
    ep.revision = u64::try_from(row.get::<i64, _>("revision")).map_err(|_| AppError::Internal)?;
    ep.lineage_replacements =
        u32::try_from(row.get::<i32, _>("spent")).map_err(|_| AppError::Internal)?;
    Ok(ep)
}

async fn write(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    ep: &Episode,
    expected: u64,
    now: u64,
) -> Result<bool, AppError> {
    let mut next = ep.clone();
    next.revision = expected + 1;
    let done = sqlx::query("UPDATE stall_episodes SET revision=$3,rung=$4,state=$5,updated_at_ms=$6 WHERE episode_id=$1 AND revision=$2")
        .bind(ep.episode_id)
        .bind(ms(expected)?)
        .bind(ms(next.revision)?)
        .bind(rung(&next).as_str().unwrap_or_default())
        .bind(json!(next))
        .bind(ms(now)?)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    Ok(done.rows_affected() == 1)
}

/// CAS the stepped episode and persist its intent before any effect runs.
/// `false` means another sweeper advanced the episode first.
pub async fn commit(
    pool: &PgPool,
    next: &Episode,
    expected: u64,
    action: Option<(Uuid, &Intent)>,
    now: u64,
) -> Result<bool, AppError> {
    let mut tx = pool.begin().await.map_err(db)?;
    if !write(&mut tx, next, expected, now).await? {
        return Ok(false);
    }
    if let Some((id, intent)) = action {
        sqlx::query("INSERT INTO stall_actions(action_id,episode_id,kind,ordinal,expected_revision,intent,created_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7)")
            .bind(id)
            .bind(next.episode_id)
            .bind(intent.kind())
            .bind(i32::try_from(intent.ordinal()).map_err(|_| AppError::Internal)?)
            .bind(ms(expected)?)
            .bind(json!(intent))
            .bind(ms(now)?)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
    }
    tx.commit().await.map_err(db)?;
    Ok(true)
}

/// The episode's in-flight (unreceipted) action, if any.
pub async fn pending(pool: &PgPool, episode: Uuid) -> Result<Option<(Uuid, Intent)>, AppError> {
    let row = sqlx::query(
        "SELECT action_id,intent FROM stall_actions WHERE episode_id=$1 AND receipt IS NULL",
    )
    .bind(episode)
    .fetch_optional(pool)
    .await
    .map_err(db)?;
    row.map(|r| Ok((r.get("action_id"), decode(r.get("intent"))?)))
        .transpose()
}

/// Record an effect receipt and fold the outcome into the episode, once.
pub async fn receipt(
    pool: &PgPool,
    episode: Uuid,
    action: Uuid,
    intent: &Intent,
    outcome: &Outcome,
    now: u64,
) -> Result<(), AppError> {
    let mut tx = pool.begin().await.map_err(db)?;
    let row = sqlx::query(
        "SELECT revision,state,lineage_id FROM stall_episodes WHERE episode_id=$1 FOR UPDATE",
    )
    .bind(episode)
    .fetch_one(&mut *tx)
    .await
    .map_err(db)?;
    let delivery = match outcome {
        Outcome::Nudged { delivery, .. } => Some(delivery.as_str()),
        _ => None,
    };
    let marked = sqlx::query("UPDATE stall_actions SET receipt=$2,receipted_at_ms=$3,delivery=$4 WHERE action_id=$1 AND receipt IS NULL")
        .bind(action)
        .bind(json!(outcome))
        .bind(ms(now)?)
        .bind(delivery)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    if marked.rows_affected() == 0 {
        return Ok(());
    }
    let revision = u64::try_from(row.get::<i64, _>("revision")).map_err(|_| AppError::Internal)?;
    let mut ep: Episode = decode(row.get("state"))?;
    ladder::apply(&mut ep, intent, outcome, now);
    if let (Intent::Replace, Outcome::Replaced { .. }) = (intent, outcome) {
        let lineage: Uuid = row.get("lineage_id");
        sqlx::query("INSERT INTO stall_lineage_budgets(lineage_id,replacements,updated_at_ms) VALUES($1,1,$2) ON CONFLICT(lineage_id) DO UPDATE SET replacements=stall_lineage_budgets.replacements+1,updated_at_ms=EXCLUDED.updated_at_ms")
            .bind(lineage)
            .bind(ms(now)?)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
    }
    if !write(&mut tx, &ep, revision, now).await? {
        return Err(AppError::Conflict("stall episode changed".into()));
    }
    tx.commit().await.map_err(db)?;
    Ok(())
}

/// Follow sent nudges into F3 delivery: handed to the harness (or
/// acknowledged) is `delivered`, an uncertain hand-off is `unknown`.
pub async fn refresh_deliveries(pool: &PgPool, episode: Uuid) -> Result<(), AppError> {
    sqlx::query("UPDATE stall_actions a SET delivery=CASE WHEN EXISTS(SELECT 1 FROM mailbox_receipts r WHERE r.message_id=n.message_id) OR EXISTS(SELECT 1 FROM mailbox_deliveries d WHERE d.message_id=n.message_id AND d.disposition='handed_to_harness') THEN 'delivered' WHEN EXISTS(SELECT 1 FROM mailbox_deliveries d WHERE d.message_id=n.message_id AND d.disposition='delivery_unknown') THEN 'unknown' ELSE a.delivery END FROM stall_nudges n WHERE n.action_id=a.action_id AND a.episode_id=$1 AND a.delivery='attempted'")
        .bind(episode)
        .execute(pool)
        .await
        .map_err(db)?;
    Ok(())
}
