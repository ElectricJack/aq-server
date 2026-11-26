//! Immutable phase execution issuance; this is not a live permission wrapper.
pub(super) use super::Provenance;
use super::accepted_owner::{self, AcceptedHistory};
use super::phase::{ExecutionReceipt, PrepareRequest};
use super::*;
use crate::profiled_execution::{initial_grant, on_heap};
use aq_admission::{LaunchBinding, MaterialReceipt};
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
pub(crate) async fn load_optional_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    binding: &LaunchBinding,
    grant: Uuid,
) -> Result<Option<PhaseExecutionHistory>, AppError> {
    // Result recording and settlement await this inline through several
    // owners. Keeping the decoded documents on the heap stops every caller's
    // future and unoptimized poll frame from carrying a copy of them.
    on_heap(|| load_documents_locked(tx, node, binding, grant)).await
}

async fn load_documents_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    binding: &LaunchBinding,
    grant: Uuid,
) -> Result<Option<PhaseExecutionHistory>, AppError> {
    if !aq_profiled_state::phase_execution::history_storage_available(tx)
        .await
        .map_err(helper_error)?
    {
        return Ok(None);
    }
    let row =
        sqlx::query("SELECT * FROM profiled_phase_execution_grants WHERE attempt_id=$1 FOR SHARE")
            .bind(binding.attempt_id)
            .fetch_optional(&mut **tx)
            .await
            .map_err(db)?;
    let Some(row) = row else { return Ok(None) };
    let documents: stored::IssuedDocuments<Provenance> =
        stored::IssuedDocuments::decode_row(&row).map_err(helper_error)?;
    let accepted = accepted_owner::load_accepted_history_locked(tx, binding).await?;
    documents
        .validate_row(
            &row,
            node,
            binding,
            grant,
            stored::AcceptedDocuments {
                receipt: &accepted.receipt,
                request: &accepted.request,
                proposal: &accepted.proposal,
            },
        )
        .map_err(helper_error)?;
    validate_provenance_row(&row, &documents.provenance, &accepted)?;
    let stored::IssuedDocuments {
        request,
        receipt,
        receipt_digest,
        provenance,
    } = documents;
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

fn validate_provenance_row(
    row: &sqlx::postgres::PgRow,
    provenance: &Provenance,
    accepted: &AcceptedHistory,
) -> Result<(), AppError> {
    if row.get::<Uuid, _>("registration_id") != provenance.registration_id
        || provenance.schema_version != 2
        || provenance.inventory_compatibility_epoch == 0
        || provenance.availability.len() > 1024
        || provenance.private_bytes != accepted.receipt.lease.materials[0].material_bytes
        || provenance.private_digest != accepted.receipt.lease.materials[0].material_digest
    {
        return Err(refused());
    }
    Ok(())
}

pub(super) fn helper_error(error: aq_profiled_state::phase_execution::Error) -> AppError {
    use aq_profiled_state::phase_execution::Error;
    match error {
        Error::Invalid => refused(),
        Error::Encoding => AppError::Internal,
        Error::Database(error) => db(error),
    }
}
