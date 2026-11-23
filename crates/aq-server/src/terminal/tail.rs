//! Authorized, disposable literal output only. This never claims a screen
//! snapshot, complete history, live process observation or input permission.
use super::persistence::{self, AccessSubject, Result, Right, configured, identity, load, now};
use crate::{
    AppState,
    error::AppError,
    execution::authority::{self, Policy},
};
pub use aq_terminal_relay::presentation::{
    Availability, MAX_FRAMES, MAX_RAW_BYTES, MAX_WIRE_BYTES, TailReply,
};
use uuid::Uuid;
pub async fn read(
    state: &AppState,
    actor: Uuid,
    id: Uuid,
    query: &aq_terminal_relay::api::ViewQuery,
) -> Result<TailReply> {
    tokio::time::timeout(
        std::time::Duration::from_secs(3),
        read_inner(&state.terminal_read_view(), actor, id, query),
    )
    .await
    .map_err(|_| AppError::IdentityUnavailable("terminal tail read unavailable".into()))?
}
async fn read_inner(
    state: &AppState,
    actor: Uuid,
    id: Uuid,
    query: &aq_terminal_relay::api::ViewQuery,
) -> Result<TailReply> {
    let config = configured(state)?;
    let terminal = identity(state, id).await?;
    let mut tx = persistence::begin_read(state).await?;
    // N-01, M1: a ~5/s tail poll is a snapshot read and waits on no row lock.
    let sql = "SELECT set_config('statement_timeout','2s',true)";
    (sqlx::query(sql).execute(&mut *tx).await).map_err(|_| AppError::Internal)?;
    let view = authority::read(
        state,
        &mut tx,
        &terminal.session,
        Policy::TerminalHistorical,
    )
    .await?;
    let access = AccessSubject::new(&view, &terminal, actor)
        .authorize(state, &mut tx, Right::Inspect)
        .await?;
    let loaded = load(state, &mut tx, &terminal, &view).await?;
    let checked_at = now(state);
    let watch = loaded.watch(access, query, checked_at)?;
    let expires_at = watch.expires_at()?;
    let replay = config.runtime.view(&watch)?;
    let active = match authority::read(state, &mut tx, &terminal.session, Policy::TerminalActive)
        .await
    {
        Ok(_) => true,
        Err(authority::AuthorityError::Database(_) | authority::AuthorityError::InvalidState) => {
            return Err(AppError::Internal);
        }
        Err(_) => false,
    };
    let until = watch.access_expires_at().min(expires_at);
    let before_commit = checked_clock(state, checked_at, until)?;
    tx.commit().await.map_err(|_| AppError::Internal)?;
    let clock = checked_clock(state, before_commit, until)?;
    let mut reply = TailReply {
        terminal,
        subscription_id: query.subscription_id,
        session_revision: loaded.authority_revision,
        server_time_ms: clock,
        expires_at_ms: expires_at,
        last_observed_at_ms: None,
        availability: Availability::NotRetained,
        disconnected: true,
        initial_incomplete: true,
        grid: None,
        frames: vec![],
        gaps: vec![],
        next_sequence: query.after_sequence,
        more: false,
    };
    if let Some(replay) = replay {
        reply.disconnected = !active
            || view.active_at(clock).is_err()
            || loaded.runtime_id != config.runtime_id
            || replay.disconnected
            || clock.saturating_sub(loaded.peer_observed_at) > 25000;
        reply.apply(replay, checked_at);
    }
    super::service::require_wire_bound(&reply, MAX_WIRE_BYTES)?;
    Ok(reply)
}

fn checked_clock(state: &AppState, after: u64, until: u64) -> Result<u64> {
    let clock = now(state);
    if (after..until).contains(&clock) {
        Ok(clock)
    } else {
        Err(AppError::IdentityUnavailable(
            "terminal inspection or watch expired".into(),
        ))
    }
}
