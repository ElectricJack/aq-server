use super::*;

/// Actual source-owner acknowledgement retained after the public Git import.
/// This records historical retention, not current physical availability or B2
/// eligibility. Caller holds common node/attempt locks before this transfer row.
pub struct RecordedSourceConfirmation {
    pub manifest: TransferManifest,
    pub receipt: CentralRetentionReceipt,
    pub acknowledgement: Value,
}
/// Resolve an actual import operation, never a worker's unverified receipt ID.
/// No current machine credential is needed to read this historical owner record.
pub async fn load_confirmed_locked(
    tx: &mut Transaction<'_, Postgres>,
    operation: Uuid,
    recorded: &crate::profiled_execution::results::RecordedResult,
) -> Result<RecordedSourceConfirmation> {
    if !recorded.report.source_receipt_ids.contains(&operation) {
        return Err(conflict(
            "source receipt is not pinned by the recorded result",
        ));
    }
    let row = sqlx::query("SELECT transfer_id,node_id,attempt_id,repository_id,artifact_id,manifest,state,receipt,central_receipt FROM source_transfers WHERE import_operation_id=$1 FOR SHARE")
        .bind(operation).fetch_optional(&mut **tx).await.map_err(internal)?
        .ok_or_else(|| conflict("source owner acknowledgement is unavailable"))?;
    if row.get::<String, _>("state") != "retained" {
        return Err(conflict("source owner acknowledgement is not retained"));
    }
    let manifest: TransferManifest =
        serde_json::from_value(row.get("manifest")).map_err(internal)?;
    manifest.validate().map_err(internal)?;
    let receipt: CentralRetentionReceipt =
        serde_json::from_value(row.get("receipt")).map_err(internal)?;
    receipt.validate().map_err(internal)?;
    let acknowledgement: Value = row.get("central_receipt");
    let confirmation = RecordedSourceConfirmation {
        manifest,
        receipt,
        acknowledgement,
    };
    confirmation.validate(&row, operation, recorded)?;
    Ok(confirmation)
}

impl RecordedSourceConfirmation {
    fn validate(
        &self,
        row: &sqlx::postgres::PgRow,
        operation: Uuid,
        recorded: &crate::profiled_execution::results::RecordedResult,
    ) -> Result<()> {
        // Check the owner row, public acknowledgement and recorded worker result together.
        let bound = self.matches_assignment(row, &recorded.report.binding)
            && self.matches_source(&recorded.spec.source);
        let retained = self.matches_row(row) && self.matches_receipt(operation);
        if !bound || !retained {
            return Err(AppError::Internal);
        }
        if !self.matches_acknowledgement()? {
            return Err(AppError::Internal);
        }
        Ok(())
    }
    fn matches_assignment(
        &self,
        row: &sqlx::postgres::PgRow,
        binding: &aq_admission::LaunchBinding,
    ) -> bool {
        let aq_session_protocol::SessionIdentity::Remote {
            node_id,
            session_id,
            ..
        } = binding.session
        else {
            return false;
        };
        let manifest = &self.manifest;
        let owner = (
            row.get::<Uuid, _>("node_id"),
            row.get::<Uuid, _>("attempt_id"),
        );
        let attempt = &manifest.attempt;
        let assigned = (attempt.attempt_id, attempt.session_id, attempt.generation);
        owner == (node_id, binding.attempt_id)
            && assigned
                == (
                    binding.attempt_id,
                    session_id,
                    binding.assignment_generation,
                )
            && manifest.resolved_spec_digest == binding.execution_spec_digest.as_str()
    }
    fn matches_source(&self, source: &aq_admission::SourceAdmission) -> bool {
        let m = &self.manifest;
        let checkpoint = m.source.base.hex.bytes().any(|b| b != b'0')
            && m.checkpoint.hex.bytes().any(|b| b != b'0')
            && m.role == aq_worker_protocol::SourceRole::Checkpoint;
        let repository = m.source.repository_id == source.repository_id
            && m.source.binding_generation == source.repository_binding_generation;
        checkpoint
            && repository
            && m.source.base.hex == source.resolved_object.hex
            && matches!(
                (&m.source.base.algorithm, &source.resolved_object.format),
                (
                    aq_worker_protocol::ObjectAlgorithm::Sha1,
                    aq_admission::ObjectFormat::Sha1
                )
            )
    }
    fn matches_row(&self, row: &sqlx::postgres::PgRow) -> bool {
        let m = &self.manifest;
        let stored = (
            row.get::<Uuid, _>("repository_id"),
            row.get::<Uuid, _>("transfer_id"),
            row.get::<Uuid, _>("artifact_id"),
        );
        stored == (m.source.repository_id, m.transfer_id, m.artifact_id)
    }
    fn matches_receipt(&self, operation: Uuid) -> bool {
        let m = &self.manifest;
        let r = &self.receipt;
        let transfer = (r.receipt_id, r.transfer_id, r.retention_handle);
        let repository = (r.repository_id, r.binding_generation);
        transfer == (operation, m.transfer_id, m.artifact_id)
            && repository == (m.source.repository_id, m.source.binding_generation)
            && r.source == m.checkpoint
    }
    fn matches_acknowledgement(&self) -> Result<bool> {
        use aq_delivery::source_transport::{matches_import_receipt, receiver_id};
        let acknowledgement = &self.acknowledgement;
        Ok(
            matches_import_receipt(acknowledgement, &self.receipt).map_err(internal)?
                && receiver_id(acknowledgement).is_some_and(|id| !id.is_nil()),
        )
    }
}
