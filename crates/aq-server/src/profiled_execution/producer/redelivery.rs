use super::{now, refused};
use crate::{
    AppState,
    error::{AppError, db},
    execution::authority::{self, AuthorityView},
    machine_api::mapped,
    worker_api::AuthenticatedNode,
};
use aq_admission::transport::{DeliveryClock, Poll};
use aq_machine_control::slots;
use aq_profiled_state::git_author::OfferDelivery;
use aq_session_protocol::SessionIdentity;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

type Tx = Transaction<'static, Postgres>;

pub async fn existing(
    state: &AppState,
    auth: &AuthenticatedNode,
    request: &Poll,
    slot: u32,
) -> Result<Option<OfferDelivery>, AppError> {
    let Some((mut tx, attempt)) = offered_attempt(state, auth, request, slot).await? else {
        return Ok(None);
    };
    let view = authority::load_attempt_locked(
        state,
        &mut tx,
        auth.id(),
        attempt,
        authority::Policy::IdentityOnly,
    )
    .await
    .map_err(AppError::from)?;
    let hot = hot_state(&mut tx, attempt).await?;
    if !redeliverable(&view, &hot) {
        return Err(refused());
    }
    let mut offer =
        super::super::native_offer::load_original_delivery_locked(&mut tx, attempt).await?;
    let offered = &mut offer.native.offered;
    let boot = request.identity.boot_id;
    if !same_remote_session(&offered.lifecycle_binding.session, &view.session, boot) {
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

/// The slot's offered attempt, with the transaction that found it.
///
/// No allocation lock: redelivery claims nothing new, so it neither waits for
/// a sibling slot's publication nor for an owner fence. An offer redelivered
/// as a Stop commits is an attempt the Stop already hands off.
async fn offered_attempt(
    state: &AppState,
    auth: &AuthenticatedNode,
    request: &Poll,
    slot: u32,
) -> Result<Option<(Tx, Uuid)>, AppError> {
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    crate::worker_api::lock_current_node(&mut tx, auth).await?;
    if !slots::redelivery_allowed(&mut tx, auth.id())
        .await
        .map_err(mapped)?
    {
        return Ok(None);
    }
    let boot = request.identity.boot_id;
    super::super::registration::current_runtime_locked(state, &mut tx, auth.id(), boot).await?;
    let offered = slots::offered_attempt(&mut tx, auth.id(), request.after_offer, slot);
    let attempt = offered.await.map_err(mapped)?;
    Ok(attempt.map(|attempt| (tx, attempt)))
}

async fn hot_state(tx: &mut Tx, attempt: Uuid) -> Result<serde_json::Value, AppError> {
    Ok(
        sqlx::query("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(&mut **tx)
            .await
            .map_err(|_| AppError::Internal)?
            .get("state"),
    )
}

/// The attempt still holds its task with no revocation, cancellation, grant
/// or stop recorded in either the authority view or the hot state.
fn redeliverable(view: &AuthorityView, hot: &serde_json::Value) -> bool {
    let live = !view.revoked && hot["revoked"] == false && view.cancellation_id.is_none();
    view.holds_task && live && view.grant_id.is_none() && !view.stopped
}

/// The offer is bound to the attempt's session, and that session is the
/// remote one of the polling boot.
fn same_remote_session(bound: &SessionIdentity, current: &SessionIdentity, boot: Uuid) -> bool {
    bound == current
        && matches!(current, SessionIdentity::Remote { boot_id, .. } if *boot_id == boot)
}
