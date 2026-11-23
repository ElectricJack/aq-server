//! Scoped mutations over the common authority bridge and encrypted SQL ledger.
mod grants;
mod mutations;
pub use grants::set_access;
pub use mutations::mutate;
mod registration;
use crate::terminal::{peer, persistence};
pub use registration::register;
type Tx = sqlx::Transaction<'static, sqlx::Postgres>;
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
        let admitted = Admission {
            access: a,
            token,
            clock,
        };
        let reply = admitted.reply(self, plan.preview())?;
        Ok((plan, reply))
    }
}
struct Admission<'a> {
    access: &'a aq_terminal_relay::Access,
    token: Uuid,
    clock: u64,
}
impl Admission<'_> {
    fn reply(
        &self,
        request: &Mutation,
        n: aq_terminal_relay::LedgerSummary,
    ) -> Result<MutationReply> {
        let (t, id, _, _, _) = request.binding();
        let reply = match request {
            Mutation::Watch(r) => MutationReply::Watch {
                terminal: t.clone(),
                operation_id: id,
                subscription_id: id,
                session_revision: n.session_revision,
                expires_at_ms: r.expires_at_ms,
            },
            Mutation::Control(_) | Mutation::Renew(_) => MutationReply::Control {
                operation_id: id,
                status: n.control,
            },
            Mutation::Release(_) => MutationReply::Released {
                terminal: t.clone(),
                operation_id: id,
                generation: n.generation,
            },
            Mutation::Input(_) | Mutation::EpochInput(_) => {
                MutationReply::Input(api::InputReceipt {
                    operation_id: id,
                    terminal: t.clone(),
                    disposition: aq_session_protocol::InputDisposition::Queued,
                    input_paused: n.control.input_paused,
                    observed_at_ms: self.clock,
                })
            }
            Mutation::OpenEpoch(_) => MutationReply::Epoch {
                operation_id: id,
                permit: n.epoch_retirement.ok_or(AppError::Internal)?.permit,
            },
            Mutation::Resize(r) => MutationReply::Resize {
                terminal: t.clone(),
                operation_id: id,
                sequence: r.sequence,
            },
            Mutation::Capture(r) => MutationReply::Capture {
                terminal: t.clone(),
                operation_id: id,
                capture_request_id: self.token,
                expires_at_ms: r.expires_at_ms.min(self.access.valid_until_ms).min(
                    n.control
                        .lease
                        .as_ref()
                        .ok_or(AppError::Internal)?
                        .expires_at_ms,
                ),
            },
            Mutation::Resolve(r) => MutationReply::Input(api::InputReceipt {
                operation_id: r.uncertain_operation_id,
                terminal: t.clone(),
                disposition: aq_session_protocol::InputDisposition::DeliveryUnknown,
                input_paused: n.control.input_paused,
                observed_at_ms: self.clock,
            }),
        };
        Ok(reply)
    }
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
    let mut tx = super::persistence::begin_read(&state).await?;
    let view = authority::read(&state, &mut tx, &t.session, Policy::TerminalHistorical).await?;
    AccessSubject::new(&view, &t, actor)
        .authorize(&state, &mut tx, Right::Inspect)
        .await?;
    let loaded = load(&state, &mut tx, &t, &view).await?;
    let active = match authority::read(&state, &mut tx, &t.session, Policy::TerminalActive).await {
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
    let state = state.terminal_read_view();
    let config = configured(&state)?;
    let t = identity(&state, id).await?;
    let mut tx = super::persistence::begin_read(&state).await?;
    let view = authority::read(&state, &mut tx, &t.session, Policy::TerminalHistorical).await?;
    let access = AccessSubject::new(&view, &t, actor)
        .authorize(&state, &mut tx, Right::Inspect)
        .await?;
    let loaded = load(&state, &mut tx, &t, &view).await?;
    let clock = now(&state);
    let watch = loaded.watch(access, query, clock)?;
    let replay = config.runtime.view(&watch)?;
    let expires_at = watch.expires_at()?;
    let active = match authority::read(&state, &mut tx, &t.session, Policy::TerminalActive).await {
        Ok(_) => true,
        Err(authority::AuthorityError::Database(_) | authority::AuthorityError::InvalidState) => {
            return Err(AppError::Internal);
        }
        Err(_) => false,
    };
    tx.commit().await.map_err(|_| AppError::Internal)?;
    OutputObservation {
        terminal: t,
        revision: loaded.authority_revision,
        peer_observed_at: loaded.peer_observed_at,
        expires_at,
        clock,
        active,
    }
    .render(query, replay)
}
struct OutputObservation {
    terminal: TerminalIdentity,
    revision: u64,
    peer_observed_at: u64,
    expires_at: u64,
    clock: u64,
    active: bool,
}
impl OutputObservation {
    fn render(
        self,
        query: &aq_terminal_relay::api::ViewQuery,
        replay: Option<aq_terminal_relay::output::View>,
    ) -> Result<aq_terminal_relay::api::OutputReply> {
        use aq_terminal_relay::api::OutputReply;
        let pending = || OutputReply::Pending {
            terminal: self.terminal.clone(),
            subscription_id: query.subscription_id,
            last_peer_observed_at_ms: self.peer_observed_at,
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
        let frames = bounded_frames(replay.frames, 128, 32768);
        let next = frames
            .last()
            .map_or(snapshot.through_sequence, |f| f.sequence)
            .max(query.after_sequence);
        let result = OutputReply::Ready {
            view: Box::new(api::WatchView {
                terminal: self.terminal,
                subscription_id: query.subscription_id,
                session_revision: self.revision,
                expires_at_ms: self.expires_at,
                last_observed_at_ms: self.clock.saturating_sub(replay.age_ms),
                disconnected: !self.active
                    || replay.disconnected
                    || self.clock.saturating_sub(self.peer_observed_at) > 25000,
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
        require_wire_bound(&result, 1048576)?;
        Ok(result)
    }
}
pub(super) fn require_wire_bound(value: &impl serde::Serialize, limit: usize) -> Result<()> {
    let wire = serde_json::to_vec(value).map_err(|_| AppError::Internal)?;
    if wire.len() > limit {
        return Err(AppError::Internal);
    }
    Ok(())
}
pub(super) fn bounded_frames(
    frames: Vec<aq_session_protocol::OutputFrame>,
    limit: usize,
    byte_limit: usize,
) -> Vec<aq_session_protocol::OutputFrame> {
    let mut total = 0;
    frames
        .into_iter()
        .take(limit)
        .take_while(|frame| {
            total += frame.bytes.len();
            total <= byte_limit
        })
        .collect()
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
    let mut tx = super::persistence::begin_read(&state).await?;
    let view = authority::read(&state, &mut tx, &t.session, Policy::TerminalHistorical).await?;
    let (mut access, inspect) = match AccessSubject::new(&view, &t, actor)
        .authorize(&state, &mut tx, Right::Inspect)
        .await
    {
        Ok(a) => (a, true),
        Err(AppError::Forbidden(_)) => (
            AccessSubject::new(&view, &t, actor)
                .authorize(&state, &mut tx, Right::Control)
                .await?,
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
