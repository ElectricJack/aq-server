//! Transactional persistence, not an issuance authority. The acceptance/grant
//! owner must validate current authority before inserting into its transaction.
use super::PermitRecord;
use aq_admission_sandbox::sandbox::{ContractError, DelegatedPermit};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("delegated permit operation conflict")]
    Conflict,
    #[error("delegated permit owner unavailable")]
    Unavailable,
    #[error("invalid delegated permit metadata")]
    Invalid(#[from] ContractError),
    #[error("invalid delegated permit storage")]
    Corrupt(#[from] serde_json::Error),
    #[error("delegated permit storage failure")]
    Database(#[from] sqlx::Error),
}

/// The caller's transaction owns node -> attempt -> permit locks. No token is
/// written, including on replay. A replay returns the original metadata, never
/// the newly proposed candidate. It does not extend its validity window.
pub async fn insert_locked(
    tx: &mut Transaction<'_, Postgres>,
    operation_id: Uuid,
    request_digest: [u8; 32],
    permit: &DelegatedPermit,
) -> Result<PermitRecord, Error> {
    if operation_id.is_nil() {
        return Err(Error::Conflict);
    }
    let record = PermitRecord::from_issued(permit)?;
    let node: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM nodes WHERE id=$1 AND revoked_at IS NULL FOR UPDATE")
            .bind(permit.binding.node.node_id)
            .fetch_optional(&mut **tx)
            .await?;
    if node.is_none() {
        return Err(Error::Unavailable);
    }
    let attempt: Option<Uuid> = sqlx::query_scalar(
        "SELECT attempt_id FROM execution_attempts WHERE attempt_id=$1 AND node_id=$2 FOR UPDATE",
    )
    .bind(permit.binding.attempt_id)
    .bind(permit.binding.node.node_id)
    .fetch_optional(&mut **tx)
    .await?;
    if attempt.is_none() {
        return Err(Error::Unavailable);
    }
    if let Some(row) = sqlx::query(
        "SELECT request_digest,record,revoked FROM delegated_permits WHERE node_id=$1 AND operation_id=$2 FOR UPDATE",
    )
    .bind(permit.binding.node.node_id)
    .bind(operation_id)
    .fetch_optional(&mut **tx)
    .await?
    {
        if row.try_get::<Vec<u8>, _>("request_digest")? != request_digest {
            return Err(Error::Conflict);
        }
        return decode(&row);
    }
    sqlx::query(
        "INSERT INTO delegated_permits(permit_id,node_id,attempt_id,operation_id,request_digest,record) VALUES($1,$2,$3,$4,$5,$6)",
    )
    .bind(record.permit_id)
    .bind(record.binding.node.node_id)
    .bind(record.binding.attempt_id)
    .bind(operation_id)
    .bind(request_digest.as_slice())
    .bind(serde_json::to_value(&record)?)
    .execute(&mut **tx)
    .await?;
    Ok(record)
}

pub async fn load_locked(
    tx: &mut Transaction<'_, Postgres>,
    permit_id: Uuid,
) -> Result<Option<PermitRecord>, Error> {
    sqlx::query("SELECT record,revoked FROM delegated_permits WHERE permit_id=$1 FOR UPDATE")
        .bind(permit_id)
        .fetch_optional(&mut **tx)
        .await?
        .map(|row| decode(&row))
        .transpose()
}

fn decode(row: &sqlx::postgres::PgRow) -> Result<PermitRecord, Error> {
    let mut record: PermitRecord = serde_json::from_value(row.try_get("record")?)?;
    if row.try_get("revoked")? {
        record.revoke();
    }
    Ok(record)
}
