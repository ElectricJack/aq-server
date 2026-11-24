use super::*;
use aq_terminal_relay::Access;
pub async fn mutate(state: &AppState, actor: Uuid, request: &Mutation) -> Result<MutationReply> {
    let state = state.terminal_view();
    let config = configured(&state)?;
    if matches!(request, Mutation::EpochInput(_) | Mutation::OpenEpoch(_)) && !config.epoch_input {
        return Err(AppError::IdentityUnavailable(
            "epoch input is not enabled".into(),
        ));
    }
    let (t, id, kind, right, active) = request.binding();
    t.validate()
        .map_err(|_| AppError::BadRequest("invalid terminal identity".into()))?;
    let mutation = Request {
        state: &state,
        config: &config,
        actor,
        request,
        terminal: t,
        id,
        right,
        active,
        kind,
        digest: digest(request)?,
    };
    for _ in 0..3 {
        if let Some(reply) = mutation.apply().await? {
            return Ok(reply);
        }
    }
    Err(AppError::Conflict(
        "terminal recovery changed; retry inspection".into(),
    ))
}

struct Request<'a> {
    state: &'a AppState,
    config: &'a super::super::config::Loaded,
    actor: Uuid,
    request: &'a Mutation,
    terminal: &'a TerminalIdentity,
    id: Uuid,
    right: Right,
    active: bool,
    kind: &'static str,
    digest: String,
}
impl Request<'_> {
    async fn apply(&self) -> Result<Option<MutationReply>> {
        let mut tx = persistence::begin(self.state).await?;
        let (loaded, access) = self.load(&mut tx).await?;
        if let Some((plan, recovering)) = self.maintenance(&loaded)? {
            let mut journal = SqlJournal {
                state: self.state,
                tx: Some(tx),
                operation: None,
                peer_contact: false,
                recovering,
            };
            plan.commit_async(&mut journal).await.map_err(relay)?;
            return Ok(None);
        }
        if let Some(reply) = self.replay(&mut tx, &loaded, &access).await? {
            tx.commit().await.map_err(|_| AppError::Internal)?;
            return Ok(Some(reply));
        }
        self.commit(tx, loaded, access).await.map(Some)
    }
    async fn load(&self, tx: &mut Tx) -> Result<(LoadedLedger, Access)> {
        let historical = authority::load_locked(
            self.state,
            tx,
            &self.terminal.session,
            Policy::TerminalHistorical,
        )
        .await?;
        let mut access = AccessSubject::new(&historical, self.terminal, self.actor)
            .authorize(self.state, tx, self.right)
            .await?;
        let view = if self.active {
            authority::load_locked(
                self.state,
                tx,
                &self.terminal.session,
                Policy::TerminalActive,
            )
            .await?
        } else {
            historical
        };
        access.execution_active = self.active;
        access.execution_expires_at_ms = view.grant_expires_at_ms;
        let loaded = load(self.state, tx, self.terminal, &view).await?;
        access.session_revision = loaded.authority_revision;
        if self.active && view.authority_revision != loaded.authority_revision {
            return Err(AppError::Conflict(
                "terminal authority requires explicit registration reconciliation".into(),
            ));
        }
        Ok((loaded, access))
    }
    fn maintenance(&self, loaded: &LoadedLedger) -> Result<Option<(Plan, bool)>> {
        if loaded.runtime_id != self.config.runtime_id {
            let plan = Ledger::recover(&loaded.ledger.encode().map_err(relay)?).map_err(relay)?;
            return Ok(Some((plan, true)));
        }
        let expiry = loaded.ledger.expire(now(self.state)).map_err(relay)?;
        Ok(
            (expiry.preview().ledger_revision != loaded.ledger.revision())
                .then_some((expiry, false)),
        )
    }
    async fn replay(
        &self,
        tx: &mut Tx,
        loaded: &LoadedLedger,
        access: &Access,
    ) -> Result<Option<MutationReply>> {
        if matches!(self.request, Mutation::EpochInput(_)) {
            return Ok(None);
        }
        let key = RequestKey {
            id: self.id,
            principal: self.actor,
            digest: &self.digest,
        };
        let replay = persistence::replay(tx, self.terminal.terminal_id, key).await?;
        match replay {
            Some(MutationReply::Input(receipt)) => Ok(Some(MutationReply::Input(
                loaded
                    .ledger
                    .receipt(access, receipt.operation_id, now(self.state))
                    .map_err(relay)?,
            ))),
            other => Ok(other),
        }
    }
    async fn commit(&self, tx: Tx, loaded: LoadedLedger, access: Access) -> Result<MutationReply> {
        let (plan, reply) = self
            .request
            .plan(&loaded.ledger, &access, now(self.state))?;
        let expiry = match self.request {
            Mutation::Watch(r) => Some(r.expires_at_ms),
            Mutation::Capture(r) => Some(r.expires_at_ms),
            _ => None,
        };
        if let Some(until) = expiry {
            self.config
                .runtime
                .reserve(&loaded.ledger, until, now(self.state))?;
        }
        let operation = if matches!(self.request, Mutation::EpochInput(_)) {
            None
        } else {
            Some(Operation {
                id: self.id,
                principal: self.actor,
                digest: self.digest.clone(),
                kind: self.kind,
                response: reply.clone(),
            })
        };
        let mut journal = SqlJournal {
            state: self.state,
            tx: Some(tx),
            operation,
            peer_contact: false,
            recovering: false,
        };
        let committed = plan.commit_async(&mut journal).await.map_err(relay)?;
        if let SessionIdentity::Remote { node_id, .. } = self.terminal.session {
            self.config.runtime.wake(node_id);
        }
        if let MutationReply::Input(_) = reply {
            let operation = match self.request {
                Mutation::Resolve(r) => r.uncertain_operation_id,
                _ => self.id,
            };
            return Ok(MutationReply::Input(
                committed
                    .ledger
                    .receipt(&access, operation, now(self.state))
                    .map_err(relay)?,
            ));
        }
        Ok(reply)
    }
}
