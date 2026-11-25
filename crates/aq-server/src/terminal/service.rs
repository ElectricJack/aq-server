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
        self.operation_binding()
    }
    fn plan(
        &self,
        ledger: &Ledger,
        access: &aq_terminal_relay::Access,
        clock: u64,
    ) -> Result<(Plan, MutationReply)> {
        let admission = aq_terminal_relay::mutation::Admission {
            access,
            token: Uuid::new_v4(),
            clock,
        };
        self.admit(ledger, admission, Uuid::new_v4).map_err(relay)
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
    let reply = aq_terminal_relay::presentation::OutputObservation {
        terminal: t,
        revision: loaded.authority_revision,
        peer_observed_at: loaded.peer_observed_at,
        expires_at,
        clock,
        active,
    }
    .render(query, replay);
    if matches!(reply, aq_terminal_relay::api::OutputReply::Ready { .. }) {
        require_wire_bound(&reply, 1048576)?;
    }
    Ok(reply)
}
pub(super) fn require_wire_bound(value: &impl serde::Serialize, limit: usize) -> Result<()> {
    let wire = serde_json::to_vec(value).map_err(|_| AppError::Internal)?;
    if wire.len() > limit {
        return Err(AppError::Internal);
    }
    Ok(())
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
