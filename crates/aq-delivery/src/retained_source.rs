use super::{DeliveryError, OperationContext, Result, RetainedSource, oid};
use crate::source_transport::{matches_import_receipt, receiver_id};
use aq_worker_protocol::{CentralRetentionReceipt, SourceRole, TransferManifest, Validate};
use serde_json::Value;
use sqlx::{Postgres, Row, Transaction, postgres::PgRow};
use uuid::Uuid;

fn mismatch() -> DeliveryError {
    DeliveryError::InvalidInput("source owner acknowledgement mismatch")
}
struct Ownership {
    manifest: TransferManifest,
    receipt: CentralRetentionReceipt,
    acknowledgement: Value,
}
impl Ownership {
    fn from_row(row: &PgRow) -> Result<Self> {
        let manifest: TransferManifest =
            serde_json::from_value(row.get("manifest")).map_err(|_| mismatch())?;
        let receipt: CentralRetentionReceipt = serde_json::from_value(
            row.try_get::<Option<Value>, _>("receipt")?
                .ok_or_else(mismatch)?,
        )
        .map_err(|_| mismatch())?;
        let acknowledgement = row
            .try_get::<Option<Value>, _>("central_receipt")?
            .ok_or_else(mismatch)?;
        manifest.validate().map_err(|_| mismatch())?;
        receipt.validate().map_err(|_| mismatch())?;
        Ok(Self {
            manifest,
            receipt,
            acknowledgement,
        })
    }
    fn owns_transfer(&self, row: &PgRow, context: &OperationContext) -> bool {
        let manifest = &self.manifest;
        manifest.role == SourceRole::Checkpoint
            && row.get::<Uuid, _>("attempt_id") == manifest.attempt.attempt_id
            && row.get::<Uuid, _>("transfer_id") == manifest.transfer_id
            && row.get::<Uuid, _>("artifact_id") == manifest.artifact_id
            && row.get::<Uuid, _>("repository_id") == context.repository_id
    }
    fn matches_binding(&self, context: &OperationContext) -> bool {
        self.manifest.source.repository_id == context.repository_id
            && self.manifest.source.binding_generation == context.binding_generation
    }
    fn certifies_manifest(&self, id: Uuid, context: &OperationContext) -> bool {
        let receipt = &self.receipt;
        let manifest = &self.manifest;
        receipt.receipt_id == id
            && receipt.transfer_id == manifest.transfer_id
            && receipt.repository_id == context.repository_id
            && receipt.binding_generation == context.binding_generation
            && receipt.source == manifest.checkpoint
            && receipt.retention_handle == manifest.artifact_id
    }
    fn current_acknowledgement(&self) -> bool {
        let ack = &self.acknowledgement;
        matches_import_receipt(ack, &self.receipt).unwrap_or(false)
            && ack["historical"] == false
            && receiver_id(ack).is_some_and(|id| !id.is_nil())
    }
    fn retained(self, row: &PgRow, context: &OperationContext, id: Uuid) -> Result<RetainedSource> {
        let transfer = self.owns_transfer(row, context) && self.matches_binding(context);
        let certified =
            transfer && self.certifies_manifest(id, context) && self.current_acknowledgement();
        if !certified {
            return Err(mismatch());
        }
        oid(&self.receipt.source, false)?;
        Ok(RetainedSource {
            oid: self.receipt.source,
            retention_handle: self.receipt.retention_handle,
        })
    }
}
pub(super) async fn load(
    tx: &mut Transaction<'_, Postgres>,
    context: &OperationContext,
    id: Uuid,
) -> Result<RetainedSource> {
    let row = sqlx::query("SELECT transfer_id,attempt_id,repository_id,artifact_id,manifest,receipt,central_receipt FROM source_transfers WHERE import_operation_id=$1 AND state='retained' FOR SHARE")
        .bind(id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(DeliveryError::InvalidInput("source is not retained"))?;
    Ownership::from_row(&row)?.retained(&row, context, id)
}
