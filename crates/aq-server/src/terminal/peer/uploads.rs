use super::*;
use aq_session_protocol::OutputGap;
use aq_terminal_relay::{Plan, epoch::Retirement};
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
    let has_output = !(r.frames.is_empty() && r.snapshots.is_empty() && r.gaps.is_empty());
    if has_output {
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
    let terminals = terminal_set(r, node)?;
    // Authenticate even empty uploads; no capability/boot bypass through an empty batch.
    let mut check = super::persistence::begin(state).await?;
    current_peer(state, &mut check, node, &r.peer).await?;
    check.commit().await.map_err(|_| AppError::Internal)?;
    let upload = Upload {
        state,
        node,
        request: r,
        config: &config,
    };
    let mut retirements = Vec::new();
    for terminal in terminals.values() {
        retirements.extend(upload.apply(terminal).await?);
    }
    let dropped_output = upload.output()?;
    for terminal in terminals.values() {
        upload.observe_output(terminal).await?;
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

fn terminal_set(
    r: &PeerUpload,
    node: &AuthenticatedNode,
) -> Result<BTreeMap<Uuid, TerminalIdentity>> {
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
    Ok(terminals)
}
struct Upload<'a> {
    state: &'a AppState,
    node: &'a AuthenticatedNode,
    request: &'a PeerUpload,
    config: &'a crate::terminal::config::Loaded,
}
impl Upload<'_> {
    async fn apply(&self, t: &TerminalIdentity) -> Result<Option<Retirement>> {
        let (state, node, r, config) = (self.state, self.node, self.request, self.config);
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
        let plan = if recovering {
            Ledger::recover(&loaded.ledger.encode().map_err(relay)?)
        } else {
            loaded.ledger.expire(clock)
        }
        .map_err(relay)?;
        let batch = Batch {
            request: r,
            terminal: t,
        };
        let plan = batch.controls(&mut tx, plan).await?;
        let plan = batch.captures(&mut tx, plan, clock).await?;
        let retirement = plan.preview().epoch_retirement;
        let mut journal = SqlJournal {
            state,
            tx: Some(tx),
            operation: None,
            peer_contact: true,
            recovering,
        };
        plan.commit_async(&mut journal).await.map_err(relay)?;
        Ok(retirement)
    }
    fn output(&self) -> Result<Vec<OutputGap>> {
        let (state, r, config) = (self.state, self.request, self.config);
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
        Ok(dropped_output)
    }
    async fn observe_output(&self, t: &TerminalIdentity) -> Result<()> {
        let (state, node, r) = (self.state, self.node, self.request);
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
        Ok(())
    }
}
struct Batch<'a> {
    request: &'a PeerUpload,
    terminal: &'a TerminalIdentity,
}
impl Batch<'_> {
    async fn controls(&self, tx: &mut Tx, mut plan: Plan) -> Result<Plan> {
        let (r, t) = (self.request, self.terminal);
        for ack in r.fence_acks.iter().filter(|x| x.terminal == *t) {
            plan = plan
                .and_then(|l| l.acknowledge_fence(&r.peer, ack))
                .map_err(relay)?;
            sqlx::query("UPDATE terminal_outbox SET acknowledged=TRUE WHERE terminal_id=$1 AND operation_id=$2 AND kind='fence'").bind(t.terminal_id).bind(ack.fence_id).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
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
            sqlx::query("UPDATE terminal_outbox SET acknowledged=TRUE WHERE terminal_id=$1 AND operation_id=$2 AND kind='resize'").bind(t.terminal_id).bind(receipt.operation_id).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
        }
        Ok(plan)
    }
    async fn captures(&self, tx: &mut Tx, mut plan: Plan, clock: u64) -> Result<Plan> {
        let (r, t) = (self.request, self.terminal);
        for snapshot in r.snapshots.iter().filter(|x| x.terminal == *t) {
            let capture:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM terminal_outbox WHERE terminal_id=$1 AND operation_id=$2 AND kind='capture')").bind(t.terminal_id).bind(snapshot.capture_request_id).fetch_one(&mut **tx).await.map_err(|_|AppError::Internal)?;
            if capture {
                plan = plan
                    .and_then(|l| l.observe_capture(&r.peer, snapshot, clock))
                    .map_err(relay)?;
                sqlx::query("UPDATE terminal_outbox SET acknowledged=TRUE WHERE terminal_id=$1 AND operation_id=$2 AND kind='capture'").bind(t.terminal_id).bind(snapshot.capture_request_id).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
            }
        }
        Ok(plan)
    }
}
