//! Current delegated authority. Immutable issuance remains in delegated_permits;
//! these records are reconstructed from PostgreSQL on every request.
use super::{CredentialKey, PermitRecord, storage};
use crate::{AppState, error::AppError, worker_api::AuthenticatedNode};
use aq_admission_sandbox::sandbox::*;
use axum::http::HeaderMap;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, Row, Transaction};
use std::result::Result;
use uuid::Uuid;

pub(crate) fn refused() -> AppError {
    AppError::Forbidden("delegated permit scope or authority unavailable; stop and preserve".into())
}
pub(crate) fn db(_: sqlx::Error) -> AppError {
    AppError::Internal
}
pub(crate) fn now(state: &AppState) -> Result<u64, AppError> {
    u64::try_from((state.clock)().timestamp_millis()).map_err(|_| refused())
}
pub(crate) fn json(value: &impl Serialize) -> Result<serde_json::Value, AppError> {
    serde_json::to_value(value).map_err(|_| AppError::Internal)
}
pub(crate) fn parse<T: serde::de::DeserializeOwned>(
    value: serde_json::Value,
) -> Result<T, AppError> {
    serde_json::from_value(value).map_err(|_| refused())
}
pub(crate) fn digest(value: &impl Serialize) -> Result<[u8; 32], AppError> {
    let mut value = json(value)?;
    value.sort_all_objects();
    Ok(Sha256::digest(serde_json::to_vec(&value).map_err(|_| AppError::Internal)?).into())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Current {
    pub record: PermitRecord,
    pub preparation: Option<PreparationAuthority>,
    pub authority: Option<PhaseExecutionAuthority>,
    pub revision: u64,
    #[serde(default)]
    pub registration: Option<Registration>,
}

/// Opaque evidence carried into every effect-owning transaction. No raw bearer
/// survives authentication; a changed scope, binding or revision invalidates it.
#[derive(Clone)]
pub struct Authentication {
    pub(crate) current: Current,
    operation: RunnerOperation,
    verifier: [u8; 32],
    clock: crate::Clock,
}
impl std::fmt::Debug for Authentication {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DelegatedAuthentication")
            .field("permit_id", &self.current.record.permit_id)
            .finish_non_exhaustive()
    }
}

pub(crate) async fn load(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> Result<Current, AppError> {
    let row=sqlx::query("SELECT c.state,p.revoked FROM delegated_permit_current c JOIN delegated_permits p USING(permit_id) WHERE c.permit_id=$1 FOR UPDATE OF c")
        .bind(id).fetch_optional(&mut **tx).await.map_err(db)?.ok_or_else(refused)?;
    if row.get::<bool, _>("revoked") {
        return Err(refused());
    }
    parse(row.get("state"))
}
pub(crate) async fn save(
    tx: &mut Transaction<'_, Postgres>,
    current: &Current,
) -> Result<(), AppError> {
    sqlx::query("INSERT INTO delegated_permit_current(permit_id,state) VALUES($1,$2) ON CONFLICT(permit_id) DO UPDATE SET state=excluded.state")
        .bind(current.record.permit_id).bind(json(current)?).execute(&mut **tx).await.map_err(db)?;
    Ok(())
}

/// Internal issuer seam. The caller must own and validate the grant transaction;
/// this function is never reachable from an arbitrary request body.
pub async fn install_execution_locked(
    tx: &mut Transaction<'_, Postgres>,
    operation: Uuid,
    request_digest: [u8; 32],
    permit: &DelegatedPermit,
    authority: &PhaseExecutionAuthority,
) -> Result<(), AppError> {
    authority
        .validate_handoff(permit, permit.issued_at_ms)
        .map_err(|_| refused())?;
    let record = storage::insert_locked(tx, operation, request_digest, permit)
        .await
        .map_err(|_| refused())?;
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM delegated_permit_current WHERE permit_id=$1)",
    )
    .bind(record.permit_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    if !exists {
        let registration = super::incarnation::registered(tx, permit.binding.node.node_id).await?;
        save(
            tx,
            &Current {
                record,
                preparation: None,
                authority: Some(authority.clone()),
                revision: 1,
                registration,
            },
        )
        .await?;
    }
    Ok(())
}

pub(crate) async fn boot(tx: &mut Transaction<'_, Postgres>, node: Uuid) -> Result<Uuid, AppError> {
    sqlx::query_scalar("SELECT COALESCE((SELECT boot_id FROM delegated_node_boots WHERE node_id=$1),(SELECT (state#>>'{identity,boot_id}')::uuid FROM execution_nodes WHERE node_id=$1))")
        .bind(node).fetch_one(&mut **tx).await.map_err(db)
}

pub(crate) async fn context(
    tx: &mut Transaction<'_, Postgres>,
    current: &Current,
) -> Result<PermitContext, AppError> {
    use crate::execution::{authority::hot::HotState, profiled::Status};
    let p = &current.record;
    let hot: HotState = parse(
        sqlx::query_scalar(
            "SELECT state FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE",
        )
        .bind(p.binding.attempt_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?
        .ok_or_else(refused)?,
    )?;
    if hot.revoked || hot.cancellation_id.is_some() {
        return Err(refused());
    }
    let (phase, expiry, hard) = match p.scope {
        PermitScope::Preparation => {
            let origin = current.preparation.as_ref().ok_or_else(refused)?;
            if hot.status != Status::Preparing
                || origin.binding.attempt_id != p.binding.attempt_id
                || origin.binding.assignment_generation != p.binding.grant_generation
            {
                return Err(refused());
            }
            (
                GrantPhase::Preparing,
                origin.preparation_deadline_ms,
                origin.preparation_deadline_ms,
            )
        }
        PermitScope::Execution => {
            let origin = current.authority.as_ref().ok_or_else(refused)?;
            let grant = hot.grant.as_ref().ok_or_else(refused)?;
            if !matches!(hot.status, Status::Granted | Status::Finished)
                || (hot.status == Status::Finished && !hot.stop.known_stopped())
                || (hot.status == Status::Granted && hot.stop.known_stopped())
                || grant.grant_id != origin.grant.grant_id
                || grant.binding != origin.grant.binding
                || grant.binding.attempt_id != p.binding.attempt_id
                || grant.binding.assignment_generation != p.binding.grant_generation
                || hot.execution_deadline_ms != Some(origin.hard_deadline_ms)
            {
                return Err(refused());
            }
            (
                if hot.status == Status::Finished {
                    GrantPhase::Ended
                } else {
                    GrantPhase::Granted
                },
                grant.expires_at_ms,
                origin.hard_deadline_ms,
            )
        }
    };
    Ok(PermitContext {
        binding: p.binding.clone(),
        phase,
        grant_expires_at_ms: expiry,
        hard_deadline_ms: hard,
    })
}

pub(crate) async fn lock_owner(
    tx: &mut Transaction<'_, Postgres>,
    current: &Current,
) -> Result<(), AppError> {
    let p = &current.record;
    let exists: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM nodes WHERE id=$1 AND revoked_at IS NULL FOR UPDATE")
            .bind(p.binding.node.node_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(db)?;
    if exists.is_none() {
        return Err(refused());
    }
    let exists: Option<Uuid> = sqlx::query_scalar(
        "SELECT attempt_id FROM execution_attempts WHERE attempt_id=$1 AND node_id=$2 FOR UPDATE",
    )
    .bind(p.binding.attempt_id)
    .bind(p.binding.node.node_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?;
    if exists.is_none() {
        return Err(refused());
    }
    Ok(())
}

pub async fn authenticate_runner(
    state: &AppState,
    headers: &HeaderMap,
    operation: RunnerOperation,
    attempt: Option<Uuid>,
) -> Result<AuthenticatedNode, AppError> {
    // Absent or non-permit credentials are unauthenticated (401) via the node
    // authenticator; only a presented permit can be refused for scope (403).
    let Some(token) = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .filter(|t| t.starts_with("aqp_"))
    else {
        return crate::worker_api::authenticate_context(state, headers).await;
    };
    let verifier: [u8; 32] = Sha256::digest(token.as_bytes()).into();
    let raw:serde_json::Value=sqlx::query_scalar("SELECT c.state FROM delegated_permit_current c JOIN delegated_permits p USING(permit_id) WHERE c.state#>'{record,verifier}'=$1 AND NOT p.revoked")
        .bind(json(&verifier)?).fetch_optional(&state.store.pool).await.map_err(db)?.ok_or_else(refused)?;
    let current: Current = parse(raw)?;
    current
        .record
        .verify_credential(token)
        .map_err(|_| refused())?;
    let channel: RunnerChannelIdentity =
        super::handoff::header(headers, "aq-runner-channel")?.ok_or_else(refused)?;
    if channel != current.record.binding || attempt.is_some_and(|a| a != channel.attempt_id) {
        return Err(refused());
    }
    // Material routes serve both preparation and execution with the same body.
    let operation = if operation == RunnerOperation::FetchPreparationMaterial
        && current.record.scope == PermitScope::Execution
    {
        RunnerOperation::FetchExecutionMaterial
    } else {
        operation
    };
    // An already-upgraded Prepared retry can only redeliver its exact retained
    // operation. The phase owner separately checks the frozen request bytes.
    let operation = if operation == RunnerOperation::Prepared
        && current.record.scope == PermitScope::Execution
    {
        let q: PermitUpgradeRequest =
            super::handoff::header(headers, "aq-permit-upgrade")?.ok_or_else(refused)?;
        if q.permit_id != current.record.permit_id || q.binding != current.record.binding {
            return Err(refused());
        }
        let retained:Option<Vec<u8>>=sqlx::query_scalar("SELECT request_digest FROM delegated_operations WHERE node_id=$1 AND operation_id=$2 AND kind='upgrade'")
            .bind(channel.node.node_id).bind(q.operation_id).fetch_optional(&state.store.pool).await.map_err(db)?;
        if retained.as_deref() != Some(digest(&q)?.as_slice()) {
            return Err(refused());
        }
        RunnerOperation::ReportResult
    } else {
        operation
    };
    let evidence = Authentication {
        current,
        operation,
        verifier,
        clock: state.clock.clone(),
    };
    let mut tx = state.store.pool.begin().await.map_err(db)?;
    sqlx::query("SET LOCAL lock_timeout='2s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("SET LOCAL statement_timeout='3s'")
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    lock_authenticated(&mut tx, &evidence).await?;
    tx.commit().await.map_err(db)?;
    let node = state
        .store
        .find_node_by_id(channel.node.node_id)
        .await?
        .ok_or_else(refused)?;
    Ok(AuthenticatedNode::from_delegated(node, evidence))
}

pub(crate) async fn lock_authenticated(
    tx: &mut Transaction<'_, Postgres>,
    auth: &Authentication,
) -> Result<Uuid, AppError> {
    lock_owner(tx, &auth.current).await?;
    let current = load(tx, auth.current.record.permit_id).await?;
    if current.revision != auth.current.revision
        || current.record.binding != auth.current.record.binding
        || current.record.scope != auth.current.record.scope
        || current.record.verifier() != auth.verifier
    {
        return Err(refused());
    }
    if boot(tx, current.record.binding.node.node_id).await? != current.record.binding.node.boot_id {
        return Err(refused());
    }
    let context = context(tx, &current).await?;
    super::incarnation::check_rebound(tx, &current).await?;
    let time = u64::try_from((auth.clock)().timestamp_millis()).map_err(|_| refused())?;
    current
        .record
        .authorize(&context, auth.operation, time)
        .map_err(|_| refused())?;
    Ok(current.record.binding.node.node_id)
}

/// Routes extract attempt IDs from their decoded types, never JSON field names.
/// Empty collections cannot establish delegated attempt scope. Terminal polls use
/// the permit's attempt in SQL; empty terminal uploads still authenticate.
pub fn require_attempts(
    auth: &AuthenticatedNode,
    attempts: impl IntoIterator<Item = Uuid>,
) -> Result<(), AppError> {
    let Some(delegate) = auth.delegated() else {
        return Ok(());
    };
    let expected = delegate.current.record.binding.attempt_id;
    let mut found = false;
    for attempt in attempts {
        found = true;
        if attempt != expected {
            return Err(refused());
        }
    }
    if found { Ok(()) } else { Err(refused()) }
}

/// A launch binding and its remote session each carry an attempt reference.
pub fn require_binding(
    auth: &AuthenticatedNode,
    binding: &aq_session_protocol::LaunchBinding,
) -> Result<(), AppError> {
    require_attempts(auth, [binding.attempt_id])?;
    require_session(auth, &binding.session)
}

pub fn require_session(
    auth: &AuthenticatedNode,
    session: &aq_session_protocol::SessionIdentity,
) -> Result<(), AppError> {
    let attempts = match session {
        aq_session_protocol::SessionIdentity::Remote { attempt_id, .. } => Some(*attempt_id),
        aq_session_protocol::SessionIdentity::Central { .. } => None,
    };
    require_attempts(auth, attempts)
}

/// Historical proof for terminal cleanup, never live runner authorization.
/// Revoked/expired credentials do not erase an actually committed extension.
pub(crate) async fn verify_extended_grant_locked(
    tx: &mut Transaction<'_, Postgres>,
    original: &aq_session_protocol::ProfiledGrant,
    live: &aq_session_protocol::ProfiledGrant,
    receipt_digest: &aq_admission::Digest,
    hard_deadline_ms: u64,
) -> Result<(), AppError> {
    if live.binding != original.binding
        || live.grant_id != original.grant_id
        || live.expires_at_ms <= original.expires_at_ms
        || live.expires_at_ms > hard_deadline_ms
    {
        return Err(refused());
    }
    let rows: Vec<serde_json::Value> = sqlx::query_scalar("SELECT c.state FROM delegated_permit_current c WHERE c.state#>>'{record,binding,attempt_id}'=$1 AND EXISTS(SELECT 1 FROM delegated_operations o WHERE o.node_id=(c.state#>>'{record,binding,node,node_id}')::uuid AND o.kind='renewal' AND o.evidence#>'{current,authority}'=c.state->'authority' AND o.evidence#>'{current,record,expires_at_ms}'=c.state#>'{record,expires_at_ms}' AND o.evidence#>'{current,record,permit_id}'=c.state#>'{record,permit_id}') LIMIT 2")
        .bind(live.binding.attempt_id.to_string()).fetch_all(&mut **tx).await.map_err(db)?;
    if rows.len() != 1 {
        return Err(refused());
    }
    let current: Current = parse(rows.into_iter().next().ok_or_else(refused)?)?;
    let origin = current.authority.ok_or_else(refused)?;
    if origin.grant != *original
        || origin.receipt_digest.as_str() != receipt_digest.as_str()
        || origin.hard_deadline_ms != hard_deadline_ms
        || current.record.scope != PermitScope::Execution
        || current.record.binding.attempt_id != original.binding.attempt_id
        || current.record.binding.grant_generation != original.binding.assignment_generation
        || current.record.expires_at_ms != live.expires_at_ms
        || current.record.hard_deadline_ms != hard_deadline_ms
    {
        return Err(refused());
    }
    Ok(())
}

pub(crate) fn key(state: &AppState) -> Result<&CredentialKey, AppError> {
    state
        .config
        .delegated_permits
        .as_deref()
        .ok_or_else(|| AppError::Maintenance("delegated permit key is not configured".into()))
}
