//! One ladder tick for one session: reconcile the in-flight intent, else read
//! evidence, step, persist the intent, run its effect and record the receipt.
use super::evidence::Evidence;
use super::ladder::{self, Intent, Outcome};
use super::policy::ExecutionPolicy;
use super::store;
use crate::error::AppError;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

/// The profiled session the ladder watches. Its lineage is the task.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Target {
    pub attempt_id: Uuid,
    pub session_id: Uuid,
    pub task_id: Uuid,
    pub project_id: Uuid,
    pub workspace_id: Uuid,
    pub generation: u64,
}

/// Evidence and effects. Every effect must be idempotent by `action_id`.
#[async_trait]
pub trait Ports: Send + Sync {
    async fn evidence(
        &self,
        target: &Target,
        policy: &ExecutionPolicy,
        now: u64,
    ) -> Result<Evidence, AppError>;
    async fn effect(
        &self,
        target: &Target,
        action_id: Uuid,
        intent: &Intent,
        policy: &ExecutionPolicy,
        now: u64,
    ) -> Result<Outcome, AppError>;
}

/// Returns the intent whose effect this tick receipted, if any.
pub async fn tick(
    pool: &PgPool,
    ports: &dyn Ports,
    target: &Target,
    policy: &ExecutionPolicy,
    now: u64,
) -> Result<Option<Intent>, AppError> {
    policy
        .validate()
        .map_err(|e| AppError::BadRequest(e.into()))?;
    let ep = store::open(pool, target, policy, now).await?;
    if let Some((id, intent)) = store::pending(pool, ep.episode_id).await? {
        let outcome = ports.effect(target, id, &intent, policy, now).await?;
        store::receipt(pool, ep.episode_id, id, &intent, &outcome, now).await?;
        return Ok(Some(intent));
    }
    store::refresh_deliveries(pool, ep.episode_id).await?;
    if ep.rung.is_final() {
        return Ok(None);
    }
    let evidence = ports.evidence(target, policy, now).await?;
    let (next, intent) = ladder::step(&ep, &evidence, policy, now);
    if next == ep && intent.is_none() {
        return Ok(None);
    }
    let action = intent
        .as_ref()
        .map(|i| (super::action_id(ep.episode_id, i.kind(), i.ordinal()), i));
    if !store::commit(pool, &next, ep.revision, action, now).await? {
        return Ok(None);
    }
    let Some((id, intent)) = action else {
        return Ok(None);
    };
    let outcome = ports.effect(target, id, intent, policy, now).await?;
    store::receipt(pool, ep.episode_id, id, intent, &outcome, now).await?;
    Ok(Some(intent.clone()))
}
