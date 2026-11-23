//! Authorized, disposable literal output only. This never claims a screen
//! snapshot, complete history, live process observation or input permission.
use super::persistence::{self, AccessSubject, Result, Right, configured, identity, load, now};
use crate::{
    AppState,
    error::AppError,
    execution::authority::{self, Policy},
};
use aq_session_protocol::{OutputFrame, OutputGap, TerminalIdentity};
use aq_terminal_relay::output::View;
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
    pub grid: Option<(u16, u16)>,
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
    let until = watch.access.valid_until_ms.min(expires_at);
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
impl TailReply {
    fn apply(&mut self, replay: View, checked_at: u64) {
        let after = self.next_sequence;
        self.grid = replay.snapshot.as_ref().map(|s| (s.columns, s.rows));
        let observed = checked_at.saturating_sub(replay.age_ms);
        if observed > 0 {
            self.last_observed_at_ms = Some(observed);
            self.availability = Availability::ResidentTail;
        }
        let count = replay.frames.len();
        self.frames = super::service::bounded_frames(replay.frames, MAX_FRAMES, MAX_RAW_BYTES);
        self.more = self.frames.len() < count;
        let last_frame = self.frames.last().map_or(after, |f| f.sequence);
        // Do not advance over a retained frame omitted by the response budget.
        let cutoff = if self.more {
            last_frame
        } else {
            replay
                .gaps
                .iter()
                .map(|g| g.last_missing)
                .chain(replay.snapshot.as_ref().map(|s| s.through_sequence))
                .fold(last_frame, u64::max)
        }
        .max(after);
        self.gaps = replay
            .gaps
            .into_iter()
            .filter_map(|mut gap| {
                gap.first_missing = gap.first_missing.max(after.saturating_add(1));
                gap.last_missing = gap.last_missing.min(cutoff);
                (gap.first_missing <= gap.last_missing).then_some(gap)
            })
            .collect();
        self.next_sequence = cutoff;
    }
}
