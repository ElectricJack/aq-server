//! Authorized, disposable literal output only. This never claims a screen
//! snapshot, complete history, live process observation or input permission.
use super::persistence::{self, Result, Right, authorize, configured, identity, load, now};
use crate::{
    AppState,
    error::AppError,
    execution::authority::{self, Policy},
};
use aq_session_protocol::{OutputFrame, OutputGap, TerminalIdentity};
use serde::{Deserialize, Serialize};
use uuid::Uuid;
pub const MAX_FRAMES: usize = 64;
pub const MAX_RAW_BYTES: usize = 65536;
pub const MAX_WIRE_BYTES: usize = 524288;
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Availability {
    ResidentTail,
    NotRetained,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TailReply {
    pub terminal: TerminalIdentity,
    pub subscription_id: Uuid,
    pub session_revision: u64,
    pub server_time_ms: u64,
    pub expires_at_ms: u64,
    pub last_observed_at_ms: Option<u64>,
    pub availability: Availability,
    pub disconnected: bool,
    pub initial_incomplete: bool,
    pub frames: Vec<OutputFrame>,
    pub gaps: Vec<OutputGap>,
    pub next_sequence: u64,
    pub more: bool,
}
impl std::fmt::Debug for TailReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TailReply(<scoped disposable output>)")
    }
}
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
    let mut access = authorize(state, &mut tx, &view, &terminal, actor, Right::Inspect).await?;
    let loaded = load(state, &mut tx, &terminal, &view).await?;
    access.session_revision = loaded.authority_revision;
    let checked_at = now(state);
    loaded
        .ledger
        .authorize_watch(&access, query.subscription_id, checked_at)
        .map_err(persistence::relay)?;
    let subscription = loaded
        .ledger
        .summary()
        .subscriptions
        .into_iter()
        .find(|s| s.subscription_id == query.subscription_id)
        .ok_or(AppError::Internal)?;
    let replay = config.runtime.view(
        &loaded.ledger,
        &access,
        query.subscription_id,
        query.after_sequence,
        checked_at,
    )?;
    let active = match authority::read(state, &mut tx, &terminal.session, Policy::TerminalActive)
        .await
    {
        Ok(_) => true,
        Err(authority::AuthorityError::Database(_) | authority::AuthorityError::InvalidState) => {
            return Err(AppError::Internal);
        }
        Err(_) => false,
    };
    let before_commit = now(state);
    if before_commit < checked_at
        || before_commit >= access.valid_until_ms
        || before_commit >= subscription.expires_at_ms
    {
        return Err(AppError::IdentityUnavailable(
            "terminal inspection or watch expired".into(),
        ));
    }
    tx.commit().await.map_err(|_| AppError::Internal)?;
    let clock = now(state);
    if clock < before_commit
        || clock >= access.valid_until_ms
        || clock >= subscription.expires_at_ms
    {
        return Err(AppError::IdentityUnavailable(
            "terminal inspection or watch expired".into(),
        ));
    }
    let mut reply = TailReply {
        terminal,
        subscription_id: query.subscription_id,
        session_revision: loaded.authority_revision,
        server_time_ms: clock,
        expires_at_ms: subscription.expires_at_ms,
        last_observed_at_ms: None,
        availability: Availability::NotRetained,
        disconnected: true,
        initial_incomplete: true,
        frames: vec![],
        gaps: vec![],
        next_sequence: query.after_sequence,
        more: false,
    };
    if let Some(replay) = replay {
        let observed = checked_at.saturating_sub(replay.age_ms);
        if observed > 0 {
            reply.last_observed_at_ms = Some(observed);
            reply.availability = Availability::ResidentTail;
        }
        reply.disconnected = !active
            || view.active_at(clock).is_err()
            || loaded.runtime_id != config.runtime_id
            || replay.disconnected
            || clock.saturating_sub(loaded.peer_observed_at) > 25000;
        let count = replay.frames.len();
        let mut bytes = 0;
        for frame in replay.frames {
            if reply.frames.len() >= MAX_FRAMES || bytes + frame.bytes.len() > MAX_RAW_BYTES {
                break;
            }
            bytes += frame.bytes.len();
            reply.frames.push(frame);
        }
        reply.more = reply.frames.len() < count;
        let last_frame = reply
            .frames
            .last()
            .map_or(query.after_sequence, |f| f.sequence);
        // Do not advance over a retained frame omitted by the response budget.
        let cutoff = if reply.more {
            last_frame
        } else {
            replay
                .gaps
                .iter()
                .map(|g| g.last_missing)
                .chain(replay.snapshot.as_ref().map(|s| s.through_sequence))
                .fold(last_frame, u64::max)
        }
        .max(query.after_sequence);
        reply.gaps = replay
            .gaps
            .into_iter()
            .filter_map(|mut gap| {
                gap.first_missing = gap
                    .first_missing
                    .max(query.after_sequence.saturating_add(1));
                gap.last_missing = gap.last_missing.min(cutoff);
                (gap.first_missing <= gap.last_missing).then_some(gap)
            })
            .collect();
        reply.next_sequence = cutoff;
    }
    if serde_json::to_vec(&reply)
        .map_err(|_| AppError::Internal)?
        .len()
        > MAX_WIRE_BYTES
    {
        return Err(AppError::Internal);
    }
    Ok(reply)
}
