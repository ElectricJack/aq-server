//! Full composed39 attestation, including unchanged result38 origin predicates.
use super::{Result, StoreError, result38};
use sqlx::{Postgres, Row, Transaction};
fn corrupt() -> StoreError {
    StoreError::Corrupt("settlement39 schema, bounds or retained owner joins inconsistent".into())
}
pub(super) async fn validate(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    validate_composed(tx, false, false, 39).await
}
pub(super) async fn validate_phase_absence(tx: &mut Transaction<'_, Postgres>) -> Result<()> {
    validate_composed(tx, true, false, 40).await
}
pub(super) async fn validate_compute_reuse(
    tx: &mut Transaction<'_, Postgres>,
    supported: u32,
) -> Result<()> {
    validate_composed(tx, true, true, supported).await
}
async fn validate_composed(
    tx: &mut Transaction<'_, Postgres>,
    phase_absence: bool,
    compute_reuse: bool,
    supported: u32,
) -> Result<()> {
    let mut expected: serde_json::Value =
        serde_json::from_str(include_str!("settlement39_catalog.json")).map_err(|_| corrupt())?;
    if expected.as_array().is_none_or(Vec::is_empty) {
        return Err(corrupt());
    }
    if phase_absence {
        let row = expected
            .as_array_mut()
            .ok_or_else(corrupt)?
            .iter_mut()
            .find(|row| row["kind"] == "function" && row["name"] == "settlement_claim_guard")
            .ok_or_else(corrupt)?;
        row["body"]["body"] = serde_json::Value::String(
            super::never_granted40::function_body("settlement_claim_guard")?.into(),
        );
    }
    if compute_reuse {
        let row = expected
            .as_array_mut()
            .ok_or_else(corrupt)?
            .iter_mut()
            .find(|row| row["kind"] == "function" && row["name"] == "profiled_result_origin_valid")
            .ok_or_else(corrupt)?;
        row["body"]["body"] = serde_json::Value::String(
            super::compute41::function_body("profiled_result_origin_valid")?.into(),
        );
    }
    // Format 50 widens both claim guards for replacement releases only.
    for (name, body) in super::replacement52::guard_overrides(supported)? {
        let row = expected
            .as_array_mut()
            .ok_or_else(corrupt)?
            .iter_mut()
            .find(|row| row["kind"] == "function" && row["name"] == name)
            .ok_or_else(corrupt)?;
        row["body"]["body"] = serde_json::Value::String(body.into());
    }
    let actual: serde_json::Value = sqlx::query_scalar(include_str!("settlement39_catalog.sql"))
        .fetch_one(&mut **tx)
        .await?;
    if actual != expected {
        return Err(corrupt());
    }
    result38::validate_data(tx).await?;
    // Finite global reservation caps this read at341*128KiB. Bound metadata
    // BEFORE fetching retained JSON; never silently repair or hash only a prefix.
    let size:(i64,i64)=sqlx::query_as("SELECT count(*),COALESCE(sum(octet_length(request::text)+octet_length(receipt::text)+octet_length(evidence::text)),0)::BIGINT FROM (SELECT request,receipt,evidence FROM profiled_settlement_evaluations LIMIT 342) bounded").fetch_one(&mut **tx).await?;
    if size.0 > 341 || size.1 > 67108864 {
        return Err(corrupt());
    }
    let completions:i64=sqlx::query_scalar("SELECT count(*) FROM (SELECT operation_id FROM profiled_task_completions LIMIT 342) bounded").fetch_one(&mut **tx).await?;
    if completions > 341 {
        return Err(corrupt());
    }
    let valid: bool = sqlx::query_scalar(include_str!("settlement39_data.sql"))
        .fetch_one(&mut **tx)
        .await?;
    if !valid {
        return Err(corrupt());
    }
    let rows=sqlx::query("SELECT request,request_digest,receipt,receipt_digest,evidence FROM profiled_settlement_evaluations ORDER BY operation_id LIMIT 342").fetch_all(&mut **tx).await?;
    if rows.len() != size.0 as usize {
        return Err(corrupt());
    }
    for row in rows {
        for (body, digest) in [("request", "request_digest"), ("receipt", "receipt_digest")] {
            let value: serde_json::Value = row.get(body);
            let mut value = value;
            value.sort_all_objects();
            let bytes = serde_json::to_vec(&value).map_err(|_| corrupt())?;
            if aq_admission::Digest::of(&bytes).as_str() != row.get::<String, _>(digest) {
                return Err(corrupt());
            }
        }
        let request: crate::profiled_execution::settlement::EvaluateRequest =
            serde_json::from_value(row.get("request")).map_err(|_| corrupt())?;
        let receipt: crate::profiled_execution::settlement::EvaluationReceipt =
            serde_json::from_value(row.get("receipt")).map_err(|_| corrupt())?;
        if request.operation_id != receipt.operation_id || request.subject != receipt.subject {
            return Err(corrupt());
        }
    }
    Ok(())
}
