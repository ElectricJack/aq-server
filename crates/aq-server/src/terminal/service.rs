//! Scoped mutations over the common authority bridge and encrypted SQL ledger.
use super::{
    persistence::*,
    protection::{Context, Purpose},
};
use crate::{
    AppState,
    error::AppError,
    execution::authority::{self, Policy},
};
use aq_session_api as api;
use aq_session_protocol::{SessionIdentity, TerminalIdentity, Validate};
pub use aq_terminal_relay::api::Mutation;
use aq_terminal_relay::{
    Ledger, Plan,
    api::{MutationReply, TerminalInfo},
    draft::{self, InputMode},
};
use uuid::Uuid;
pub trait MutationExt {
    fn binding(&self) -> (&TerminalIdentity, Uuid, &'static str, Right, bool);
    fn plan(
        &self,
        l: &Ledger,
        a: &aq_terminal_relay::Access,
        clock: u64,
    ) -> Result<(Plan, MutationReply)>;
}
impl MutationExt for Mutation {
    fn binding(&self) -> (&TerminalIdentity, Uuid, &'static str, Right, bool) {
        match self {
            Self::Watch(r) => (&r.terminal, r.operation_id, "watch", Right::Inspect, false),
            Self::Control(r) => (&r.terminal, r.operation_id, "control", Right::Control, true),
            Self::Renew(r) => (&r.terminal, r.operation_id, "renew", Right::Control, true),
            Self::Release(r) => (&r.terminal, r.operation_id, "release", Right::Control, true),
            Self::Input(r) => (&r.terminal, r.operation_id, "input", Right::Control, true),
            Self::EpochInput(r) => (
                &r.input.terminal,
                r.input.operation_id,
                "input",
                Right::Control,
                true,
            ),
            Self::OpenEpoch(r) => (
                &r.terminal,
                r.operation_id,
                "open_epoch",
                Right::Control,
                true,
            ),
            Self::Resize(r) => (&r.terminal, r.operation_id, "resize", Right::Control, true),
            Self::Capture(r) => (&r.terminal, r.operation_id, "capture", Right::Both, true),
            Self::Resolve(r) => (&r.terminal, r.operation_id, "resolve", Right::Both, true),
        }
    }
    fn plan(
        &self,
        l: &Ledger,
        a: &aq_terminal_relay::Access,
        clock: u64,
    ) -> Result<(Plan, MutationReply)> {
        let (t, id, _, _, _) = self.binding();
        let token = Uuid::new_v4();
        let plan = match self {
            Self::Watch(r) => l.watch(a, r, clock),
            Self::Control(r) => l.acquire(a, r, token, Uuid::new_v4(), clock),
            Self::Renew(r) => l.renew(a, r, token, Uuid::new_v4(), clock),
            Self::Release(r) => l.release(a, r, token, clock),
            Self::Input(r) => l.input(a, r, clock),
            Self::EpochInput(r) => l.epoch_input(a, r, clock),
            Self::OpenEpoch(r) => l.open_epoch_request(a, r, token, clock),
            Self::Resize(r) => l.resize(a, r, clock),
            Self::Capture(r) => l.capture(a, r, token, clock),
            Self::Resolve(r) => l.resolve(a, r, clock),
        }
        .map_err(relay)?;
        let n = plan.preview();
        let reply = match self {
            Self::Watch(r) => MutationReply::Watch {
                terminal: t.clone(),
                operation_id: id,
                subscription_id: id,
                session_revision: n.session_revision,
                expires_at_ms: r.expires_at_ms,
            },
            Self::Control(_) | Self::Renew(_) => MutationReply::Control {
                operation_id: id,
                status: n.control,
            },
            Self::Release(_) => MutationReply::Released {
                terminal: t.clone(),
                operation_id: id,
                generation: n.generation,
            },
            Self::Input(_) | Self::EpochInput(_) => MutationReply::Input(api::InputReceipt {
                operation_id: id,
                terminal: t.clone(),
                disposition: aq_session_protocol::InputDisposition::Queued,
                input_paused: n.control.input_paused,
                observed_at_ms: clock,
            }),
            Self::OpenEpoch(_) => MutationReply::Epoch {
                operation_id: id,
                permit: n.epoch_retirement.ok_or(AppError::Internal)?.permit,
            },
            Self::Resize(r) => MutationReply::Resize {
                terminal: t.clone(),
                operation_id: id,
                sequence: r.sequence,
            },
            Self::Capture(r) => MutationReply::Capture {
                terminal: t.clone(),
                operation_id: id,
                capture_request_id: token,
                expires_at_ms: r.expires_at_ms.min(a.valid_until_ms).min(
                    n.control
                        .lease
                        .as_ref()
                        .ok_or(AppError::Internal)?
                        .expires_at_ms,
                ),
            },
            Self::Resolve(r) => MutationReply::Input(api::InputReceipt {
                operation_id: r.uncertain_operation_id,
                terminal: t.clone(),
                disposition: aq_session_protocol::InputDisposition::DeliveryUnknown,
                input_paused: n.control.input_paused,
                observed_at_ms: clock,
            }),
        };
        Ok((plan, reply))
    }
}
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
    let request_digest = digest(request)?;
    for _ in 0..3 {
        let mut tx = super::persistence::begin(&state).await?;
        let historical =
            authority::load_locked(&state, &mut tx, &t.session, Policy::TerminalHistorical).await?;
        let mut access = authorize(&state, &mut tx, &historical, t, actor, right).await?;
        let view = if active {
            authority::load_locked(&state, &mut tx, &t.session, Policy::TerminalActive).await?
        } else {
            historical
        };
        access.execution_active = active;
        access.execution_expires_at_ms = view.grant_expires_at_ms;
        let loaded = load(&state, &mut tx, t, &view).await?;
        access.session_revision = loaded.authority_revision;
        if active && view.authority_revision != loaded.authority_revision {
            return Err(AppError::Conflict(
                "terminal authority requires explicit registration reconciliation".into(),
            ));
        }
        if loaded.runtime_id != config.runtime_id {
            let mut journal = SqlJournal {
                state: &state,
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
            continue;
        }
        let expired = loaded.ledger.expire(now(&state)).map_err(relay)?;
        if expired.preview().ledger_revision != loaded.ledger.summary().ledger_revision {
            let mut journal = SqlJournal {
                state: &state,
                tx: Some(tx),
                operation: None,
                peer_contact: false,
                recovering: false,
            };
            expired.commit_async(&mut journal).await.map_err(relay)?;
            continue;
        }
        if !matches!(request, Mutation::EpochInput(_))
            && let Some(reply) = replay(&mut tx, t.terminal_id, id, actor, &request_digest).await?
        {
            let reply = if let MutationReply::Input(receipt) = reply {
                MutationReply::Input(
                    loaded
                        .ledger
                        .receipt(&access, receipt.operation_id, now(&state))
                        .map_err(relay)?,
                )
            } else {
                reply
            };
            tx.commit().await.map_err(|_| AppError::Internal)?;
            return Ok(reply);
        }
        let (plan, reply) = request.plan(&loaded.ledger, &access, now(&state))?;
        match request {
            Mutation::Watch(r) => config.runtime.reserve(
                t,
                loaded.ledger.terminal_policy(),
                r.expires_at_ms,
                now(&state),
            )?,
            Mutation::Capture(r) => config.runtime.reserve(
                t,
                loaded.ledger.terminal_policy(),
                r.expires_at_ms,
                now(&state),
            )?,
            _ => {}
        }
        let operation = if matches!(request, Mutation::EpochInput(_)) {
            None
        } else {
            Some(Operation {
                id,
                principal: actor,
                digest: request_digest.clone(),
                kind,
                response: reply.clone(),
            })
        };
        let mut journal = SqlJournal {
            state: &state,
            tx: Some(tx),
            operation,
            peer_contact: false,
            recovering: false,
        };
        let committed = plan.commit_async(&mut journal).await.map_err(relay)?;
        if let SessionIdentity::Remote { node_id, .. } = t.session {
            config.runtime.wake(node_id);
        }
        if let MutationReply::Input(_) = reply {
            let operation = match request {
                Mutation::Resolve(r) => r.uncertain_operation_id,
                _ => id,
            };
            return Ok(MutationReply::Input(
                committed
                    .ledger
                    .receipt(&access, operation, now(&state))
                    .map_err(relay)?,
            ));
        }
        return Ok(reply);
    }
    Err(AppError::Conflict(
        "terminal recovery changed; retry inspection".into(),
    ))
}
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
    require_owner(&state, &mut tx).await?;
    super::peer::current_peer(&state, &mut tx, node, &r.peer).await?;
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
    if let Some(existing) = existing {
        if existing != r.terminal.terminal_id {
            return Err(AppError::Conflict(
                "attempt already owns another terminal identity".into(),
            ));
        }
        let loaded = load(&state, &mut tx, &r.terminal, &view).await?;
        if loaded.ledger.input_mode() != r.input_mode {
            return Err(AppError::Conflict(
                "terminal input mode is immutable".into(),
            ));
        }
        if let Some(reply) = replay(
            &mut tx,
            r.terminal.terminal_id,
            r.operation_id,
            node.id(),
            &digest(r)?,
        )
        .await?
        {
            let MutationReply::Registered { info, .. } = reply else {
                return Err(AppError::Internal);
            };
            tx.commit().await.map_err(|_| AppError::Internal)?;
            return Ok(info);
        }
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
            let response = summary_info(
                plan.preview(),
                now(&state),
                loaded.output_observed_at,
                false,
            );
            let mut j = SqlJournal {
                state: &state,
                tx: Some(tx),
                operation: Some(registration_operation(r, node.id(), response)?),
                peer_contact: true,
                recovering: false,
            };
            let value = plan.commit_async(&mut j).await.map_err(relay)?;
            return Ok(info(&value.ledger, now(&state), 0, false));
        }
        if loaded.runtime_id != config.runtime_id {
            let plan = Ledger::recover(&loaded.ledger.encode().map_err(relay)?).map_err(relay)?;
            let mut j = SqlJournal {
                state: &state,
                tx: Some(tx),
                operation: Some(registration_operation(
                    r,
                    node.id(),
                    summary_info(
                        plan.preview(),
                        now(&state),
                        loaded.output_observed_at,
                        false,
                    ),
                )?),
                peer_contact: true,
                recovering: true,
            };
            let value = plan.commit_async(&mut j).await.map_err(relay)?;
            return Ok(info(
                &value.ledger,
                now(&state),
                loaded.output_observed_at,
                false,
            ));
        }
        save_operation(
            &mut tx,
            r.terminal.terminal_id,
            &registration_operation(
                r,
                node.id(),
                info(
                    &loaded.ledger,
                    loaded.peer_observed_at,
                    loaded.output_observed_at,
                    false,
                ),
            )?,
            now(&state),
        )
        .await?;
        tx.commit().await.map_err(|_| AppError::Internal)?;
        return Ok(info(
            &loaded.ledger,
            loaded.peer_observed_at,
            loaded.output_observed_at,
            false,
        ));
    }
    sqlx::query("SELECT pg_advisory_xact_lock(x'61717465726d'::bigint)")
        .execute(&mut *tx)
        .await
        .map_err(|_| AppError::Internal)?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM terminal_sessions")
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| AppError::Internal)?;
    if count >= 4096 {
        return Err(AppError::IdentityUnavailable(
            "terminal registry capacity exhausted".into(),
        ));
    }
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
    let sealed = config
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
  .bind(r.terminal.terminal_id).bind(attempt_id).bind(node_id).bind(boot_id).bind(session_id).bind(dbtime(assignment_generation)?).bind(dbtime(r.terminal.epoch)?).bind(serde_json::to_string(&r.terminal).map_err(|_|AppError::Internal)?).bind(dbtime(view.authority_revision)?).bind(&view.execution_spec_digest).bind(view.launch_manifest_digest.as_ref().ok_or(AppError::Internal)?.as_str()).bind(r.input_mode.key()).bind(config.runtime_id).bind(sealed.version as i32).bind(sealed.key_id).bind(sealed.nonce).bind(sealed.ciphertext).bind(dbtime(now(&state))?).execute(&mut *tx).await.map_err(|_|AppError::Internal)?;
    save_operation(
        &mut tx,
        r.terminal.terminal_id,
        &registration_operation(r, node.id(), info(&ledger, now(&state), 0, false))?,
        now(&state),
    )
    .await?;
    tx.commit().await.map_err(|_| AppError::Internal)?;
    Ok(info(&ledger, now(&state), 0, false))
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
fn info(l: &Ledger, peer: u64, output: u64, disconnected: bool) -> TerminalInfo {
    summary_info(l.summary(), peer, output, disconnected)
}
fn summary_info(
    s: aq_terminal_relay::LedgerSummary,
    peer: u64,
    output: u64,
    disconnected: bool,
) -> TerminalInfo {
    TerminalInfo {
        terminal: s.terminal,
        session_revision: s.session_revision,
        input_mode: s.input_mode,
        terminal_policy: s.terminal_policy,
        control: s.control,
        last_peer_observed_at_ms: peer,
        last_output_observed_at_ms: output,
        disconnected,
    }
}
pub async fn inspect(state: &AppState, actor: Uuid, id: Uuid) -> Result<TerminalInfo> {
    let state = state.terminal_read_view();
    let config = configured(&state)?;
    let t = identity(&state, id).await?;
    let mut tx = super::persistence::begin(&state).await?;
    let view =
        authority::load_locked(&state, &mut tx, &t.session, Policy::TerminalHistorical).await?;
    authorize(&state, &mut tx, &view, &t, actor, Right::Inspect).await?;
    let loaded = load(&state, &mut tx, &t, &view).await?;
    let active =
        match authority::load_locked(&state, &mut tx, &t.session, Policy::TerminalActive).await {
            Ok(_) => true,
            Err(authority::AuthorityError::InvalidState) => return Err(AppError::Internal),
            Err(authority::AuthorityError::Database(_)) => return Err(AppError::Internal),
            Err(_) => false,
        };
    let disconnected = !active
        || loaded.runtime_id != config.runtime_id
        || now(&state).saturating_sub(loaded.peer_observed_at) > 25000;
    tx.commit().await.map_err(|_| AppError::Internal)?;
    let mut response = info(
        &loaded.ledger,
        loaded.peer_observed_at,
        loaded.output_observed_at,
        disconnected,
    );
    if loaded.runtime_id != config.runtime_id {
        response.control.fence_acknowledged = false;
    }
    Ok(response)
}

pub async fn output(
    state: &AppState,
    actor: Uuid,
    id: Uuid,
    query: &aq_terminal_relay::api::ViewQuery,
) -> Result<aq_terminal_relay::api::OutputReply> {
    use aq_terminal_relay::api::OutputReply;
    let state = state.terminal_read_view();
    let config = configured(&state)?;
    let t = identity(&state, id).await?;
    let mut tx = super::persistence::begin(&state).await?;
    let view =
        authority::load_locked(&state, &mut tx, &t.session, Policy::TerminalHistorical).await?;
    let mut access = authorize(&state, &mut tx, &view, &t, actor, Right::Inspect).await?;
    let loaded = load(&state, &mut tx, &t, &view).await?;
    access.session_revision = loaded.authority_revision;
    let clock = now(&state);
    loaded
        .ledger
        .authorize_watch(&access, query.subscription_id, clock)
        .map_err(relay)?;
    let replay = config.runtime.view(
        &loaded.ledger,
        &access,
        query.subscription_id,
        query.after_sequence,
        clock,
    )?;
    let subscription = loaded
        .ledger
        .summary()
        .subscriptions
        .into_iter()
        .find(|s| s.subscription_id == query.subscription_id)
        .ok_or(AppError::Internal)?;
    let active =
        match authority::load_locked(&state, &mut tx, &t.session, Policy::TerminalActive).await {
            Ok(_) => true,
            Err(
                authority::AuthorityError::Database(_) | authority::AuthorityError::InvalidState,
            ) => return Err(AppError::Internal),
            Err(_) => false,
        };
    tx.commit().await.map_err(|_| AppError::Internal)?;
    let pending = || OutputReply::Pending {
        terminal: t.clone(),
        subscription_id: query.subscription_id,
        last_peer_observed_at_ms: loaded.peer_observed_at,
        disconnected: true,
        resynchronize: true,
    };
    let Some(replay) = replay else {
        return Ok(pending());
    };
    let Some(snapshot) = replay.snapshot else {
        return Ok(pending());
    };
    let original = replay.frames.len();
    let mut frames = vec![];
    let mut bytes = 0;
    for frame in replay.frames {
        if frames.len() >= 128 || bytes + frame.bytes.len() > 32768 {
            break;
        }
        bytes += frame.bytes.len();
        frames.push(frame);
    }
    let next = frames
        .last()
        .map_or(snapshot.through_sequence, |f| f.sequence)
        .max(query.after_sequence);
    let result = OutputReply::Ready {
        view: Box::new(api::WatchView {
            terminal: t,
            subscription_id: query.subscription_id,
            session_revision: loaded.authority_revision,
            expires_at_ms: subscription.expires_at_ms,
            last_observed_at_ms: clock.saturating_sub(replay.age_ms),
            disconnected: !active
                || replay.disconnected
                || clock.saturating_sub(loaded.peer_observed_at) > 25000,
            coverage: if replay.truncated {
                api::Coverage::Gapped
            } else {
                api::Coverage::Complete
            },
            truncated: replay.truncated,
            snapshot_id: snapshot.snapshot_id,
            snapshot_reference: api::Source {
                identity: snapshot.snapshot_id.to_string(),
                digest: snapshot.digest.as_str().into(),
                revision: "1".into(),
            },
            renderer_reference: api::Source {
                identity: snapshot.renderer.id.as_str().into(),
                digest: snapshot.renderer.digest.as_str().into(),
                revision: snapshot.renderer.version.as_str().into(),
            },
            through_sequence: snapshot.through_sequence,
            columns: snapshot.columns,
            rows: snapshot.rows,
            gaps: replay.gaps,
        }),
        snapshot,
        next_sequence: next,
        more: frames.len() < original,
        frames,
    };
    if serde_json::to_vec(&result)
        .map_err(|_| AppError::Internal)?
        .len()
        > 1048576
    {
        return Err(AppError::Internal);
    }
    Ok(result)
}

/// Owner-managed exact-session inspection and independent terminal control grants.
/// ACL writes never grant execution authority or extend an execution/controller lease.
pub async fn set_access(
    state: &AppState,
    actor: Uuid,
    r: &aq_terminal_relay::api::AccessUpdate,
) -> Result<MutationReply> {
    use crate::{session_access, store::Role};
    use sqlx::Row;
    if r.operation_id.is_nil() || r.principal_id.is_nil() {
        return Err(AppError::BadRequest(
            "invalid terminal access identity".into(),
        ));
    }
    let state = state.terminal_view();
    let config = configured(&state)?;
    let mut tx = super::persistence::begin(&state).await?;
    let view = authority::load_locked(
        &state,
        &mut tx,
        &r.terminal.session,
        Policy::TerminalHistorical,
    )
    .await?;
    if session_access::current_role_locked(&mut tx, actor).await? != Role::Owner {
        return Err(AppError::Forbidden(
            "terminal access requires current owner".into(),
        ));
    }
    session_access::current_role_locked(&mut tx, r.principal_id).await?;
    let key = session_access::session_key(&r.terminal.session)?;
    let inspect=sqlx::query("SELECT revision,session_identity::text FROM session_inspection_acl WHERE session_key=$1 AND principal_id=$2 FOR UPDATE").bind(&key).bind(r.principal_id).fetch_optional(&mut *tx).await.map_err(|_|AppError::Internal)?;
    if let Some(row) = &inspect {
        let exact: SessionIdentity =
            serde_json::from_str(row.get::<&str, _>(1)).map_err(|_| AppError::Internal)?;
        if exact != r.terminal.session {
            return Err(AppError::Internal);
        }
    }
    let control:Option<i64>=sqlx::query_scalar("SELECT revision FROM terminal_control_acl WHERE terminal_id=$1 AND principal_id=$2 FOR UPDATE").bind(r.terminal.terminal_id).bind(r.principal_id).fetch_optional(&mut *tx).await.map_err(|_|AppError::Internal)?;
    let loaded = load(&state, &mut tx, &r.terminal, &view).await?;
    if r.expected_session_revision != loaded.authority_revision {
        return Err(AppError::Conflict(
            "terminal session revision changed".into(),
        ));
    }
    let request_digest = digest(r)?;
    if let Some(reply) = replay(
        &mut tx,
        r.terminal.terminal_id,
        r.operation_id,
        actor,
        &request_digest,
    )
    .await?
    {
        tx.commit().await.map_err(|_| AppError::Internal)?;
        return Ok(reply);
    }
    let clock = now(&state);
    if r.expires_at_ms <= clock
        || r.expires_at_ms > clock.checked_add(86_400_000).ok_or(AppError::Internal)?
    {
        return Err(AppError::BadRequest("terminal access expiry bound".into()));
    }
    if inspect.as_ref().map_or(0, |row| row.get::<i64, _>(0))
        != dbtime(r.expected_inspection_revision)?
        || control.unwrap_or(0) != dbtime(r.expected_control_revision)?
    {
        return Err(AppError::Conflict(
            "terminal access revision changed".into(),
        ));
    }
    if r.can_control && !loaded.ledger.terminal_policy().input_enabled {
        return Err(AppError::Forbidden("terminal policy is output-only".into()));
    }
    if r.can_control {
        authority::load_locked(&state, &mut tx, &r.terminal.session, Policy::TerminalActive)
            .await?;
    }
    let inspection_revision = r
        .expected_inspection_revision
        .checked_add(1)
        .ok_or(AppError::Internal)?;
    let control_revision = r
        .expected_control_revision
        .checked_add(1)
        .ok_or(AppError::Internal)?;
    let scope_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM session_inspection_acl WHERE session_key=$1")
            .bind(&key)
            .fetch_one(&mut *tx)
            .await
            .map_err(|_| AppError::Internal)?;
    let control_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM terminal_control_acl WHERE terminal_id=$1")
            .bind(r.terminal.terminal_id)
            .fetch_one(&mut *tx)
            .await
            .map_err(|_| AppError::Internal)?;
    if (inspect.is_none() && scope_count >= 128) || (control.is_none() && control_count >= 128) {
        return Err(AppError::IdentityUnavailable(
            "terminal ACL capacity exhausted".into(),
        ));
    }
    sqlx::query("INSERT INTO session_inspection_acl(session_key,session_identity,principal_id,revision,can_inspect,expires_at_ms) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(session_key,principal_id) DO UPDATE SET revision=excluded.revision,can_inspect=excluded.can_inspect,expires_at_ms=excluded.expires_at_ms").bind(key).bind(serde_json::to_value(&r.terminal.session).map_err(|_|AppError::Internal)?).bind(r.principal_id).bind(dbtime(inspection_revision)?).bind(r.can_inspect).bind(dbtime(r.expires_at_ms)?).execute(&mut *tx).await.map_err(|_|AppError::Internal)?;
    sqlx::query("INSERT INTO terminal_control_acl(terminal_id,principal_id,revision,can_control,expires_at_ms) VALUES($1,$2,$3,$4,$5) ON CONFLICT(terminal_id,principal_id) DO UPDATE SET revision=excluded.revision,can_control=excluded.can_control,expires_at_ms=excluded.expires_at_ms").bind(r.terminal.terminal_id).bind(r.principal_id).bind(dbtime(control_revision)?).bind(r.can_control).bind(dbtime(r.expires_at_ms)?).execute(&mut *tx).await.map_err(|_|AppError::Internal)?;
    let mut plan = if loaded.runtime_id != config.runtime_id {
        Ledger::recover(&loaded.ledger.encode().map_err(relay)?)
    } else {
        loaded.ledger.expire(clock)
    }
    .map_err(relay)?;
    if (!r.can_control
        || loaded
            .ledger
            .summary()
            .control
            .lease
            .as_ref()
            .is_some_and(|c| c.expires_at_ms > r.expires_at_ms))
        && loaded
            .ledger
            .summary()
            .control
            .lease
            .as_ref()
            .is_some_and(|c| c.principal_id.as_str() == r.principal_id.to_string())
    {
        plan = plan.and_then(|l| l.revoke(Uuid::new_v4())).map_err(relay)?;
    }
    let reply = MutationReply::Access {
        terminal: r.terminal.clone(),
        operation_id: r.operation_id,
        inspection_revision,
        control_revision,
    };
    let mut journal = SqlJournal {
        state: &state,
        tx: Some(tx),
        operation: Some(Operation {
            id: r.operation_id,
            principal: actor,
            digest: request_digest,
            kind: "acl",
            response: reply.clone(),
        }),
        peer_contact: false,
        recovering: loaded.runtime_id != config.runtime_id,
    };
    plan.commit_async(&mut journal).await.map_err(relay)?;
    if let SessionIdentity::Remote { node_id, .. } = r.terminal.session {
        config.runtime.wake(node_id);
    }
    Ok(reply)
}

pub async fn receipt(
    state: &AppState,
    actor: Uuid,
    id: Uuid,
    operation: Uuid,
) -> Result<api::InputReceipt> {
    let state = state.terminal_read_view();
    let config = configured(&state)?;
    let t = identity(&state, id).await?;
    let mut tx = begin(&state).await?;
    let view =
        authority::load_locked(&state, &mut tx, &t.session, Policy::TerminalHistorical).await?;
    let (mut access, inspect) =
        match authorize(&state, &mut tx, &view, &t, actor, Right::Inspect).await {
            Ok(a) => (a, true),
            Err(AppError::Forbidden(_)) => (
                authorize(&state, &mut tx, &view, &t, actor, Right::Control).await?,
                false,
            ),
            Err(e) => return Err(e),
        };
    let loaded = load(&state, &mut tx, &t, &view).await?;
    if loaded.runtime_id != config.runtime_id {
        return Err(AppError::IdentityUnavailable(
            "terminal restart recovery pending".into(),
        ));
    }
    access.session_revision = loaded.authority_revision;
    let receipt = if inspect {
        loaded
            .ledger
            .inspected_receipt(&access, operation, now(&state))
    } else {
        loaded.ledger.receipt(&access, operation, now(&state))
    }
    .map_err(relay)?;
    tx.commit().await.map_err(|_| AppError::Internal)?;
    Ok(receipt)
}
