use super::{now, refused};
use crate::{
    AppState,
    error::{AppError, db},
    machine_api::mapped,
    worker_api::AuthenticatedNode,
};
use aq_admission::transport::{DeliveryClock, Poll};
use aq_machine_control::slots;
use aq_profiled_state::git_author::OfferDelivery;
use sqlx::Row;
pub async fn existing(
    state: &AppState,
    auth: &AuthenticatedNode,
    request: &Poll,
    slot: u32,
) -> Result<Option<OfferDelivery>, AppError> {
    // Most polls have nothing to redeliver and share the node row with their
    // sibling slots. Loading an offer locks that row FOR UPDATE, and two slots
    // that each upgrade a shared lock wait for each other until one times out,
    // the same one on every poll. A slot that finds an offer under the shared
    // lock therefore releases it and looks again under the exclusive one.
    let mut exclusive = false;
    let (mut tx, attempt) = loop {
        let mut tx = state.store.pool.begin().await.map_err(db)?;
        if exclusive {
            crate::worker_api::lock_current_node(&mut tx, auth).await?;
        } else {
            crate::worker_api::read_current_node(&mut tx, auth).await?;
        }
        if !slots::redelivery_allowed(&mut tx, auth.id())
            .await
            .map_err(mapped)?
        {
            return Ok(None);
        }
        let boot = request.identity.boot_id;
        super::super::registration::current_runtime_locked(state, &mut tx, auth.id(), boot).await?;
        let offered = slots::offered_attempt(&mut tx, auth.id(), request.after_offer, slot);
        let Some(attempt) = offered.await.map_err(mapped)? else {
            return Ok(None);
        };
        if exclusive {
            break (tx, attempt);
        }
        tx.rollback().await.map_err(db)?;
        exclusive = true;
    };
    let view = crate::execution::authority::load_attempt_locked(
        state,
        &mut tx,
        auth.id(),
        attempt,
        crate::execution::authority::Policy::IdentityOnly,
    )
    .await
    .map_err(AppError::from)?;
    let hot: serde_json::Value =
        sqlx::query("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(&mut *tx)
            .await
            .map_err(|_| AppError::Internal)?
            .get("state");
    if !view.holds_task
        || view.revoked
        || view.cancellation_id.is_some()
        || view.grant_id.is_some()
        || view.stopped
        || hot["revoked"] != false
    {
        return Err(refused());
    }
    let mut offer =
        super::super::native_offer::load_original_delivery_locked(&mut tx, attempt).await?;
    let offered = &mut offer.native.offered;
    if offered.lifecycle_binding.session != view.session
        || !matches!(view.session,aq_session_protocol::SessionIdentity::Remote{boot_id,..}if boot_id==request.identity.boot_id)
    {
        return Err(refused());
    }
    let time = now(state)?;
    if time >= offered.clock.expires_at_ms {
        return Ok(None);
    }
    offered.clock = DeliveryClock::new(time, offered.clock.expires_at_ms).map_err(|_| refused())?;
    offer.validate().map_err(|_| refused())?;
    tx.commit().await.map_err(|_| AppError::Internal)?;
    Ok(Some(offer))
}
