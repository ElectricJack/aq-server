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
pub use aq_terminal_relay::mutation::Right;
pub struct Scopes {
    pub inspect: Option<Access>,
    pub control: Option<Access>,
    pub both: Option<Access>,
}
pub struct AccessSubject<'a> {
    view: &'a AuthorityView,
    terminal: &'a TerminalIdentity,
    actor: Uuid,
}
impl<'a> AccessSubject<'a> {
    pub fn new(view: &'a AuthorityView, terminal: &'a TerminalIdentity, actor: Uuid) -> Self {
        Self {
            view,
            terminal,
            actor,
        }
    }
    pub async fn scopes(
        &self,
        state: &AppState,
        tx: &mut Transaction<'_, Postgres>,
    ) -> Result<Scopes> {
        let role = session_access::current_role(tx, self.actor, self.view.rows).await?;
        let inspection =
            match session_access::require_inspect_locked(state, tx, self.view, self.actor).await {
                Ok(v) => Some(v),
                Err(AppError::Forbidden(_)) => None,
                Err(e) => return Err(e),
            };
        let row=sqlx::query(&self.view.rows.sql("SELECT can_control,expires_at_ms FROM terminal_control_acl WHERE terminal_id=$1 AND principal_id=$2", "FOR SHARE")).bind(self.terminal.terminal_id).bind(self.actor).fetch_optional(&mut **tx).await.map_err(|_|AppError::Internal)?;
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
        let principal = Text::new(self.actor.to_string()).map_err(|_| AppError::Internal)?;
        let make = |until: Option<u64>| {
            until.map(|valid_until_ms| Access {
                principal: principal.clone(),
                terminal: self.terminal.clone(),
                session_revision: self.view.authority_revision,
                watch: inspect_until.is_some(),
                control: control_until.is_some(),
                valid_until_ms,
                execution_active: self.view.active_at(checked).is_ok(),
                execution_expires_at_ms: self.view.grant_expires_at_ms,
            })
        };
        Ok(Scopes {
            inspect: make(inspect_until),
            control: make(control_until),
            both: make(inspect_until.zip(control_until).map(|(a, b)| a.min(b))),
        })
    }
    pub async fn authorize(
        &self,
        state: &AppState,
        tx: &mut Transaction<'_, Postgres>,
        right: Right,
    ) -> Result<Access> {
        let scopes = self.scopes(state, tx).await?;
        match right {
            Right::Inspect => scopes.inspect,
            Right::Control => scopes.control,
            Right::Both => scopes.both,
        }
        .ok_or_else(|| AppError::Forbidden("terminal scope denied".into()))
    }
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
    require_sealed_identity(&row, terminal, view)?;
    let ledger = open_ledger(state, &row, terminal)?;
    require_metadata(&row, terminal, view, &ledger)?;
    Ok(LoadedLedger {
        ledger,
        runtime_id: row.get(3),
        peer_observed_at: row.get::<i64, _>(8) as u64,
        output_observed_at: row.get::<i64, _>(9) as u64,
        authority_revision: row.get::<i64, _>(1) as u64,
    })
}
fn require_sealed_identity(
    row: &sqlx::postgres::PgRow,
    terminal: &TerminalIdentity,
    view: &AuthorityView,
) -> Result<()> {
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
    Ok(())
}
fn open_ledger(
    state: &AppState,
    row: &sqlx::postgres::PgRow,
    terminal: &TerminalIdentity,
) -> Result<Ledger> {
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
    Ledger::load(&plain).map_err(relay)
}
fn require_metadata(
    row: &sqlx::postgres::PgRow,
    terminal: &TerminalIdentity,
    view: &AuthorityView,
    ledger: &Ledger,
) -> Result<()> {
    let summary = ledger.summary();
    if view.terminal_policy.as_ref() != Some(ledger.terminal_policy()) {
        return Err(AppError::Conflict("terminal policy seal mismatch".into()));
    }
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
    let stored_version = (row.get::<i64, _>(1) as u64, row.get::<i64, _>(2) as u64);
    let ledger_version = (summary.session_revision, summary.ledger_revision);
    let stored_counts = (row.get::<i64, _>(13) as u64, row.get::<i32, _>(14) as usize);
    let ledger_counts = (summary.generation, summary.pending_bytes);
    let same_identity =
        summary.terminal == *terminal && summary.input_mode.key() == row.get::<&str, _>(12);
    let same_state = stored_version == ledger_version && stored_counts == ledger_counts;
    if !same_identity || !same_state {
        return Err(AppError::Internal);
    }
    Ok(())
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
pub struct RequestKey<'a> {
    pub id: Uuid,
    pub principal: Uuid,
    pub digest: &'a str,
}
pub async fn replay(
    tx: &mut Transaction<'_, Postgres>,
    terminal: Uuid,
    request: RequestKey<'_>,
) -> Result<Option<MutationReply>> {
    let row=sqlx::query("SELECT principal_id,request_digest,response::text FROM terminal_operations WHERE terminal_id=$1 AND operation_id=$2").bind(terminal).bind(request.id).fetch_optional(&mut **tx).await.map_err(|_|AppError::Internal)?;
    row.map(|r| {
        if r.get::<Uuid, _>(0) != request.principal || r.get::<&str, _>(1) != request.digest {
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
mod journal;
mod writes;
pub use journal::SqlJournal;

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
