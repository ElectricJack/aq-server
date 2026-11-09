//! One encrypted authoritative ledger plus bounded derived query metadata.
//! All callers first lock common node/attempt and fresh role/ACL rows. No network
//! occurs in transactions. Input outbox entries are one-shot claims, not retries.
use super::{
    config::Loaded,
    protection::{Context, Purpose, Sealed},
};
use crate::{
    AppState,
    error::AppError,
    execution::authority::{AuthorityView, Rows},
    session_access,
    store::Role,
};
use aq_session_protocol::{Digest, SessionIdentity, TerminalIdentity, Text, Validate};
use aq_terminal_relay::{Access, AsyncJournal, Ledger, api::MutationReply, draft::Effect};
use serde::Serialize;
use sqlx::{Postgres, Row, Transaction};
use std::sync::Arc;
use uuid::Uuid;
pub type Result<T> = std::result::Result<T, AppError>;
pub fn now(state: &AppState) -> u64 {
    (state.clock)().timestamp_millis().max(0) as u64
}
pub fn dbtime(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|_| AppError::BadRequest("terminal integer bound".into()))
}
pub fn relay(e: aq_terminal_relay::Error) -> AppError {
    use aq_terminal_relay::Error;
    match e {
        Error::Forbidden => AppError::Forbidden(e.to_string()),
        Error::Invalid => AppError::BadRequest(e.to_string()),
        Error::InputDeadline => AppError::InputDeadline,
        Error::Capacity => AppError::IdentityUnavailable(e.to_string()),
        Error::Persistence => AppError::Internal,
        _ => AppError::Conflict(e.to_string()),
    }
}
pub fn configured(state: &AppState) -> Result<Arc<Loaded>> {
    state
        .config
        .terminals
        .clone()
        .ok_or_else(|| AppError::IdentityUnavailable("terminal relay unavailable".into()))
}
pub fn digest(value: &impl Serialize) -> Result<String> {
    let mut v = serde_json::to_value(value).map_err(|_| AppError::Internal)?;
    v.sort_all_objects();
    Ok(
        Digest::of(&serde_json::to_vec(&v).map_err(|_| AppError::Internal)?)
            .as_str()
            .into(),
    )
}
#[derive(Clone, Copy)]
pub enum Right {
    Inspect,
    Control,
    Both,
}
pub struct Scopes {
    pub inspect: Option<Access>,
    pub control: Option<Access>,
    pub both: Option<Access>,
}
pub async fn scopes(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    view: &AuthorityView,
    terminal: &TerminalIdentity,
    actor: Uuid,
) -> Result<Scopes> {
    let role = session_access::current_role(tx, actor, view.rows).await?;
    let inspection = match session_access::require_inspect_locked(state, tx, view, actor).await {
        Ok(v) => Some(v),
        Err(AppError::Forbidden(_)) => None,
        Err(e) => return Err(e),
    };
    let row=sqlx::query(&view.rows.sql("SELECT can_control,expires_at_ms FROM terminal_control_acl WHERE terminal_id=$1 AND principal_id=$2", "FOR SHARE")).bind(terminal.terminal_id).bind(actor).fetch_optional(&mut **tx).await.map_err(|_|AppError::Internal)?;
    let checked = now(state);
    let inspect_until = inspection
        .filter(|v| checked < v.valid_until_ms)
        .map(|v| v.valid_until_ms);
    let control_until = if role == Role::Owner {
        Some(u64::MAX)
    } else {
        row.filter(|r| {
            r.get::<bool, _>(0) && r.get::<i64, _>(1) > 0 && checked < r.get::<i64, _>(1) as u64
        })
        .map(|r| r.get::<i64, _>(1) as u64)
    };
    let principal = Text::new(actor.to_string()).map_err(|_| AppError::Internal)?;
    let make = |until: Option<u64>| {
        until.map(|valid_until_ms| Access {
            principal: principal.clone(),
            terminal: terminal.clone(),
            session_revision: view.authority_revision,
            watch: inspect_until.is_some(),
            control: control_until.is_some(),
            valid_until_ms,
            execution_active: view.active_at(checked).is_ok(),
            execution_expires_at_ms: view.grant_expires_at_ms,
        })
    };
    Ok(Scopes {
        inspect: make(inspect_until),
        control: make(control_until),
        both: make(inspect_until.zip(control_until).map(|(a, b)| a.min(b))),
    })
}
pub async fn authorize(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    view: &AuthorityView,
    terminal: &TerminalIdentity,
    actor: Uuid,
    right: Right,
) -> Result<Access> {
    let scopes = scopes(state, tx, view, terminal, actor).await?;
    match right {
        Right::Inspect => scopes.inspect,
        Right::Control => scopes.control,
        Right::Both => scopes.both,
    }
    .ok_or_else(|| AppError::Forbidden("terminal scope denied".into()))
}
pub struct LoadedLedger {
    pub ledger: Ledger,
    pub runtime_id: Uuid,
    pub peer_observed_at: u64,
    pub output_observed_at: u64,
    pub authority_revision: u64,
}
pub async fn load(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    terminal: &TerminalIdentity,
    view: &AuthorityView,
) -> Result<LoadedLedger> {
    terminal
        .validate()
        .map_err(|_| AppError::BadRequest("invalid terminal identity".into()))?;
    owner(state, tx, view.rows).await?;
    let row=sqlx::query(&view.rows.sql("SELECT identity::text,authority_revision,ledger_revision,runtime_id,protection_version,key_id,nonce,ciphertext,observed_at_ms,last_output_at_ms,execution_spec_digest,launch_manifest_digest,input_mode,controller_generation,pending_bytes,controller_principal FROM terminal_sessions WHERE terminal_id=$1", "FOR UPDATE")).bind(terminal.terminal_id).fetch_optional(&mut **tx).await.map_err(|_|AppError::Internal)?.ok_or_else(||AppError::NotFound("terminal not found".into()))?;
    let exact: TerminalIdentity =
        serde_json::from_str(row.get::<&str, _>(0)).map_err(|_| AppError::Internal)?;
    if exact != *terminal || view.session != terminal.session {
        return Err(AppError::Conflict("terminal identity changed".into()));
    }
    if row.get::<String, _>(10) != view.execution_spec_digest
        || view
            .launch_manifest_digest
            .as_ref()
            .is_none_or(|d| d.as_str() != row.get::<&str, _>(11))
    {
        return Err(AppError::Conflict("terminal source seal changed".into()));
    }
    let revision: i64 = row.get(2);
    if revision < 0 {
        return Err(AppError::Internal);
    }
    let sealed = Sealed {
        version: row.get::<i32, _>(4) as u32,
        key_id: row.get(5),
        nonce: row.get(6),
        ciphertext: row.get(7),
    };
    let plain = configured(state)?
        .keyring
        .open(
            &Context {
                terminal: terminal.clone(),
                ledger_revision: revision as u64,
                purpose: Purpose::Ledger,
            },
            &sealed,
        )
        .map_err(|_| AppError::Internal)?;
    let ledger = Ledger::load(&plain).map_err(relay)?;
    let summary = ledger.summary();
    if view.terminal_policy.as_ref() != Some(ledger.terminal_policy()) {
        return Err(AppError::Conflict("terminal policy seal mismatch".into()));
    }
    let authority_revision: i64 = row.get(1);
    let principal = summary
        .control
        .lease
        .as_ref()
        .map(|c| Uuid::parse_str(c.principal_id.as_str()))
        .transpose()
        .map_err(|_| AppError::Internal)?;
    if principal != row.get::<Option<Uuid>, _>(15) {
        return Err(AppError::Internal);
    }
    if summary.terminal != *terminal
        || summary.session_revision != authority_revision as u64
        || summary.ledger_revision != revision as u64
        || summary.input_mode.key() != row.get::<&str, _>(12)
        || summary.generation != row.get::<i64, _>(13) as u64
        || summary.pending_bytes != row.get::<i32, _>(14) as usize
    {
        return Err(AppError::Internal);
    }
    Ok(LoadedLedger {
        ledger,
        runtime_id: row.get(3),
        peer_observed_at: row.get::<i64, _>(8) as u64,
        output_observed_at: row.get::<i64, _>(9) as u64,
        authority_revision: authority_revision as u64,
    })
}
pub async fn identity(state: &AppState, id: Uuid) -> Result<TerminalIdentity> {
    let body: Option<String> =
        sqlx::query_scalar("SELECT identity::text FROM terminal_sessions WHERE terminal_id=$1")
            .bind(id)
            .fetch_optional(&state.store.pool)
            .await
            .map_err(|_| AppError::Internal)?;
    serde_json::from_str(&body.ok_or_else(|| AppError::NotFound("terminal not found".into()))?)
        .map_err(|_| AppError::Internal)
}
#[derive(Clone)]
pub struct Operation {
    pub id: Uuid,
    pub principal: Uuid,
    pub digest: String,
    pub kind: &'static str,
    pub response: MutationReply,
}
pub async fn replay(
    tx: &mut Transaction<'_, Postgres>,
    terminal: Uuid,
    id: Uuid,
    principal: Uuid,
    request_digest: &str,
) -> Result<Option<MutationReply>> {
    let row=sqlx::query("SELECT principal_id,request_digest,response::text FROM terminal_operations WHERE terminal_id=$1 AND operation_id=$2").bind(terminal).bind(id).fetch_optional(&mut **tx).await.map_err(|_|AppError::Internal)?;
    row.map(|r| {
        if r.get::<Uuid, _>(0) != principal || r.get::<&str, _>(1) != request_digest {
            return Err(AppError::Conflict(
                "terminal operation identity conflict".into(),
            ));
        }
        serde_json::from_str(r.get::<&str, _>(2)).map_err(|_| AppError::Internal)
    })
    .transpose()
}
pub async fn save_operation(
    tx: &mut Transaction<'_, Postgres>,
    terminal: Uuid,
    op: &Operation,
    clock: u64,
) -> Result<()> {
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM terminal_operations WHERE terminal_id=$1")
            .bind(terminal)
            .fetch_one(&mut **tx)
            .await
            .map_err(|_| AppError::Internal)?;
    let bytes:i64=sqlx::query_scalar("SELECT coalesce(sum(octet_length(response::text)),0)::bigint FROM terminal_operations WHERE terminal_id=$1").bind(terminal).fetch_one(&mut **tx).await.map_err(|_|AppError::Internal)?;
    let response = serde_json::to_string(&op.response).map_err(|_| AppError::Internal)?;
    if count >= 1024 || bytes + response.len() as i64 > 4194304 {
        return Err(AppError::IdentityUnavailable(
            "terminal receipt capacity exhausted".into(),
        ));
    }
    sqlx::query("INSERT INTO terminal_operations(terminal_id,operation_id,principal_id,request_digest,kind,response,created_at_ms) VALUES($1,$2,$3,$4,$5,$6::jsonb,$7)").bind(terminal).bind(op.id).bind(op.principal).bind(&op.digest).bind(op.kind).bind(response).bind(dbtime(clock)?).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
    Ok(())
}
fn effect_meta(effect: &Effect) -> (Uuid, &'static str, u64, Option<u64>, bool) {
    match effect {
        Effect::Fence(v) => (
            v.fence_id,
            "fence",
            v.generation,
            v.controller.as_ref().map(|c| c.expires_at_ms),
            false,
        ),
        Effect::Capture(v) => (
            v.capture_request_id,
            "capture",
            v.controller_generation,
            Some(v.expires_at_ms),
            false,
        ),
        Effect::Input(v) => (
            v.operation_id,
            "input",
            v.lease.generation,
            Some(v.deadline_ms),
            true,
        ),
        Effect::EpochInput(v) => (
            v.input.operation_id,
            "epoch_input",
            v.input.lease.generation,
            Some(v.input.deadline_ms),
            true,
        ),
        Effect::Resize(v) => (
            v.operation_id,
            "resize",
            v.lease.generation,
            Some(v.deadline_ms),
            false,
        ),
        Effect::Watch(v) => (v.subscription_id, "watch", 0, Some(v.expires_at_ms), false),
    }
}
pub struct SqlJournal<'a> {
    pub state: &'a AppState,
    pub tx: Option<Transaction<'static, Postgres>>,
    pub operation: Option<Operation>,
    pub peer_contact: bool,
    pub recovering: bool,
}
impl AsyncJournal for SqlJournal<'_> {
    fn commit<'a>(
        &'a mut self,
        terminal: &'a TerminalIdentity,
        expected: u64,
        next: &'a [u8],
        effects: &'a [Effect],
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = aq_terminal_relay::Result<()>> + Send + 'a>,
    > {
        Box::pin(async move {
            self.persist(terminal, expected, next, effects)
                .await
                .map_err(|_| aq_terminal_relay::Error::Persistence)
        })
    }
}
impl SqlJournal<'_> {
    async fn persist(
        &mut self,
        t: &TerminalIdentity,
        expected: u64,
        next: &[u8],
        effects: &[Effect],
    ) -> Result<()> {
        let config = configured(self.state)?;
        let ledger = Ledger::load(next).map_err(relay)?;
        let summary = ledger.summary();
        let clock = now(self.state);
        let tx = self.tx.as_mut().ok_or(AppError::Internal)?;
        if summary.terminal != *t {
            return Err(AppError::Internal);
        }
        let sealed = config
            .keyring
            .seal(
                &Context {
                    terminal: t.clone(),
                    ledger_revision: summary.ledger_revision,
                    purpose: Purpose::Ledger,
                },
                next,
            )
            .map_err(|_| AppError::Internal)?;
        let principal = summary
            .control
            .lease
            .as_ref()
            .map(|c| Uuid::parse_str(c.principal_id.as_str()))
            .transpose()
            .map_err(|_| AppError::Internal)?;
        let changed=sqlx::query("UPDATE terminal_sessions SET authority_revision=$3,ledger_revision=$4,runtime_id=$5,protection_version=$6,key_id=$7,nonce=$8,ciphertext=$9,controller_generation=$10,controller_principal=$11,pending_bytes=$12,operation_count=$13,observed_at_ms=CASE WHEN $14 THEN $15 ELSE observed_at_ms END WHERE terminal_id=$1 AND ledger_revision=$2")
   .bind(t.terminal_id).bind(dbtime(expected)?).bind(dbtime(summary.session_revision)?).bind(dbtime(summary.ledger_revision)?).bind(config.runtime_id).bind(sealed.version as i32).bind(&sealed.key_id).bind(&sealed.nonce).bind(&sealed.ciphertext).bind(dbtime(summary.generation)?).bind(principal).bind(summary.pending_bytes as i32).bind(summary.mutation_count as i32).bind(self.peer_contact).bind(dbtime(clock)?).execute(&mut **tx).await.map_err(|_|AppError::Internal)?.rows_affected();
        if changed != 1 {
            return Err(AppError::Conflict(
                "terminal ledger revision changed".into(),
            ));
        }
        let metadata = ledger.input_metadata();
        let meta = serde_json::to_string(&metadata).map_err(|_| AppError::Internal)?;
        sqlx::query("DELETE FROM terminal_input_receipts WHERE terminal_id=$1 AND NOT(operation_id=ANY($2))").bind(t.terminal_id).bind(metadata.iter().map(|v|v.operation_id).collect::<Vec<_>>()).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
        sqlx::query("INSERT INTO terminal_input_receipts(terminal_id,operation_id,principal_id,request_digest,command_digest,disposition,deadline_ms,dispatched,byte_count,admission_revision,epoch_id,updated_at_ms) SELECT $1,(x->>'operation_id')::uuid,(x->>'principal')::uuid,x->>'request_digest',x->>'command_digest',x->>'disposition',(x->>'deadline_ms')::bigint,(x->>'dispatched')::boolean,(x->>'byte_count')::integer,(x->>'admission_revision')::bigint,(x->>'epoch_id')::uuid,$3 FROM jsonb_array_elements($2::jsonb) x ON CONFLICT(terminal_id,operation_id) DO UPDATE SET principal_id=excluded.principal_id,request_digest=excluded.request_digest,command_digest=excluded.command_digest,disposition=excluded.disposition,deadline_ms=excluded.deadline_ms,dispatched=excluded.dispatched,byte_count=excluded.byte_count,admission_revision=excluded.admission_revision,epoch_id=excluded.epoch_id,updated_at_ms=CASE WHEN terminal_input_receipts.disposition IS DISTINCT FROM excluded.disposition OR terminal_input_receipts.dispatched IS DISTINCT FROM excluded.dispatched THEN excluded.updated_at_ms ELSE terminal_input_receipts.updated_at_ms END")
   .bind(t.terminal_id).bind(meta).bind(dbtime(clock)?).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
        let inflight: Vec<_> = metadata
            .iter()
            .filter(|m| {
                m.dispatched
                    && matches!(
                        m.disposition,
                        aq_session_protocol::InputDisposition::Queued
                            | aq_session_protocol::InputDisposition::Received
                            | aq_session_protocol::InputDisposition::Injecting
                    )
            })
            .map(|m| m.operation_id)
            .collect();
        sqlx::query("DELETE FROM terminal_outbox WHERE terminal_id=$1 AND kind IN('input','epoch_input') AND NOT(operation_id=ANY($2))").bind(t.terminal_id).bind(inflight).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
        sqlx::query("DELETE FROM terminal_outbox WHERE terminal_id=$1 AND kind IN('watch','capture','resize') AND (acknowledged OR expires_at_ms<=$2 OR(kind='resize' AND controller_generation<$3))").bind(t.terminal_id).bind(dbtime(clock)?).bind(dbtime(summary.generation)?).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
        if let SessionIdentity::Remote { node_id, .. } = t.session {
            if summary.control.lease.is_some() {
                let controllers:i64=sqlx::query_scalar("SELECT count(DISTINCT s.terminal_id) FROM terminal_sessions s JOIN terminal_outbox o ON o.terminal_id=s.terminal_id WHERE s.node_id=$1 AND s.terminal_id<>$2 AND o.kind='fence' AND o.controller_generation=s.controller_generation AND (o.expires_at_ms>$3 OR(o.expires_at_ms IS NULL AND NOT o.acknowledged))").bind(node_id).bind(t.terminal_id).bind(dbtime(clock)?).fetch_one(&mut **tx).await.map_err(|_|AppError::Internal)?;
                if controllers >= 32 {
                    return Err(AppError::IdentityUnavailable(
                        "node terminal controller capacity exhausted".into(),
                    ));
                }
            }
            let other:i64=sqlx::query_scalar("SELECT count(*) FROM terminal_subscriptions WHERE node_id=$1 AND terminal_id<>$2 AND expires_at_ms>$3").bind(node_id).bind(t.terminal_id).bind(dbtime(clock)?).fetch_one(&mut **tx).await.map_err(|_|AppError::Internal)?;
            if other
                + summary
                    .subscriptions
                    .iter()
                    .filter(|s| clock < s.expires_at_ms)
                    .count() as i64
                > 32
            {
                return Err(AppError::IdentityUnavailable(
                    "node terminal subscription capacity exhausted".into(),
                ));
            }
        }
        sqlx::query("DELETE FROM terminal_subscriptions WHERE terminal_id=$1")
            .bind(t.terminal_id)
            .execute(&mut **tx)
            .await
            .map_err(|_| AppError::Internal)?;
        if !summary.subscriptions.is_empty() {
            let body =
                serde_json::to_string(&summary.subscriptions).map_err(|_| AppError::Internal)?;
            let SessionIdentity::Remote { node_id, .. } = t.session else {
                return Err(AppError::Conflict(
                    "central terminal authority unavailable".into(),
                ));
            };
            sqlx::query("INSERT INTO terminal_subscriptions(terminal_id,subscription_id,principal_id,node_id,expires_at_ms) SELECT $1,(x->>'subscription_id')::uuid,(x->>'principal_id')::uuid,$3,(x->>'expires_at_ms')::bigint FROM jsonb_array_elements($2::jsonb) x").bind(t.terminal_id).bind(body).bind(node_id).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
        }
        if self.recovering {
            sqlx::query("DELETE FROM terminal_outbox WHERE terminal_id=$1 AND kind IN('input','epoch_input','capture','resize','watch')").bind(t.terminal_id).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
            sqlx::query("UPDATE terminal_outbox SET acknowledged=FALSE WHERE terminal_id=$1 AND kind='fence' AND controller_generation=$2").bind(t.terminal_id).bind(dbtime(summary.generation)?).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
        }
        for (ordinal, effect) in effects.iter().enumerate() {
            let (id, kind, generation, expires, claimed) = effect_meta(effect);
            if matches!(effect, Effect::Fence(_)) {
                sqlx::query("DELETE FROM terminal_outbox WHERE terminal_id=$1 AND kind='fence' AND controller_generation<$2").bind(t.terminal_id).bind(dbtime(generation)?).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
            }
            let body = serde_json::to_vec(effect).map_err(|_| AppError::Internal)?;
            let sealed = config
                .keyring
                .seal(
                    &Context {
                        terminal: t.clone(),
                        ledger_revision: summary.ledger_revision,
                        purpose: Purpose::Outbox {
                            operation_id: id,
                            ordinal: ordinal as u32,
                        },
                    },
                    &body,
                )
                .map_err(|_| AppError::Internal)?;
            sqlx::query("INSERT INTO terminal_outbox(terminal_id,operation_id,kind,controller_generation,ledger_revision,expires_at_ms,claimed,protection_version,key_id,nonce,ciphertext,ordinal) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12)").bind(t.terminal_id).bind(id).bind(kind).bind(dbtime(generation)?).bind(dbtime(summary.ledger_revision)?).bind(expires.map(dbtime).transpose()?).bind(claimed).bind(sealed.version as i32).bind(sealed.key_id).bind(sealed.nonce).bind(sealed.ciphertext).bind(ordinal as i32).execute(&mut **tx).await.map_err(|_|AppError::Internal)?;
        }
        let counts:(i64,i64)=sqlx::query_as("SELECT count(*),coalesce(sum(octet_length(ciphertext)),0)::bigint FROM terminal_outbox WHERE terminal_id=$1").bind(t.terminal_id).fetch_one(&mut **tx).await.map_err(|_|AppError::Internal)?;
        if counts.0 > 128 || counts.1 > 262144 {
            return Err(AppError::IdentityUnavailable(
                "terminal outbox capacity exhausted".into(),
            ));
        }
        if let Some(op) = &self.operation {
            save_operation(tx, t.terminal_id, op, clock).await?;
        }
        self.tx
            .take()
            .ok_or(AppError::Internal)?
            .commit()
            .await
            .map_err(|_| AppError::Internal)?;
        Ok(())
    }
}

/// Explicit runtime publication, not a launch/controller grant. A replacement
/// owner waits for current terminal transactions, then old instances fail closed.
pub async fn install(state: &AppState) -> Result<()> {
    let state = state.terminal_view();
    let config = configured(&state)?;
    if config.runtime_id.is_nil() {
        return Err(AppError::BadRequest(
            "invalid terminal runtime owner".into(),
        ));
    }
    let mut tx = state
        .store
        .pool
        .begin()
        .await
        .map_err(|_| AppError::Internal)?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await
        .map_err(|_| AppError::Internal)?;
    sqlx::query("SET LOCAL statement_timeout='3s'")
        .execute(&mut *tx)
        .await
        .map_err(|_| AppError::Internal)?;
    sqlx::query("INSERT INTO meta(key,value) VALUES('terminal_relay_owner',$1) ON CONFLICT(key) DO UPDATE SET value=excluded.value").bind(config.runtime_id.to_string()).execute(&mut *tx).await.map_err(|_|AppError::Internal)?;
    tx.commit().await.map_err(|_| AppError::Internal)
}
pub async fn require_owner(state: &AppState, tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    owner(state, tx, Rows::Lock).await
}
async fn owner(state: &AppState, tx: &mut Transaction<'_, Postgres>, rows: Rows) -> Result<()> {
    let sql = rows.sql(
        "SELECT value FROM meta WHERE key='terminal_relay_owner'",
        "FOR SHARE",
    );
    let current: Option<String> = sqlx::query_scalar(&sql)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|_| AppError::Internal)?;
    if current.as_deref() != Some(configured(state)?.runtime_id.to_string().as_str()) {
        return Err(AppError::IdentityUnavailable(
            "terminal runtime owner changed or not installed".into(),
        ));
    }
    Ok(())
}

/// Canonical terminal lock prefix: installed owner -> common node/attempt ->
/// current users/ACL -> terminal ledger -> deterministic child rows.
pub async fn begin(state: &AppState) -> Result<Transaction<'static, Postgres>> {
    let mut tx = state
        .store
        .pool
        .begin()
        .await
        .map_err(|_| AppError::Internal)?;
    require_owner(state, &mut tx).await?;
    Ok(tx)
}
/// M1: a human read's transaction: a read-only snapshot that takes no row locks.
pub async fn begin_read(state: &AppState) -> Result<Transaction<'static, Postgres>> {
    let snapshot = crate::execution::authority::snapshot(&state.store.pool).await;
    let mut tx = snapshot.map_err(crate::error::db)?;
    owner(state, &mut tx, Rows::Snapshot).await?;
    Ok(tx)
}
