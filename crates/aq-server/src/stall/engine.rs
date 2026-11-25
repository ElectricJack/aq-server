//! One ladder tick for one session: reconcile the in-flight intent, else read
//! evidence, step, persist the intent, run its effect and record the receipt.
use super::evidence::Evidence;
use super::ladder::{self, Intent, Outcome};
use super::policy::ExecutionPolicy;
use super::store;
use crate::error::AppError;
use async_trait::async_trait;
use sqlx::PgPool;

pub use aq_stall_ladder::run::{Action, Target, Tick};
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
        action: Action<'_>,
        tick: Tick<'_>,
    ) -> Result<Outcome, AppError>;
}

/// Returns the intent whose effect this tick receipted, if any.
pub async fn tick(
    pool: &PgPool,
    ports: &dyn Ports,
    target: &Target,
    tick: Tick<'_>,
) -> Result<Option<Intent>, AppError> {
    let Tick { policy, now } = tick;
    policy
        .validate()
        .map_err(|e| AppError::BadRequest(e.into()))?;
    let ep = store::open(pool, target, policy, now).await?;
    if let Some((id, intent)) = store::pending(pool, ep.episode_id).await? {
        let action = Action::new(id, &intent);
        let outcome = ports.effect(target, action, tick).await?;
        store::receipt(pool, ep.episode_id, action, (&outcome, now)).await?;
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
        .map(|i| Action::new(super::action_id(ep.episode_id, i.kind(), i.ordinal()), i));
    if !store::commit(pool, (&next, ep.revision), action, now).await? {
        return Ok(None);
    }
    let Some(action) = action else {
        return Ok(None);
    };
    let outcome = ports.effect(target, action, tick).await?;
    store::receipt(pool, ep.episode_id, action, (&outcome, now)).await?;
    Ok(Some(action.intent.clone()))
}
