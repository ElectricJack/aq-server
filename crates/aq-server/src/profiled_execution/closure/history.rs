//! Immutable semantic registration snapshots. Operation IDs, inventory and
//! configuration/timestamp refreshes are not incarnation identity.
use super::{canonical, conflict, database};
use crate::{
    error::AppError,
    profiled_execution::registration::{self, NativeRegistration},
};
use aq_admission::{Digest, LaunchBinding, SessionIdentity, transport::TransportDocument};
use aq_session_protocol::Validate;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;
pub(crate) struct RegistrationHistory {
    pub id: Uuid,
    pub metadata: NativeRegistration,
}
fn validate(metadata: &NativeRegistration) -> Result<(), AppError> {
    TransportDocument::Register(metadata.request.clone())
        .validate()
        .map_err(|_| conflict())?;
    for text in [&metadata.qualification.id, &metadata.qualification.version] {
        text.validate().map_err(|_| conflict())?;
    }
    if metadata.schema_version != 1
        || metadata.configuration_revision == 0
        || metadata.configuration_revision > i64::MAX as u64
        || metadata.registered_at_ms == 0
        || metadata.registered_at_ms > i64::MAX as u64
        || metadata.semantic_digest != registration::semantic_digest(&metadata.request)?
    {
        return Err(conflict());
    }
    Ok(())
}
fn identity(metadata: &NativeRegistration) -> Result<Digest, AppError> {
    validate(metadata)?;
    aq_selection::digest(&serde_json::json!({
        "identity": metadata.request.identity,
        "semantic_registration": metadata.semantic_digest,
        "runtime": metadata.request.runtime,
        "qualification": metadata.qualification,
    }))
    .map_err(|_| AppError::Internal)
}
fn decode(row: sqlx::postgres::PgRow) -> Result<RegistrationHistory, AppError> {
    let bytes: Vec<u8> = row.get("metadata_bytes");
    if bytes.is_empty()
        || bytes.len() > 65536
        || Digest::of(&bytes).as_str() != row.get::<String, _>("metadata_digest")
    {
        return Err(conflict());
    }
    let metadata: NativeRegistration = serde_json::from_slice(&bytes).map_err(|_| conflict())?;
    validate(&metadata)?;
    let key = identity(&metadata)?;
    let id = Uuid::new_v5(&metadata.request.identity.node_id, key.as_str().as_bytes());
    if canonical(&metadata)? != bytes
        || row.get::<Uuid, _>("registration_id") != id
        || row.get::<Uuid, _>("node_id") != metadata.request.identity.node_id
        || row.get::<Uuid, _>("boot_id") != metadata.request.identity.boot_id
        || row.get::<String, _>("identity_digest") != key.as_str()
        || row.get::<String, _>("semantic_digest") != metadata.semantic_digest.as_str()
    {
        return Err(conflict());
    }
    Ok(RegistrationHistory { id, metadata })
}
pub(crate) async fn load_locked(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
) -> Result<RegistrationHistory, AppError> {
    let row = sqlx::query(
        "SELECT * FROM profiled_native_registration_history WHERE registration_id=$1 FOR SHARE",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(database)?
    .ok_or_else(conflict)?;
    decode(row)
}
/// Caller holds the current enrolled-node lock. Existing semantic identity is
/// compared without requiring a retry's operation/revision/time to be identical.
pub(crate) async fn capture_locked(
    tx: &mut Transaction<'_, Postgres>,
    metadata: &NativeRegistration,
) -> Result<RegistrationHistory, AppError> {
    let key = identity(metadata)?;
    let id = Uuid::new_v5(&metadata.request.identity.node_id, key.as_str().as_bytes());
    if let Some(row) = sqlx::query(
        "SELECT * FROM profiled_native_registration_history WHERE registration_id=$1 FOR SHARE",
    )
    .bind(id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(database)?
    {
        let old = decode(row)?;
        if identity(&old.metadata)? != key {
            return Err(conflict());
        }
        return Ok(old);
    }
    let bytes = canonical(metadata)?;
    if bytes.len() > 65536 {
        return Err(conflict());
    }
    // No quota (architecture 0006): row retention keeps each node's newest rows.
    sqlx::query(
        "INSERT INTO profiled_native_registration_history(registration_id,node_id,boot_id,\
         identity_digest,semantic_digest,metadata_bytes) VALUES($1,$2,$3,$4,$5,$6)",
    )
    .bind(id)
    .bind(metadata.request.identity.node_id)
    .bind(metadata.request.identity.boot_id)
    .bind(key.as_str())
    .bind(metadata.semantic_digest.as_str())
    .bind(bytes)
    .execute(&mut **tx)
    .await
    .map_err(database)?;
    load_locked(tx, id).await
}
/// Existing links are authoritative. A missing older link may only capture the
/// actual still-held original boot under the caller's node→attempt/hot locks.
pub(crate) async fn for_attempt_locked(
    tx: &mut Transaction<'_, Postgres>,
    binding: &LaunchBinding,
) -> Result<RegistrationHistory, AppError> {
    let SessionIdentity::Remote {
        node_id, boot_id, ..
    } = binding.session
    else {
        return Err(conflict());
    };
    if let Some(history) = linked_attempt_locked(tx, binding.attempt_id, node_id, boot_id).await? {
        return Ok(history);
    }
    let held: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM execution_attempts WHERE attempt_id=$1 AND node_id=$2 AND \
         kind='profiled_v1' AND holds_task)",
    )
    .bind(binding.attempt_id)
    .bind(node_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(database)?;
    if !held {
        return Err(conflict());
    }
    let current = registration::admitted_runtime_locked(tx, node_id, boot_id).await?;
    let history = capture_locked(tx, &current).await?;
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended(\
         'aq-native-attempt-registration-link-v1',0))",
    )
    .execute(&mut **tx)
    .await
    .map_err(database)?;
    check_link_capacity_locked(tx, node_id).await?;
    sqlx::query(
        "INSERT INTO profiled_launch_registrations(attempt_id,registration_id,node_id,boot_id) \
         VALUES($1,$2,$3,$4)",
    )
    .bind(binding.attempt_id)
    .bind(history.id)
    .bind(node_id)
    .bind(boot_id)
    .execute(&mut **tx)
    .await
    .map_err(database)?;
    Ok(history)
}

async fn linked_attempt_locked(
    tx: &mut Transaction<'_, Postgres>,
    attempt: Uuid,
    node: Uuid,
    boot: Uuid,
) -> Result<Option<RegistrationHistory>, AppError> {
    let row = sqlx::query(
        "SELECT registration_id,node_id,boot_id FROM profiled_launch_registrations WHERE \
         attempt_id=$1 FOR SHARE",
    )
    .bind(attempt)
    .fetch_optional(&mut **tx)
    .await
    .map_err(database)?;
    let Some(row) = row else {
        return Ok(None);
    };
    if row.get::<Uuid, _>("node_id") != node || row.get::<Uuid, _>("boot_id") != boot {
        return Err(conflict());
    }
    let history = load_locked(tx, row.get("registration_id")).await?;
    if history.metadata.request.identity.node_id != node
        || history.metadata.request.identity.boot_id != boot
    {
        return Err(conflict());
    }
    Ok(Some(history))
}

async fn check_link_capacity_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
) -> Result<(), AppError> {
    let kind = aq_history_caps::Kind::LaunchRegistrations;
    let usage = aq_history_caps::usage(tx, kind, Some(node), 0)
        .await
        .map_err(database)?;
    if usage.total_rows >= 4096
        || usage.total_bytes + 64 > 262144
        || usage.scope_rows >= 256
        || usage.scope_bytes + 64 > 16384
    {
        return Err(AppError::Conflict(
            "native attempt registration link capacity exhausted".into(),
        ));
    }
    Ok(())
}

/// A superseded boot supplies terminal evidence only, never current launch authority.
pub(crate) async fn for_boot_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    boot: Uuid,
) -> Result<NativeRegistration, AppError> {
    let rows = sqlx::query(
        "SELECT * FROM profiled_native_registration_history WHERE node_id=$1 AND boot_id=$2 \
         ORDER BY registration_id LIMIT 2 FOR SHARE",
    )
    .bind(node)
    .bind(boot)
    .fetch_all(&mut **tx)
    .await
    .map_err(database)?;
    if rows.len() != 1 {
        return Err(conflict());
    }
    Ok(decode(rows.into_iter().next().ok_or_else(conflict)?)?.metadata)
}
