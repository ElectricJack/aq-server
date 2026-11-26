//! Node-outbound relay. Correlation IDs do not authorize effects; each enclosed
//! receipt/fence/capture is checked against the encrypted canonical ledger.
mod dispatch;
use dispatch::dispatch_next;
mod outbox;
use outbox::poll_once;
mod uploads;
use crate::terminal::persistence;
pub use uploads::{control_upload, upload};
type Tx = Transaction<'static, Postgres>;
use super::{
    persistence::*,
    protection::{Context, Purpose, Sealed},
};
pub use crate::worker_api::AuthenticatedNode;
use crate::{
    AppState,
    error::AppError,
    execution::authority::{self, Policy, Rows},
};
use aq_session_protocol::TerminalIdentity;
use aq_terminal_relay::{
    Ledger,
    draft::{self, Peer, PeerUpload, UploadAck},
};
use sqlx::{Postgres, Transaction};
use std::collections::BTreeMap;
use uuid::Uuid;
pub async fn authenticate(
    state: &AppState,
    headers: &axum::http::HeaderMap,
) -> Result<AuthenticatedNode> {
    crate::delegated_permits::runtime::authenticate_runner(
        state,
        headers,
        aq_admission_sandbox::sandbox::RunnerOperation::Terminal,
        None,
    )
    .await
}
/// The peer's node, credential and boot are current; `begin` already checked the owner.
/// `Rows::Snapshot` reads a node-signed request's node row unlocked (uploads, registration).
pub(super) async fn peer_rows(
    tx: &mut Transaction<'_, Postgres>,
    node: &AuthenticatedNode,
    peer: &Peer,
    rows: Rows,
) -> Result<()> {
    let Peer::Remote { node_id, boot_id } = peer else {
        return Err(AppError::Conflict(
            "central terminal authority unavailable".into(),
        ));
    };
    if node.id() != *node_id {
        return Err(AppError::Forbidden("terminal peer mismatch".into()));
    }
    match rows {
        Rows::Snapshot if node.delegated().is_none() => {
            crate::worker_api::snapshot_current_node(tx, node).await?
        }
        _ => crate::worker_api::lock_current_node(tx, node).await?,
    };
    let current: Option<Uuid> = sqlx::query_scalar(
        "SELECT (state->'identity'->>'boot_id')::uuid FROM execution_nodes WHERE node_id=$1",
    )
    .bind(node.id())
    .fetch_optional(&mut **tx)
    .await
    .map_err(|_| AppError::Internal)?;
    if current != Some(*boot_id) {
        return Err(AppError::Conflict("stale terminal peer boot".into()));
    }
    Ok(())
}
/// A poll cursor is only a transport watermark for replayable outbox entries.
/// Claimed input is excluded regardless of cursor, including a lost HTTP response.
pub async fn poll(
    state: &AppState,
    node: &AuthenticatedNode,
    r: &draft::RelayPoll,
) -> Result<draft::Envelope> {
    r.validate()
        .map_err(|_| AppError::BadRequest("invalid terminal poll".into()))?;
    let state = state.terminal_view();
    let config = configured(&state)?;
    let slot = config
        .runtime
        .poll_channel(node.id(), node.permitted_attempt())?;
    if r.hold_ms > 0 {
        let mut tx = super::persistence::begin(&state).await?;
        peer_rows(&mut tx, node, &r.peer, Rows::Lock).await?;
        let work:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM terminal_subscriptions w JOIN terminal_sessions s ON s.terminal_id=w.terminal_id WHERE w.node_id=$1 AND s.boot_id=$3 AND ($4::uuid IS NULL OR s.attempt_id=$4) AND w.expires_at_ms>$2) OR EXISTS(SELECT 1 FROM terminal_outbox o JOIN terminal_sessions s ON s.terminal_id=o.terminal_id WHERE s.node_id=$1 AND s.boot_id=$3 AND ($4::uuid IS NULL OR s.attempt_id=$4) AND o.kind='fence' AND o.controller_generation=s.controller_generation AND(o.expires_at_ms>$2 OR(o.expires_at_ms IS NULL AND NOT o.acknowledged)))").bind(node.id()).bind(dbtime(now(&state))?).bind(remote_boot(&r.peer)?).bind(node.permitted_attempt()).fetch_one(&mut *tx).await.map_err(|_|AppError::Internal)?;
        tx.commit().await.map_err(|_| AppError::Internal)?;
        if !work {
            return Err(AppError::Conflict(
                "terminal hold requires current subscription or controller work".into(),
            ));
        }
    }
    let first = poll_once(&state, node, r, PollMode::Full).await?;
    if !first.effects.is_empty() || r.hold_ms == 0 {
        return Ok(first);
    }
    // No SQL connection or authority lock survives this wait.
    slot.wait(r.hold_ms).await;
    poll_once(&state, node, r, PollMode::Full).await
}
/// Explicit output subscription delivery. Activity hints on the full poll keep
/// their original meaning; this route cannot claim or execute control effects.
pub async fn output_poll(
    state: &AppState,
    node: &AuthenticatedNode,
    r: &draft::RelayPoll,
) -> Result<draft::Envelope> {
    r.validate()
        .map_err(|_| AppError::BadRequest("invalid terminal output poll".into()))?;
    if r.hold_ms != 0 || r.active_controllers != 0 {
        return Err(AppError::BadRequest(
            "terminal output poll requires zero hold and controllers".into(),
        ));
    }
    // Read-only, so the node's agents poll in parallel: no per-channel slot.
    poll_once(&state.terminal_view(), node, r, PollMode::Output).await
}
/// A node-signed output poll only reads: a `begin_read` snapshot without row locks, so the
/// node's agents never queue on its row. Delegated polls keep the locked permit recheck.
async fn begin_peer(
    state: &AppState,
    node: &AuthenticatedNode,
    r: &draft::RelayPoll,
    mode: PollMode,
) -> Result<(Tx, Rows)> {
    if mode == PollMode::Output && node.delegated().is_none() {
        let mut tx = begin_read(state).await?;
        peer_rows(&mut tx, node, &r.peer, Rows::Snapshot).await?;
        return Ok((tx, Rows::Snapshot));
    }
    let mut tx = super::persistence::begin(state).await?;
    peer_rows(&mut tx, node, &r.peer, Rows::Lock).await?;
    Ok((tx, Rows::Lock))
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum PollMode {
    Full,
    Output,
}
async fn active(state: &AppState, tx: &mut Tx, t: &TerminalIdentity, rows: Rows) -> Result<bool> {
    match authority::load(state, tx, &t.session, Policy::TerminalActive, rows).await {
        Ok(_) => Ok(true),
        Err(authority::AuthorityError::Database(_) | authority::AuthorityError::InvalidState) => {
            Err(AppError::Internal)
        }
        Err(_) => Ok(false),
    }
}
// Avoid recursive async futures and bound recovery work to one retry. Other
// sessions recover on later polls; no input is dispatched from recovered bytes.
async fn poll_once_after_recovery(
    state: &AppState,
    node: &AuthenticatedNode,
    r: &draft::RelayPoll,
) -> Result<draft::Envelope> {
    let mut tx = super::persistence::begin(state).await?;
    peer_rows(&mut tx, node, &r.peer, Rows::Lock).await?;
    tx.commit().await.map_err(|_| AppError::Internal)?;
    Ok(draft::Envelope {
        version: 1,
        capability: r.capability.clone(),
        peer: r.peer.clone(),
        exchange_id: Uuid::new_v4(),
        cursor: r.cursor,
        effects: vec![],
    })
}
fn require_mode(ledger: &Ledger, capability: &aq_session_protocol::Text) -> Result<()> {
    if ledger.input_mode() == draft::InputMode::EpochV1
        && capability.as_str() != aq_terminal_relay::epoch::CAPABILITY
    {
        return Err(AppError::Conflict(
            "terminal requires exact epoch relay capability".into(),
        ));
    }
    Ok(())
}

fn remote_boot(peer: &Peer) -> Result<Uuid> {
    match peer {
        Peer::Remote { boot_id, .. } => Ok(*boot_id),
        Peer::Central { .. } => Err(AppError::Conflict(
            "central terminal authority unavailable".into(),
        )),
    }
}
