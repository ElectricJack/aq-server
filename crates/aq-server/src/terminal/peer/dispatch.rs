use super::*;
use aq_terminal_relay::{Access, Plan};
pub(super) async fn dispatch_next(
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
    let active = active(state, &mut tx, &t, Rows::Lock).await?;
    let access = match AccessSubject::new(&view, &t, principal)
        .authorize(state, &mut tx, Right::Control)
        .await
    {
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
    let candidate = Candidate {
        id,
        principal,
        access,
        active: active && view.authority_revision == loaded.authority_revision,
    };
    let Some(decision) = candidate.plan(state, &loaded, r)? else {
        return Ok(None);
    };
    let mut journal = SqlJournal {
        state,
        tx: Some(tx),
        operation: None,
        peer_contact: false,
        recovering: false,
    };
    decision.commit(&mut journal).await
}
enum Decision {
    Update(Plan),
    Dispatch(Plan),
}
impl Decision {
    async fn commit(self, journal: &mut SqlJournal<'_>) -> Result<Option<draft::Effect>> {
        match self {
            Self::Update(plan) => {
                plan.commit_async(journal).await.map_err(relay)?;
                Ok(None)
            }
            Self::Dispatch(plan) => Ok(plan
                .commit_async(journal)
                .await
                .map_err(relay)?
                .effects
                .into_iter()
                .next()),
        }
    }
}
struct Candidate {
    id: Uuid,
    principal: Uuid,
    access: Option<Access>,
    active: bool,
}
impl Candidate {
    fn plan(
        self,
        state: &AppState,
        loaded: &LoadedLedger,
        r: &draft::RelayPoll,
    ) -> Result<Option<Decision>> {
        let metadata = loaded.ledger.input_metadata();
        let Some(exact) = metadata.iter().find(|v| v.operation_id == self.id) else {
            return Ok(None);
        };
        if exact.principal.as_str() != self.principal.to_string() {
            return Err(AppError::Internal);
        }
        let clock = now(state);
        if clock >= exact.deadline_ms {
            return loaded
                .ledger
                .expire(clock)
                .map(|p| Some(Decision::Update(p)))
                .map_err(relay);
        }
        let permitted = self
            .access
            .filter(|a| self.active && clock < a.valid_until_ms.min(a.execution_expires_at_ms));
        let Some(mut access) = permitted else {
            return loaded
                .ledger
                .reject_queued_for(&exact.principal)
                .map(|p| Some(Decision::Update(p)))
                .map_err(relay);
        };
        access.execution_active = true;
        access.session_revision = loaded.authority_revision;
        require_mode(&loaded.ledger, &r.capability)?;
        let plan = match loaded.ledger.input_mode() {
            draft::InputMode::FiniteV1 => loaded.ledger.dispatch_input(&access, self.id, clock),
            draft::InputMode::EpochV1 => {
                loaded.ledger.dispatch_epoch_input(&access, self.id, clock)
            }
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
        Ok(Some(Decision::Dispatch(plan)))
    }
}
