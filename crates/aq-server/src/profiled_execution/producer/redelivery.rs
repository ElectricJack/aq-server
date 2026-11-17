use super::{now, refused};
use crate::{AppState, error::AppError, worker_api::AuthenticatedNode};
use aq_admission::{
    startup::NativeOffer,
    transport::{DeliveryClock, Poll},
};
use sqlx::Row;
use uuid::Uuid;
pub async fn existing(
    state: &AppState,
    auth: &AuthenticatedNode,
    request: &Poll,
    slot: u32,
) -> Result<Option<NativeOffer>, AppError> {
    let mut tx = state
        .store
        .pool
        .begin()
        .await
        .map_err(|_| AppError::Internal)?;
    crate::worker_api::lock_current_node(&mut tx, auth).await?;
    if !aq_machine_control::slots::redelivery_allowed(&mut tx, auth.id())
        .await
        .map_err(crate::machine_api::mapped)?
    {
        return Ok(None);
    }
    super::super::registration::current_runtime_locked(
        state,
        &mut tx,
        auth.id(),
        request.identity.boot_id,
    )
    .await?;
    let modern = aq_machine_control::slots::available(&mut tx)
        .await
        .map_err(crate::machine_api::mapped)?;
    let query = "SELECT a.attempt_id FROM execution_attempts a JOIN profiled_authority_hot h USING(attempt_id) LEFT JOIN execution_attempt_slots s USING(attempt_id) WHERE a.node_id=$1 AND a.kind='profiled_v1' AND a.holds_task AND COALESCE(s.slot,1)=$3 AND h.state->>'status'='offered' AND ($2::UUID IS NULL OR a.offer_id>$2) ORDER BY a.offer_id LIMIT 1 FOR UPDATE OF a,h";
    let query = if modern {
        query.to_owned()
    } else {
        query
            .replace("LEFT JOIN execution_attempt_slots s USING(attempt_id)", "")
            .replace("COALESCE(s.slot,1)", "1")
    };
    let attempt: Option<Uuid> = sqlx::query_scalar(&query)
        .bind(auth.id())
        .bind(request.after_offer)
        .bind(slot as i32)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| AppError::Internal)?;
    let Some(attempt) = attempt else {
        return Ok(None);
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
    let mut offer = super::load_original_offer_locked(&mut tx, attempt).await?;
    if offer.offered.lifecycle_binding.session != view.session
        || !matches!(view.session,aq_session_protocol::SessionIdentity::Remote{boot_id,..}if boot_id==request.identity.boot_id)
    {
        return Err(refused());
    }
    let time = now(state)?;
    if time >= offer.offered.clock.expires_at_ms {
        return Ok(None);
    }
    offer.offered.clock =
        DeliveryClock::new(time, offer.offered.clock.expires_at_ms).map_err(|_| refused())?;
    offer.validate().map_err(|_| refused())?;
    tx.commit().await.map_err(|_| AppError::Internal)?;
    Ok(Some(offer))
}
