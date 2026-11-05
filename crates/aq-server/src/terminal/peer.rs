//! Node-outbound relay. Correlation IDs do not authorize effects; each enclosed
//! receipt/fence/capture is checked against the encrypted canonical ledger.
use super::{
    persistence::*,
    protection::{Context, Purpose, Sealed},
};
pub use crate::worker_api::AuthenticatedNode;
use crate::{
    AppState,
    error::AppError,
    execution::authority::{self, Policy},
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
pub(super) async fn current_peer(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    node: &AuthenticatedNode,
    peer: &Peer,
) -> Result<()> {
    require_owner(state, tx).await?;
    let Peer::Remote { node_id, boot_id } = peer else {
        return Err(AppError::Conflict(
            "central terminal authority unavailable".into(),
        ));
    };
    if node.id() != *node_id {
        return Err(AppError::Forbidden("terminal peer mismatch".into()));
    }
    crate::worker_api::lock_current_node(tx, node).await?;
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
pub async fn upload(
    state: &AppState,
    node: &AuthenticatedNode,
    r: &PeerUpload,
) -> Result<UploadAck> {
    upload_in(&state.terminal_read_view(), node, r).await
}
pub async fn control_upload(
    state: &AppState,
    node: &AuthenticatedNode,
    r: &PeerUpload,
) -> Result<UploadAck> {
    if !r.frames.is_empty() || !r.snapshots.is_empty() || !r.gaps.is_empty() {
        return Err(AppError::BadRequest(
            "control upload must be metadata-only".into(),
        ));
    }
    upload_in(&state.terminal_view(), node, r).await
}
async fn upload_in(
    state: &AppState,
    node: &AuthenticatedNode,
    r: &PeerUpload,
) -> Result<UploadAck> {
    r.validate()
        .map_err(|_| AppError::BadRequest("invalid terminal upload".into()))?;
    let config = configured(state)?;
    let mut terminals = BTreeMap::<Uuid, TerminalIdentity>::new();
    for t in r
        .receipts
        .iter()
        .map(|x| &x.terminal)
        .chain(r.epoch_receipts.iter().map(|x| &x.receipt.terminal))
        .chain(r.resize_receipts.iter().map(|x| &x.terminal))
        .chain(r.fence_acks.iter().map(|x| &x.terminal))
        .chain(r.frames.iter().map(|x| &x.terminal))
        .chain(r.snapshots.iter().map(|x| &x.terminal))
        .chain(r.gaps.iter().map(|x| &x.terminal))
    {
        crate::delegated_permits::runtime::require_session(node, &t.session)?;
        if let Some(old) = terminals.insert(t.terminal_id, t.clone())
            && old != *t
        {
            return Err(AppError::Conflict("mixed terminal epoch".into()));
        }
    }
    if terminals.len() > 32 {
        return Err(AppError::BadRequest("terminal upload scope bound".into()));
    }
    // Authenticate even empty uploads; no capability/boot bypass through an empty batch.
    let mut check = super::persistence::begin(state).await?;
    current_peer(state, &mut check, node, &r.peer).await?;
    check.commit().await.map_err(|_| AppError::Internal)?;
    let mut retirements = vec![];
    for t in terminals.values() {
        let mut tx = super::persistence::begin(state).await?;
        let view =
            authority::load_locked(state, &mut tx, &t.session, Policy::TerminalHistorical).await?;
        current_peer(state, &mut tx, node, &r.peer).await?;
        let loaded = load(state, &mut tx, t, &view).await?;
        require_mode(&loaded.ledger, &r.capability)?;
        if loaded.ledger.input_mode() == draft::InputMode::EpochV1
            && r.receipts.iter().any(|x| x.terminal == *t)
        {
            return Err(AppError::Conflict(
                "epoch input requires epoch receipt envelope".into(),
            ));
        }
        let clock = now(state);
        let recovering = loaded.runtime_id != config.runtime_id;
        let mut plan = if recovering {
            Ledger::recover(&loaded.ledger.encode().map_err(relay)?)
        } else {
            loaded.ledger.expire(clock)
        }
        .map_err(relay)?;
        for ack in r.fence_acks.iter().filter(|x| x.terminal == *t) {
            plan = plan
                .and_then(|l| l.acknowledge_fence(&r.peer, ack))
                .map_err(relay)?;
            sqlx::query("UPDATE terminal_outbox SET acknowledged=TRUE WHERE terminal_id=$1 AND operation_id=$2 AND kind='fence'").bind(t.terminal_id).bind(ack.fence_id).execute(&mut *tx).await.map_err(|_|AppError::Internal)?;
        }
        for receipt in r.receipts.iter().filter(|x| x.terminal == *t) {
            plan = plan
                .and_then(|l| l.observe_input(&r.peer, receipt))
                .map_err(relay)?;
        }
        for receipt in r.epoch_receipts.iter().filter(|x| x.receipt.terminal == *t) {
            plan = plan
                .and_then(|l| l.observe_epoch_input(&r.peer, receipt))
                .map_err(relay)?;
        }
        for receipt in r.resize_receipts.iter().filter(|x| x.terminal == *t) {
            plan = plan
                .and_then(|l| l.observe_resize(&r.peer, receipt))
                .map_err(relay)?;
            sqlx::query("UPDATE terminal_outbox SET acknowledged=TRUE WHERE terminal_id=$1 AND operation_id=$2 AND kind='resize'").bind(t.terminal_id).bind(receipt.operation_id).execute(&mut *tx).await.map_err(|_|AppError::Internal)?;
        }
        for snapshot in r.snapshots.iter().filter(|x| x.terminal == *t) {
            let capture:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM terminal_outbox WHERE terminal_id=$1 AND operation_id=$2 AND kind='capture')").bind(t.terminal_id).bind(snapshot.capture_request_id).fetch_one(&mut *tx).await.map_err(|_|AppError::Internal)?;
            if capture {
                plan = plan
                    .and_then(|l| l.observe_capture(&r.peer, snapshot, clock))
                    .map_err(relay)?;
                sqlx::query("UPDATE terminal_outbox SET acknowledged=TRUE WHERE terminal_id=$1 AND operation_id=$2 AND kind='capture'").bind(t.terminal_id).bind(snapshot.capture_request_id).execute(&mut *tx).await.map_err(|_|AppError::Internal)?;
            }
        }
        let summary = plan.preview();
        if let Some(retirement) = summary.epoch_retirement {
            retirements.push(retirement);
        }
        let mut journal = SqlJournal {
            state,
            tx: Some(tx),
            operation: None,
            peer_contact: true,
            recovering,
        };
        plan.commit_async(&mut journal).await.map_err(relay)?;
    }
    // Disposable output is never authority or proof of input. Its read path repeats
    // current inspection ACL checks; absence after restart requires a fresh snapshot.
    for gap in &r.gaps {
        config.runtime.gap(&r.peer, gap, now(state))?;
    }
    for snapshot in &r.snapshots {
        config
            .runtime
            .snapshot(&r.peer, snapshot.clone(), now(state))?;
    }
    let mut dropped_output = vec![];
    for frame in &r.frames {
        let dropped = config.runtime.frame(&r.peer, frame.clone(), now(state))?;
        if let Some(gap) = dropped {
            dropped_output.push(gap);
        }
    }
    for t in terminals.values() {
        let last = r
            .frames
            .iter()
            .filter(|f| f.terminal == *t)
            .map(|f| f.sequence)
            .chain(
                r.snapshots
                    .iter()
                    .filter(|s| s.terminal == *t)
                    .map(|s| s.through_sequence),
            )
            .max();
        if let Some(last) = last {
            let mut tx = super::persistence::begin(state).await?;
            authority::load_locked(state, &mut tx, &t.session, Policy::TerminalHistorical).await?;
            current_peer(state, &mut tx, node, &r.peer).await?;
            sqlx::query("UPDATE terminal_sessions SET last_output_at_ms=$2,last_output_sequence=greatest(last_output_sequence,$3) WHERE terminal_id=$1").bind(t.terminal_id).bind(dbtime(now(state))?).bind(dbtime(last)?).execute(&mut *tx).await.map_err(|_|AppError::Internal)?;
            tx.commit().await.map_err(|_| AppError::Internal)?;
        }
    }
    let ack = UploadAck {
        version: 1,
        capability: r.capability.clone(),
        peer: r.peer.clone(),
        exchange_id: r.exchange_id,
        cursor: r.cursor,
        retirements,
        dropped_output,
    };
    ack.validate().map_err(|_| AppError::Internal)?;
    Ok(ack)
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
        current_peer(&state, &mut tx, node, &r.peer).await?;
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
    let state = state.terminal_view();
    let config = configured(&state)?;
    let _slot = config
        .runtime
        .output_poll_channel(node.id(), node.permitted_attempt())?;
    poll_once(&state, node, r, PollMode::Output).await
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum PollMode {
    Full,
    Output,
}
async fn poll_once(
    state: &AppState,
    node: &AuthenticatedNode,
    r: &draft::RelayPoll,
    mode: PollMode,
) -> Result<draft::Envelope> {
    use sqlx::Row;
    let config = configured(state)?;
    let mut initial = super::persistence::begin(state).await?;
    current_peer(state, &mut initial, node, &r.peer).await?;
    // Filter before LIMIT and cursor calculation: unrelated control entries
    // cannot consume an output page or advance its transport watermark.
    let query = match mode {
        PollMode::Full => {
            "SELECT o.outbox_id,s.identity::text FROM terminal_outbox o JOIN terminal_sessions s ON s.terminal_id=o.terminal_id WHERE s.node_id=$1 AND s.boot_id=$4 AND ($5::uuid IS NULL OR s.attempt_id=$5) AND o.outbox_id>$2 AND o.kind NOT IN('input','epoch_input') AND NOT o.claimed AND NOT o.acknowledged AND(o.expires_at_ms IS NULL OR o.expires_at_ms>$3) ORDER BY o.outbox_id LIMIT 64"
        }
        PollMode::Output => {
            "SELECT o.outbox_id,s.identity::text FROM terminal_outbox o JOIN terminal_sessions s ON s.terminal_id=o.terminal_id WHERE s.node_id=$1 AND s.boot_id=$4 AND ($5::uuid IS NULL OR s.attempt_id=$5) AND o.outbox_id>$2 AND o.kind='watch' AND NOT o.claimed AND NOT o.acknowledged AND(o.expires_at_ms IS NULL OR o.expires_at_ms>$3) ORDER BY o.outbox_id LIMIT 64"
        }
    };
    let rows = sqlx::query(query)
        .bind(node.id())
        .bind(dbtime(r.cursor)?)
        .bind(dbtime(now(state))?)
        .bind(remote_boot(&r.peer)?)
        .bind(node.permitted_attempt())
        .fetch_all(&mut *initial)
        .await
        .map_err(|_| AppError::Internal)?;
    let mut grouped = BTreeMap::<Uuid, (TerminalIdentity, Vec<i64>)>::new();
    let mut cursor = r.cursor;
    for row in rows {
        let id: i64 = row.get(0);
        let t: TerminalIdentity =
            serde_json::from_str(row.get::<&str, _>(1)).map_err(|_| AppError::Internal)?;
        cursor = cursor.max(id as u64);
        grouped
            .entry(t.terminal_id)
            .or_insert_with(|| (t, vec![]))
            .1
            .push(id);
    }
    initial.commit().await.map_err(|_| AppError::Internal)?;
    let mut effects = vec![];
    for (t, ids) in grouped.values() {
        crate::delegated_permits::runtime::require_session(node, &t.session)?;
        let mut tx = super::persistence::begin(state).await?;
        current_peer(state, &mut tx, node, &r.peer).await?;
        let view =
            authority::load_locked(state, &mut tx, &t.session, Policy::TerminalHistorical).await?;
        let active = match authority::load_locked(
            state,
            &mut tx,
            &t.session,
            Policy::TerminalActive,
        )
        .await
        {
            Ok(_) => true,
            Err(
                authority::AuthorityError::Database(_) | authority::AuthorityError::InvalidState,
            ) => return Err(AppError::Internal),
            Err(_) => false,
        };
        // These are immutable ciphertext candidates read under common node/attempt
        // locks, not authority. Decode first so all users/ACLs lock BEFORE ledger.
        let out=sqlx::query("SELECT operation_id,ledger_revision,ordinal,protection_version,key_id,nonce,ciphertext FROM terminal_outbox WHERE terminal_id=$1 AND outbox_id=ANY($2) AND NOT claimed AND NOT acknowledged AND(expires_at_ms IS NULL OR expires_at_ms>$3) ORDER BY outbox_id").bind(t.terminal_id).bind(ids).bind(dbtime(now(state))?).fetch_all(&mut *tx).await.map_err(|_|AppError::Internal)?;
        let hint: Option<Uuid> = sqlx::query_scalar(
            "SELECT controller_principal FROM terminal_sessions WHERE terminal_id=$1",
        )
        .bind(t.terminal_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| AppError::Internal)?;
        let mut decoded = vec![];
        let mut actors = std::collections::BTreeSet::new();
        for row in out {
            let sealed = Sealed {
                version: row.get::<i32, _>(3) as u32,
                key_id: row.get(4),
                nonce: row.get(5),
                ciphertext: row.get(6),
            };
            let bytes = config
                .keyring
                .open(
                    &Context {
                        terminal: t.clone(),
                        ledger_revision: row.get::<i64, _>(1) as u64,
                        purpose: Purpose::Outbox {
                            operation_id: row.get(0),
                            ordinal: row.get::<i32, _>(2) as u32,
                        },
                    },
                    &sealed,
                )
                .map_err(|_| AppError::Internal)?;
            let effect: draft::Effect =
                serde_json::from_slice(&bytes).map_err(|_| AppError::Internal)?;
            effect.validate().map_err(|_| AppError::Internal)?;
            if mode == PollMode::Output && !matches!(effect, draft::Effect::Watch(_)) {
                return Err(AppError::Internal);
            }
            if effect.terminal() != t
                || !r.peer.matches(t)
                || matches!(
                    effect,
                    draft::Effect::Input(_) | draft::Effect::EpochInput(_)
                )
            {
                return Err(AppError::Internal);
            }
            let actor = match &effect {
                draft::Effect::Fence(f) => f.controller.as_ref().map(|c| c.principal_id.as_str()),
                draft::Effect::Resize(v) => Some(v.lease.principal_id.as_str()),
                draft::Effect::Watch(w) => Some(w.principal_id.as_str()),
                _ => None,
            }
            .map(Uuid::parse_str)
            .transpose()
            .map_err(|_| AppError::Internal)?;
            let actor = if matches!(effect, draft::Effect::Capture(_)) {
                hint
            } else {
                actor
            };
            if let Some(actor) = actor {
                actors.insert(actor);
            }
            decoded.push((effect, actor));
        }
        let mut scopes_by_actor = BTreeMap::new();
        for actor in actors {
            match scopes(state, &mut tx, &view, t, actor).await {
                Ok(scopes) => {
                    scopes_by_actor.insert(actor, scopes);
                }
                Err(AppError::Forbidden(_)) => {}
                Err(e) => return Err(e),
            }
        }
        let loaded = load(state, &mut tx, t, &view).await?;
        require_mode(&loaded.ledger, &r.capability)?;
        if loaded.runtime_id != config.runtime_id {
            if mode == PollMode::Output {
                return Err(AppError::IdentityUnavailable(
                    "terminal output subscription requires current runtime".into(),
                ));
            }
            let mut journal = SqlJournal {
                state,
                tx: Some(tx),
                operation: None,
                peer_contact: false,
                recovering: true,
            };
            Ledger::recover(&loaded.ledger.encode().map_err(relay)?)
                .map_err(relay)?
                .commit_async(&mut journal)
                .await
                .map_err(relay)?;
            return poll_once_after_recovery(state, node, r).await;
        }
        for scopes in scopes_by_actor.values_mut() {
            for a in [&mut scopes.inspect, &mut scopes.control, &mut scopes.both]
                .into_iter()
                .flatten()
            {
                a.session_revision = loaded.authority_revision;
                a.execution_active = active && view.authority_revision == loaded.authority_revision;
            }
        }
        for (effect, actor) in decoded {
            let scopes = actor.and_then(|id| scopes_by_actor.get(&id));
            let access = scopes.and_then(|s| match &effect {
                draft::Effect::Watch(_) => s.inspect.as_ref(),
                draft::Effect::Capture(_) => s.both.as_ref(),
                _ => s.control.as_ref(),
            });
            match loaded
                .ledger
                .authorize_replayable_effect(access, &effect, now(state))
            {
                Ok(()) => effects.push(effect),
                Err(
                    aq_terminal_relay::Error::Forbidden
                    | aq_terminal_relay::Error::Stale
                    | aq_terminal_relay::Error::Expired
                    | aq_terminal_relay::Error::PendingFence,
                ) => {}
                Err(_) => return Err(AppError::Internal),
            }
        }
        tx.commit().await.map_err(|_| AppError::Internal)?;
    }
    // Dispatch at most one canonical FIFO input per poll, with fresh current
    // role/ACL and TerminalActive checks. Projection rows are only candidate hints.
    if mode == PollMode::Full
        && effects.len() < 64
        && let Some(effect) = dispatch_next(state, node, r).await?
    {
        effects.push(effect);
    }
    let response = draft::Envelope {
        version: 1,
        capability: r.capability.clone(),
        peer: r.peer.clone(),
        exchange_id: Uuid::new_v4(),
        cursor,
        effects,
    };
    response.validate().map_err(|_| AppError::Internal)?;
    Ok(response)
}
// Avoid recursive async futures and bound recovery work to one retry. Other
// sessions recover on later polls; no input is dispatched from recovered bytes.
async fn poll_once_after_recovery(
    state: &AppState,
    node: &AuthenticatedNode,
    r: &draft::RelayPoll,
) -> Result<draft::Envelope> {
    let mut tx = super::persistence::begin(state).await?;
    current_peer(state, &mut tx, node, &r.peer).await?;
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
async fn dispatch_next(
    state: &AppState,
    node: &AuthenticatedNode,
    r: &draft::RelayPoll,
) -> Result<Option<draft::Effect>> {
    let rows=sqlx::query("SELECT identity::text,operation_id,principal_id FROM (SELECT DISTINCT ON(s.terminal_id) s.identity,s.terminal_id,i.operation_id,i.principal_id,i.updated_at_ms,i.admission_revision FROM terminal_input_receipts i JOIN terminal_sessions s ON s.terminal_id=i.terminal_id WHERE s.node_id=$1 AND s.boot_id=$3 AND ($4::uuid IS NULL OR s.attempt_id=$4) AND ((NOT i.dispatched AND i.disposition='queued' AND NOT EXISTS(SELECT 1 FROM terminal_input_receipts h WHERE h.terminal_id=s.terminal_id AND h.dispatched AND h.disposition IN('queued','received','injecting') AND h.deadline_ms>$2)) OR(i.dispatched AND i.disposition IN('queued','received','injecting') AND i.deadline_ms<=$2)) ORDER BY s.terminal_id,i.updated_at_ms,i.admission_revision) candidates ORDER BY updated_at_ms,terminal_id LIMIT 8").bind(node.id()).bind(dbtime(now(state))?).bind(remote_boot(&r.peer)?).bind(node.permitted_attempt()).fetch_all(&state.store.pool).await.map_err(|_|AppError::Internal)?;
    for row in rows {
        if let Some(effect) = dispatch_candidate(state, node, r, row).await? {
            return Ok(Some(effect));
        }
    }
    Ok(None)
}
async fn dispatch_candidate(
    state: &AppState,
    node: &AuthenticatedNode,
    r: &draft::RelayPoll,
    row: sqlx::postgres::PgRow,
) -> Result<Option<draft::Effect>> {
    use sqlx::Row;
    let t: TerminalIdentity =
        serde_json::from_str(row.get::<&str, _>(0)).map_err(|_| AppError::Internal)?;
    crate::delegated_permits::runtime::require_session(node, &t.session)?;
    let id: Uuid = row.get(1);
    let principal: Uuid = row.get(2);
    let mut tx = super::persistence::begin(state).await?;
    current_peer(state, &mut tx, node, &r.peer).await?;
    let view =
        authority::load_locked(state, &mut tx, &t.session, Policy::TerminalHistorical).await?;
    let active =
        match authority::load_locked(state, &mut tx, &t.session, Policy::TerminalActive).await {
            Ok(_) => true,
            Err(
                authority::AuthorityError::Database(_) | authority::AuthorityError::InvalidState,
            ) => return Err(AppError::Internal),
            Err(_) => false,
        };
    let access = match authorize(state, &mut tx, &view, &t, principal, Right::Control).await {
        Ok(a) => Some(a),
        Err(AppError::Forbidden(_)) => None,
        Err(e) => return Err(e),
    };
    let loaded = load(state, &mut tx, &t, &view).await?;
    let config = configured(state)?;
    if loaded.runtime_id != config.runtime_id {
        let mut journal = SqlJournal {
            state,
            tx: Some(tx),
            operation: None,
            peer_contact: false,
            recovering: true,
        };
        Ledger::recover(&loaded.ledger.encode().map_err(relay)?)
            .map_err(relay)?
            .commit_async(&mut journal)
            .await
            .map_err(relay)?;
        return Ok(None);
    }
    let metadata = loaded.ledger.input_metadata();
    let Some(exact) = metadata.iter().find(|v| v.operation_id == id) else {
        return Ok(None);
    };
    if exact.principal.as_str() != principal.to_string() {
        return Err(AppError::Internal);
    }
    let clock = now(state);
    if clock >= exact.deadline_ms {
        let mut journal = SqlJournal {
            state,
            tx: Some(tx),
            operation: None,
            peer_contact: false,
            recovering: false,
        };
        loaded
            .ledger
            .expire(clock)
            .map_err(relay)?
            .commit_async(&mut journal)
            .await
            .map_err(relay)?;
        return Ok(None);
    }
    if !active
        || view.authority_revision != loaded.authority_revision
        || access
            .as_ref()
            .is_none_or(|a| clock >= a.valid_until_ms || clock >= a.execution_expires_at_ms)
    {
        let mut journal = SqlJournal {
            state,
            tx: Some(tx),
            operation: None,
            peer_contact: false,
            recovering: false,
        };
        loaded
            .ledger
            .reject_queued_for(&exact.principal)
            .map_err(relay)?
            .commit_async(&mut journal)
            .await
            .map_err(relay)?;
        return Ok(None);
    }
    let mut access = access.ok_or(AppError::Internal)?;
    access.execution_active = true;
    access.session_revision = loaded.authority_revision;
    require_mode(&loaded.ledger, &r.capability)?;
    let plan = match loaded.ledger.input_mode() {
        draft::InputMode::FiniteV1 => loaded.ledger.dispatch_input(&access, id, clock),
        draft::InputMode::EpochV1 => loaded.ledger.dispatch_epoch_input(&access, id, clock),
    };
    let plan = match plan {
        Ok(p) => p,
        Err(
            aq_terminal_relay::Error::Capacity
            | aq_terminal_relay::Error::Unknown
            | aq_terminal_relay::Error::PendingFence
            | aq_terminal_relay::Error::Transition
            | aq_terminal_relay::Error::Stale
            | aq_terminal_relay::Error::Expired,
        ) => return Ok(None),
        Err(e) => return Err(relay(e)),
    };
    let mut journal = SqlJournal {
        state,
        tx: Some(tx),
        operation: None,
        peer_contact: false,
        recovering: false,
    };
    Ok(plan
        .commit_async(&mut journal)
        .await
        .map_err(relay)?
        .effects
        .into_iter()
        .next())
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
