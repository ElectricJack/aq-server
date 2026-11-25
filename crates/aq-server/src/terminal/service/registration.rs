use super::*;
pub async fn register(
    state: &AppState,
    node: &super::peer::AuthenticatedNode,
    r: &draft::Register,
) -> Result<TerminalInfo> {
    crate::delegated_permits::runtime::require_session(node, &r.terminal.session)?;
    r.validate()
        .map_err(|_| AppError::BadRequest("invalid terminal registration/capability".into()))?;
    let draft::Peer::Remote {
        node_id,
        boot_id: _,
    } = r.peer
    else {
        return Err(AppError::Conflict(
            "central terminal authority unsupported".into(),
        ));
    };
    if node.id() != node_id {
        return Err(AppError::Forbidden("terminal peer mismatch".into()));
    }
    let state = state.terminal_view();
    let config = configured(&state)?;
    if r.input_mode == InputMode::EpochV1 && !config.epoch_input {
        return Err(AppError::Conflict("epoch input is not enabled".into()));
    }
    let mut tx = super::persistence::begin(&state).await?;
    super::peer::peer_rows(&mut tx, node, &r.peer, authority::Rows::Snapshot).await?;
    let view = authority::load_locked(&state, &mut tx, &r.terminal.session, Policy::TerminalActive)
        .await?;
    if view.authority_revision != r.expected_authority_revision {
        return Err(AppError::Conflict(
            "terminal authority revision changed".into(),
        ));
    }
    let existing: Option<Uuid> =
        sqlx::query_scalar("SELECT terminal_id FROM terminal_sessions WHERE attempt_id=$1")
            .bind(view.attempt.attempt_id)
            .fetch_optional(&mut *tx)
            .await
            .map_err(|_| AppError::Internal)?;
    let registration = Registration {
        state: &state,
        request: r,
        principal: node.id(),
        config: &config,
    };
    match existing {
        Some(id) => registration.existing(tx, &view, id).await,
        None => registration.create(tx, &view).await,
    }
}
struct Registration<'a> {
    state: &'a AppState,
    request: &'a draft::Register,
    principal: Uuid,
    config: &'a super::super::config::Loaded,
}
impl Registration<'_> {
    async fn existing(
        &self,
        mut tx: Tx,
        view: &authority::AuthorityView,
        existing: Uuid,
    ) -> Result<TerminalInfo> {
        let r = self.request;
        if existing != r.terminal.terminal_id {
            return Err(AppError::Conflict(
                "attempt already owns another terminal identity".into(),
            ));
        }
        let loaded = load(self.state, &mut tx, &r.terminal, view).await?;
        if loaded.ledger.input_mode() != r.input_mode {
            return Err(AppError::Conflict(
                "terminal input mode is immutable".into(),
            ));
        }
        if let Some(reply) = replay(
            &mut tx,
            r.terminal.terminal_id,
            RequestKey {
                id: r.operation_id,
                principal: self.principal,
                digest: &digest(r)?,
            },
        )
        .await?
        {
            let MutationReply::Registered { info, .. } = reply else {
                return Err(AppError::Internal);
            };
            tx.commit().await.map_err(|_| AppError::Internal)?;
            return Ok(info);
        }
        if let Some(transition) = self.reconcile(view, &loaded)? {
            return self
                .commit_reconciliation(tx, loaded.output_observed_at, transition)
                .await;
        }
        let response = info(
            &loaded.ledger,
            loaded.peer_observed_at,
            loaded.output_observed_at,
            false,
        );
        save_operation(
            &mut tx,
            r.terminal.terminal_id,
            &registration_operation(r, self.principal, response.clone())?,
            now(self.state),
        )
        .await?;
        tx.commit().await.map_err(|_| AppError::Internal)?;
        Ok(response)
    }
    async fn commit_reconciliation(
        &self,
        tx: Tx,
        output_observed_at: u64,
        transition: (Plan, bool),
    ) -> Result<TerminalInfo> {
        let (plan, recovering) = transition;
        let response = summary_info(plan.preview(), now(self.state), output_observed_at, false);
        let mut journal = SqlJournal {
            state: self.state,
            tx: Some(tx),
            operation: Some(registration_operation(
                self.request,
                self.principal,
                response,
            )?),
            peer_contact: true,
            recovering,
        };
        let value = plan.commit_async(&mut journal).await.map_err(relay)?;
        let last_output = if recovering { output_observed_at } else { 0 };
        Ok(info(&value.ledger, now(self.state), last_output, false))
    }
    fn reconcile(
        &self,
        view: &authority::AuthorityView,
        loaded: &LoadedLedger,
    ) -> Result<Option<(Plan, bool)>> {
        if view.authority_revision < loaded.authority_revision {
            return Err(AppError::Conflict(
                "terminal authority revision regressed".into(),
            ));
        }
        if view.authority_revision > loaded.authority_revision {
            let plan = loaded
                .ledger
                .rebind_authority(
                    view.authority_revision,
                    view.execution_deadline_ms.ok_or(AppError::Internal)?,
                    Uuid::new_v4(),
                )
                .map_err(relay)?;
            return Ok(Some((plan, false)));
        }
        if loaded.runtime_id != self.config.runtime_id {
            let recovery =
                Ledger::recover(&loaded.ledger.encode().map_err(relay)?).map_err(relay)?;
            return Ok(Some((recovery, true)));
        }
        Ok(None)
    }
    async fn create(&self, mut tx: Tx, view: &authority::AuthorityView) -> Result<TerminalInfo> {
        let r = self.request;
        reserve_capacity(&mut tx).await?;
        let ceiling = view.execution_deadline_ms.ok_or(AppError::Internal)?;
        let ledger = Ledger::new_profiled(
            r.terminal.clone(),
            view.authority_revision,
            ceiling,
            None,
            view.terminal_policy
                .clone()
                .ok_or_else(|| AppError::Conflict("sealed terminal policy unavailable".into()))?,
            r.input_mode == InputMode::EpochV1,
        )
        .map_err(relay)?;
        self.insert(&mut tx, view, &ledger).await?;
        save_operation(
            &mut tx,
            r.terminal.terminal_id,
            &registration_operation(r, self.principal, info(&ledger, now(self.state), 0, false))?,
            now(self.state),
        )
        .await?;
        tx.commit().await.map_err(|_| AppError::Internal)?;
        Ok(info(&ledger, now(self.state), 0, false))
    }
    async fn insert(
        &self,
        tx: &mut Tx,
        view: &authority::AuthorityView,
        ledger: &Ledger,
    ) -> Result<()> {
        let r = self.request;
        let sealed = self
            .config
            .keyring
            .seal(
                &Context {
                    terminal: r.terminal.clone(),
                    ledger_revision: 0,
                    purpose: Purpose::Ledger,
                },
                &ledger.encode().map_err(relay)?,
            )
            .map_err(|_| AppError::Internal)?;
        let SessionIdentity::Remote {
            node_id,
            boot_id,
            attempt_id,
            assignment_generation,
            session_id,
        } = r.terminal.session
        else {
            return Err(AppError::Conflict("central authority unavailable".into()));
        };
        sqlx::query("INSERT INTO terminal_sessions(terminal_id,attempt_id,node_id,boot_id,session_id,assignment_generation,terminal_epoch,identity,authority_revision,execution_spec_digest,launch_manifest_digest,input_mode,ledger_revision,runtime_id,protection_version,key_id,nonce,ciphertext,controller_generation,pending_bytes,operation_count,observed_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8::jsonb,$9,$10,$11,$12,0,$13,$14,$15,$16,$17,0,0,0,$18)")
  .bind(r.terminal.terminal_id).bind(attempt_id).bind(node_id).bind(boot_id).bind(session_id).bind(dbtime(assignment_generation)?).bind(dbtime(r.terminal.epoch)?).bind(serde_json::to_string(&r.terminal).map_err(|_|AppError::Internal)?).bind(dbtime(view.authority_revision)?).bind(&view.execution_spec_digest).bind(view.launch_manifest_digest.as_ref().ok_or(AppError::Internal)?.as_str()).bind(r.input_mode.key()).bind(self.config.runtime_id).bind(sealed.version as i32).bind(sealed.key_id).bind(sealed.nonce).bind(sealed.ciphertext).bind(dbtime(now(self.state))?).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
        Ok(())
    }
}
async fn reserve_capacity(tx: &mut Tx) -> Result<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(x'61717465726d'::bigint)")
        .execute(&mut **tx)
        .await
        .map_err(|_| AppError::Internal)?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM terminal_sessions")
        .fetch_one(&mut **tx)
        .await
        .map_err(|_| AppError::Internal)?;
    if count >= 4096 {
        return Err(AppError::IdentityUnavailable(
            "terminal registry capacity exhausted".into(),
        ));
    }
    Ok(())
}
fn registration_operation(
    r: &draft::Register,
    principal: Uuid,
    info: TerminalInfo,
) -> Result<Operation> {
    Ok(Operation {
        id: r.operation_id,
        principal,
        digest: digest(r)?,
        kind: "register",
        response: MutationReply::Registered {
            operation_id: r.operation_id,
            info,
        },
    })
}
