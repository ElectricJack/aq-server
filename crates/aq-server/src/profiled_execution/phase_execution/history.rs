//! Immutable phase execution issuance; this is not a live permission wrapper.
pub(super) use super::Provenance;
use super::accepted_owner::{self, AcceptedHistory};
use super::phase::{ExecutionReceipt, PrepareRequest};
use super::*;
use crate::profiled_execution::initial_grant;
use aq_admission::{LaunchBinding, MaterialReceipt};
use serde::Serialize;
use sqlx::Row;
use uuid::Uuid;
pub(crate) struct PhaseExecutionHistory {
    pub accepted: AcceptedHistory,
    pub request: PrepareRequest,
    pub receipt: ExecutionReceipt,
    pub receipt_digest: Digest,
    pub provenance: Provenance,
    pub installation: MaterialReceipt,
}
fn decode<T: serde::de::DeserializeOwned + Serialize>(
    bytes: &[u8],
    expected: &str,
) -> Result<T, AppError> {
    if bytes.is_empty() || bytes.len() > 131072 || Digest::of(bytes).as_str() != expected {
        return Err(refused());
    }
    let v: T = serde_json::from_slice(bytes).map_err(|_| refused())?;
    if canonical(&v)? != bytes {
        return Err(refused());
    }
    Ok(v)
}
pub(crate) async fn load_optional_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    binding: &LaunchBinding,
    grant: Uuid,
) -> Result<Option<PhaseExecutionHistory>, AppError> {
    let present: bool =
        sqlx::query_scalar("SELECT to_regclass('profiled_phase_execution_grants') IS NOT NULL")
            .fetch_one(&mut **tx)
            .await
            .map_err(db)?;
    if !present {
        let version: String =
            sqlx::query_scalar("SELECT value FROM meta WHERE key='state_format_version'")
                .fetch_one(&mut **tx)
                .await
                .map_err(db)?;
        if version.parse::<u32>().map_err(|_| refused())? >= 32 {
            return Err(refused());
        }
        return Ok(None);
    }
    let row =
        sqlx::query("SELECT * FROM profiled_phase_execution_grants WHERE attempt_id=$1 FOR SHARE")
            .bind(binding.attempt_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(db)?;
    let Some(row) = row else { return Ok(None) };
    let request: PrepareRequest = decode(
        &row.get::<Vec<u8>, _>("request_bytes"),
        &row.get::<String, _>("request_bytes_digest"),
    )?;
    let receipt: ExecutionReceipt = decode(
        &row.get::<Vec<u8>, _>("receipt_bytes"),
        &row.get::<String, _>("receipt_bytes_digest"),
    )?;
    let provenance: Provenance = decode(
        &row.get::<Vec<u8>, _>("provenance_bytes"),
        &row.get::<String, _>("provenance_digest"),
    )?;
    let receipt_digest = receipt.digest().map_err(|_| refused())?;
    let accepted = accepted_owner::load_accepted_history_locked(tx, binding).await?;
    receipt
        .validate_for(
            &request,
            &accepted.receipt,
            &accepted.request,
            &accepted.proposal,
        )
        .map_err(|_| refused())?;
    if row.get::<Uuid, _>("node_id") != node
        || row.get::<Uuid, _>("boot_id") != request.original.identity.boot_id
        || request.original.identity.node_id != node
        || receipt.grant.binding != *binding
        || receipt.grant.grant_id != grant
        || row.get::<Uuid, _>("grant_id") != grant
        || row.get::<Uuid, _>("operation_id") != request.original.operation_id
        || row.get::<String, _>("request_digest")
            != request.digest().map_err(|_| refused())?.as_str()
        || row.get::<String, _>("receipt_digest") != receipt_digest.as_str()
        || row.get::<String, _>("accepted_receipt_digest")
            != accepted.receipt.digest().map_err(|_| refused())?.as_str()
        || row.get::<i64, _>("issued_at_ms") as u64 != receipt.issued_at_ms
        || row.get::<i64, _>("expires_at_ms") as u64 != receipt.grant.expires_at_ms
        || row.get::<i64, _>("execution_deadline_ms") as u64 != receipt.execution_deadline_ms
        || row.get::<Uuid, _>("registration_id") != provenance.registration_id
        || provenance.schema_version != 2
        || provenance.inventory_compatibility_epoch == 0
        || provenance.availability.len() > 1024
        || provenance.private_bytes != accepted.receipt.lease.materials[0].material_bytes
        || provenance.private_digest != accepted.receipt.lease.materials[0].material_digest
    {
        return Err(refused());
    }
    super::validate_provenance_profile(&provenance, &receipt, &accepted)?;
    let registration =
        super::super::closure::history::load_locked(tx, provenance.registration_id).await?;
    if registration.metadata.request.identity != request.original.identity {
        return Err(refused());
    }
    let linked =
        crate::profiled_execution::source_exports::load_linked_locked(tx, binding.attempt_id)
            .await?;
    provenance.retention.validate_for(linked.token())?;
    provenance
        .graph
        .validate_at(receipt.issued_at_ms)
        .map_err(|_| refused())?;
    let installation = initial_grant::load_installed_receipt_locked(
        tx,
        &accepted.receipt.lease.materials[0],
        receipt.issued_at_ms,
    )
    .await?;
    if !same(&installation, &request.original.materials[0])? {
        return Err(refused());
    }
    Ok(Some(PhaseExecutionHistory {
        accepted,
        request,
        receipt,
        receipt_digest,
        provenance,
        installation,
    }))
}
