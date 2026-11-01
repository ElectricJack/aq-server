//! Installation claims retained atomically with authority, never reconstructed.
use super::*;
use aq_admission::MaterialReceipt;
fn validate(receipt: &MaterialReceipt, lease: &MaterialLease, issued: u64) -> Result<(), AppError> {
    receipt
        .validate_against(lease, issued)
        .map_err(|_| refused())?;
    if receipt.private_binding.id.as_str() != format!("aq.private-material/{}", lease.lease_id)
        || receipt.private_binding.version.as_str() != "1"
    {
        return Err(refused());
    }
    Ok(())
}
async fn retained(
    tx: &mut Transaction<'_, Postgres>,
    lease: &MaterialLease,
    issued: u64,
) -> Result<Option<MaterialReceipt>, AppError> {
    let row=sqlx::query("SELECT operation_id,receipt_digest,receipt FROM admission_material_receipts WHERE lease_id=$1 FOR SHARE").bind(lease.lease_id).fetch_optional(&mut **tx).await.map_err(db)?;
    let Some(row) = row else { return Ok(None) };
    let value: Value = row.get("receipt");
    let receipt: MaterialReceipt = serde_json::from_value(value.clone()).map_err(|_| refused())?;
    if !same(&value, &receipt)? {
        return Err(refused());
    }
    validate(&receipt, lease, issued)?;
    if row.get::<Uuid, _>("operation_id") != receipt.operation_id
        || row.get::<String, _>("receipt_digest")
            != aq_selection::digest(&receipt)
                .map_err(|_| AppError::Internal)?
                .as_str()
    {
        return Err(refused());
    }
    Ok(Some(receipt))
}
/// Caller holds exact attempt/material lease authority locks. `original_issued_ms`
/// is the retained grant's original clock, not today's retrieval-lease expiry.
/// This reads an existing installation claim; it grants no current authority.
pub(crate) async fn load_installed_receipt_locked(
    tx: &mut Transaction<'_, Postgres>,
    lease: &MaterialLease,
    original_issued_ms: u64,
) -> Result<MaterialReceipt, AppError> {
    retained(tx, lease, original_issued_ms)
        .await?
        .ok_or_else(refused)
}
/// Under the same lease lock and shared quota lock as first grant. Each retained
/// row is included once in the shared actual retained JSONB aggregate; exact
/// existing bytes neither rotate installation identity nor add another row.
pub(super) async fn retain(
    tx: &mut Transaction<'_, Postgres>,
    lease: &MaterialLease,
    receipt: &MaterialReceipt,
    issued: u64,
) -> Result<(), AppError> {
    validate(receipt, lease, issued)?;
    if let Some(old) = retained(tx, lease, issued).await? {
        if !same(&old, receipt)? {
            return Err(refused());
        }
        return Ok(());
    }
    let value = serde_json::to_value(receipt).map_err(|_| AppError::Internal)?;
    let bytes: i32 = sqlx::query_scalar("SELECT octet_length($1::jsonb::text)")
        .bind(&value)
        .fetch_one(&mut **tx)
        .await
        .map_err(db)?;
    let bytes = u64::try_from(bytes).map_err(|_| refused())?;
    if bytes > 65536 {
        return Err(refused());
    }
    sqlx::query("INSERT INTO admission_material_receipts(lease_id,operation_id,receipt_digest,receipt) VALUES($1,$2,$3,$4)").bind(lease.lease_id).bind(receipt.operation_id).bind(aq_selection::digest(receipt).map_err(|_|AppError::Internal)?.as_str()).bind(value).execute(&mut **tx).await.map_err(db)?;
    Ok(())
}
