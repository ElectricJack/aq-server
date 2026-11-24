use super::*;
use sqlx::{Row, postgres::PgRow};
use std::collections::BTreeSet;
pub(super) async fn poll_once(
    state: &AppState,
    node: &AuthenticatedNode,
    r: &draft::RelayPoll,
    mode: PollMode,
) -> Result<draft::Envelope> {
    let config = configured(state)?;
    let poll = Poll {
        state,
        node,
        request: r,
        mode,
        config: &config,
    };
    let (cursor, grouped) = poll.candidates().await?;
    let mut effects = Vec::new();
    for (terminal, ids) in grouped.values() {
        let batch = Delivery {
            poll: &poll,
            terminal,
        };
        match batch.effects(ids).await? {
            Some(authorized) => effects.extend(authorized),
            None => return poll_once_after_recovery(state, node, r).await,
        }
    }
    if mode == PollMode::Full && effects.len() < 64 {
        effects.extend(dispatch_next(state, node, r).await?);
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

struct Poll<'a> {
    state: &'a AppState,
    node: &'a AuthenticatedNode,
    request: &'a draft::RelayPoll,
    mode: PollMode,
    config: &'a crate::terminal::config::Loaded,
}
type Candidates = BTreeMap<Uuid, (TerminalIdentity, Vec<i64>)>;
impl Poll<'_> {
    async fn candidates(&self) -> Result<(u64, Candidates)> {
        let (state, node, r, mode) = (self.state, self.node, self.request, self.mode);
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
        Ok((cursor, grouped))
    }
}
struct Delivery<'a> {
    poll: &'a Poll<'a>,
    terminal: &'a TerminalIdentity,
}
type Decoded = Vec<(draft::Effect, Option<Uuid>)>;
impl Delivery<'_> {
    async fn effects(&self, ids: &[i64]) -> Result<Option<Vec<draft::Effect>>> {
        let Poll {
            state,
            node,
            request: r,
            mode,
            config,
        } = *self.poll;
        let t = self.terminal;
        crate::delegated_permits::runtime::require_session(node, &t.session)?;
        let mut tx = super::persistence::begin(state).await?;
        current_peer(state, &mut tx, node, &r.peer).await?;
        let view =
            authority::load_locked(state, &mut tx, &t.session, Policy::TerminalHistorical).await?;
        let active = active_locked(state, &mut tx, t).await?;
        let decoded = self.decode(&mut tx, ids).await?;
        let mut scopes_by_actor = self.scopes(&mut tx, &view, &decoded).await?;
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
            return Ok(None);
        }
        let current = active && view.authority_revision == loaded.authority_revision;
        for scopes in scopes_by_actor.values_mut() {
            scopes.set_authority(loaded.authority_revision, current);
        }
        let effects = self.authorized(decoded, &scopes_by_actor, &loaded.ledger)?;
        tx.commit().await.map_err(|_| AppError::Internal)?;
        Ok(Some(effects))
    }
    async fn decode(&self, tx: &mut Tx, ids: &[i64]) -> Result<Decoded> {
        let (state, t) = (self.poll.state, self.terminal);
        // These are immutable ciphertext candidates read under common node/attempt
        // locks, not authority. Decode first so all users/ACLs lock BEFORE ledger.
        let out=sqlx::query("SELECT operation_id,ledger_revision,ordinal,protection_version,key_id,nonce,ciphertext FROM terminal_outbox WHERE terminal_id=$1 AND outbox_id=ANY($2) AND NOT claimed AND NOT acknowledged AND(expires_at_ms IS NULL OR expires_at_ms>$3) ORDER BY outbox_id").bind(t.terminal_id).bind(ids).bind(dbtime(now(state))?).fetch_all(&mut **tx).await.map_err(|_|AppError::Internal)?;
        let hint: Option<Uuid> = sqlx::query_scalar(
            "SELECT controller_principal FROM terminal_sessions WHERE terminal_id=$1",
        )
        .bind(t.terminal_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(|_| AppError::Internal)?;
        out.into_iter().map(|row| self.open(row, hint)).collect()
    }
    fn open(&self, row: PgRow, hint: Option<Uuid>) -> Result<(draft::Effect, Option<Uuid>)> {
        let sealed = Sealed {
            version: row.get::<i32, _>(3) as u32,
            key_id: row.get(4),
            nonce: row.get(5),
            ciphertext: row.get(6),
        };
        let bytes = self
            .poll
            .config
            .keyring
            .open(
                &Context {
                    terminal: self.terminal.clone(),
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
        self.require_effect(&effect)?;
        let actor = effect_actor(&effect, hint)?;
        Ok((effect, actor))
    }
    fn require_effect(&self, effect: &draft::Effect) -> Result<()> {
        if self.poll.mode == PollMode::Output && !matches!(effect, draft::Effect::Watch(_)) {
            return Err(AppError::Internal);
        }
        let same_terminal =
            effect.terminal() == self.terminal && self.poll.request.peer.matches(self.terminal);
        if !same_terminal
            || matches!(
                effect,
                draft::Effect::Input(_) | draft::Effect::EpochInput(_)
            )
        {
            return Err(AppError::Internal);
        }
        Ok(())
    }
    async fn scopes(
        &self,
        tx: &mut Tx,
        view: &authority::AuthorityView,
        decoded: &Decoded,
    ) -> Result<BTreeMap<Uuid, Scopes>> {
        let actors: BTreeSet<_> = decoded.iter().filter_map(|(_, actor)| *actor).collect();
        let mut scopes_by_actor = BTreeMap::new();
        for actor in actors {
            match AccessSubject::new(view, self.terminal, actor)
                .scopes(self.poll.state, tx)
                .await
            {
                Ok(scopes) => {
                    scopes_by_actor.insert(actor, scopes);
                }
                Err(AppError::Forbidden(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(scopes_by_actor)
    }
    fn authorized(
        &self,
        decoded: Decoded,
        scopes_by_actor: &BTreeMap<Uuid, Scopes>,
        ledger: &Ledger,
    ) -> Result<Vec<draft::Effect>> {
        let mut effects = Vec::new();
        for (effect, actor) in decoded {
            let scopes = actor.and_then(|id| scopes_by_actor.get(&id));
            let access = scopes.and_then(|s| match &effect {
                draft::Effect::Watch(_) => s.inspect.as_ref(),
                draft::Effect::Capture(_) => s.both.as_ref(),
                _ => s.control.as_ref(),
            });
            match ledger.authorize_replayable_effect(access, &effect, now(self.poll.state)) {
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
        Ok(effects)
    }
}
fn effect_actor(effect: &draft::Effect, hint: Option<Uuid>) -> Result<Option<Uuid>> {
    let actor = match effect {
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
    Ok(actor)
}

impl Scopes {
    fn set_authority(&mut self, revision: u64, active: bool) {
        for access in [&mut self.inspect, &mut self.control, &mut self.both]
            .into_iter()
            .flatten()
        {
            access.session_revision = revision;
            access.execution_active = active;
        }
    }
}
