use super::*;
use crate::{session_access, store::Role};
use aq_terminal_relay::api::AccessUpdate;
use sqlx::Row;
/// Owner-managed exact-session inspection and independent terminal control grants.
/// ACL writes never grant execution authority or extend an execution/controller lease.
pub async fn set_access(state: &AppState, actor: Uuid, r: &AccessUpdate) -> Result<MutationReply> {
    if r.operation_id.is_nil() || r.principal_id.is_nil() {
        return Err(AppError::BadRequest(
            "invalid terminal access identity".into(),
        ));
    }
    let state = state.terminal_view();
    let config = configured(&state)?;
    let mut tx = super::persistence::begin(&state).await?;
    let (grants, loaded) = lock_grants(&state, &mut tx, r, actor).await?;
    let request_digest = digest(r)?;
    if let Some(reply) = replay(
        &mut tx,
        r.terminal.terminal_id,
        RequestKey {
            id: r.operation_id,
            principal: actor,
            digest: &request_digest,
        },
    )
    .await?
    {
        tx.commit().await.map_err(|_| AppError::Internal)?;
        return Ok(reply);
    }
    let clock = now(&state);
    grants.validate(r, &loaded.ledger, clock)?;
    if r.can_control {
        authority::load_locked(&state, &mut tx, &r.terminal.session, Policy::TerminalActive)
            .await?;
    }
    let (inspection_revision, control_revision) = grants.update(&mut tx, r).await?;
    let recovering = loaded.runtime_id != config.runtime_id;
    let plan = control_plan(&loaded, r, recovering, clock)?;
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
        recovering,
    };
    plan.commit_async(&mut journal).await.map_err(relay)?;
    if let SessionIdentity::Remote { node_id, .. } = r.terminal.session {
        config.runtime.wake(node_id);
    }
    Ok(reply)
}

async fn lock_grants(
    state: &AppState,
    tx: &mut Tx,
    r: &AccessUpdate,
    actor: Uuid,
) -> Result<(Grants, LoadedLedger)> {
    let view =
        authority::load_locked(state, tx, &r.terminal.session, Policy::TerminalHistorical).await?;
    if session_access::current_role_locked(tx, actor).await? != Role::Owner {
        return Err(AppError::Forbidden(
            "terminal access requires current owner".into(),
        ));
    }
    session_access::current_role_locked(tx, r.principal_id).await?;
    let grants = Grants::lock(tx, r).await?;
    let loaded = load(state, tx, &r.terminal, &view).await?;
    if r.expected_session_revision != loaded.authority_revision {
        return Err(AppError::Conflict(
            "terminal session revision changed".into(),
        ));
    }
    Ok((grants, loaded))
}
struct Grants {
    key: String,
    inspect: Option<i64>,
    control: Option<i64>,
}
impl Grants {
    async fn lock(tx: &mut Tx, r: &AccessUpdate) -> Result<Self> {
        let key = session_access::session_key(&r.terminal.session)?;
        let inspect=sqlx::query("SELECT revision,session_identity::text FROM session_inspection_acl WHERE session_key=$1 AND principal_id=$2 FOR UPDATE").bind(&key).bind(r.principal_id).fetch_optional(&mut **tx).await.map_err(|_|AppError::Internal)?;
        if let Some(row) = &inspect {
            let exact: SessionIdentity =
                serde_json::from_str(row.get::<&str, _>(1)).map_err(|_| AppError::Internal)?;
            if exact != r.terminal.session {
                return Err(AppError::Internal);
            }
        }
        let control:Option<i64>=sqlx::query_scalar("SELECT revision FROM terminal_control_acl WHERE terminal_id=$1 AND principal_id=$2 FOR UPDATE").bind(r.terminal.terminal_id).bind(r.principal_id).fetch_optional(&mut **tx).await.map_err(|_|AppError::Internal)?;
        Ok(Self {
            key,
            inspect: inspect.map(|row| row.get(0)),
            control,
        })
    }
    fn validate(&self, r: &AccessUpdate, ledger: &Ledger, clock: u64) -> Result<()> {
        if r.expires_at_ms <= clock
            || r.expires_at_ms > clock.checked_add(86_400_000).ok_or(AppError::Internal)?
        {
            return Err(AppError::BadRequest("terminal access expiry bound".into()));
        }
        if self.inspect.unwrap_or(0) != dbtime(r.expected_inspection_revision)?
            || self.control.unwrap_or(0) != dbtime(r.expected_control_revision)?
        {
            return Err(AppError::Conflict(
                "terminal access revision changed".into(),
            ));
        }
        if r.can_control && !ledger.terminal_policy().input_enabled {
            return Err(AppError::Forbidden("terminal policy is output-only".into()));
        }
        Ok(())
    }
    async fn update(&self, tx: &mut Tx, r: &AccessUpdate) -> Result<(u64, u64)> {
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
                .bind(&self.key)
                .fetch_one(&mut **tx)
                .await
                .map_err(|_| AppError::Internal)?;
        let control_count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM terminal_control_acl WHERE terminal_id=$1")
                .bind(r.terminal.terminal_id)
                .fetch_one(&mut **tx)
                .await
                .map_err(|_| AppError::Internal)?;
        let full_inspect = self.inspect.is_none() && scope_count >= 128;
        let full_control = self.control.is_none() && control_count >= 128;
        if full_inspect || full_control {
            return Err(AppError::IdentityUnavailable(
                "terminal ACL capacity exhausted".into(),
            ));
        }
        sqlx::query("INSERT INTO session_inspection_acl(session_key,session_identity,principal_id,revision,can_inspect,expires_at_ms) VALUES($1,$2,$3,$4,$5,$6) ON CONFLICT(session_key,principal_id) DO UPDATE SET revision=excluded.revision,can_inspect=excluded.can_inspect,expires_at_ms=excluded.expires_at_ms").bind(&self.key).bind(serde_json::to_value(&r.terminal.session).map_err(|_|AppError::Internal)?).bind(r.principal_id).bind(dbtime(inspection_revision)?).bind(r.can_inspect).bind(dbtime(r.expires_at_ms)?).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
        sqlx::query("INSERT INTO terminal_control_acl(terminal_id,principal_id,revision,can_control,expires_at_ms) VALUES($1,$2,$3,$4,$5) ON CONFLICT(terminal_id,principal_id) DO UPDATE SET revision=excluded.revision,can_control=excluded.can_control,expires_at_ms=excluded.expires_at_ms").bind(r.terminal.terminal_id).bind(r.principal_id).bind(dbtime(control_revision)?).bind(r.can_control).bind(dbtime(r.expires_at_ms)?).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
        Ok((inspection_revision, control_revision))
    }
}
fn control_plan(
    loaded: &LoadedLedger,
    r: &AccessUpdate,
    recovering: bool,
    clock: u64,
) -> Result<Plan> {
    let mut plan = if recovering {
        Ledger::recover(&loaded.ledger.encode().map_err(relay)?)
    } else {
        loaded.ledger.expire(clock)
    }
    .map_err(relay)?;
    let lease = loaded.ledger.summary().control.lease;
    if lease.is_some_and(|c| {
        c.principal_id.as_str() == r.principal_id.to_string()
            && (!r.can_control || c.expires_at_ms > r.expires_at_ms)
    }) {
        plan = plan.and_then(|l| l.revoke(Uuid::new_v4())).map_err(relay)?;
    }
    Ok(plan)
}
