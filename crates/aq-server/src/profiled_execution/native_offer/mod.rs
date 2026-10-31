//! Historical original publication only; this function grants no authority.
pub mod observation;
use crate::{error::AppError, profiled_execution::artifacts};
use aq_admission::{Digest, execution_spec::decode_profiled_spec, startup::NativeOffer};
use observation::StoredOffer;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;
/// Caller holds authenticated node and exact attempt/hot locks. Missing older
/// wrappers refuse; clocks, startup and retention receipts are never fabricated.
pub async fn load_original_offer_locked(
    tx: &mut Transaction<'_, Postgres>,
    attempt: Uuid,
) -> Result<NativeOffer, AppError> {
    let row=sqlx::query("SELECT launch_binding,launch_manifest,execution_spec,execution_spec_digest,manifest_digest,graph_task_observation,offer_expires_at_ms FROM profiled_launches WHERE attempt_id=$1 FOR SHARE").bind(attempt).fetch_optional(&mut **tx).await.map_err(|_|AppError::Internal)?.ok_or_else(refused)?;
    let stored: StoredOffer =
        serde_json::from_value(row.get("graph_task_observation")).map_err(|_| refused())?;
    stored.value()?;
    let native = stored.native_offer;
    let offer = &native.offered.offer;
    if offer.binding.attempt_id != attempt
        || row.get::<serde_json::Value, _>("launch_binding")
            != serde_json::to_value(&offer.binding).map_err(|_| AppError::Internal)?
        || row.get::<serde_json::Value, _>("launch_manifest")
            != serde_json::to_value(&offer.manifest).map_err(|_| AppError::Internal)?
        || row.get::<String, _>("manifest_digest") != offer.binding.launch_manifest_digest.as_str()
        || row.get::<String, _>("execution_spec_digest")
            != offer.binding.execution_spec_digest.as_str()
        || row.get::<i64, _>("offer_expires_at_ms") as u64 != native.offered.clock.expires_at_ms
    {
        return Err(refused());
    }
    let spec_bytes = canonical(&row.get::<serde_json::Value, _>("execution_spec"))?;
    if Digest::of(&spec_bytes) != offer.binding.execution_spec_digest {
        return Err(refused());
    }
    let spec = decode_profiled_spec(&spec_bytes).map_err(|_| refused())?;
    // Validate at the original publication instant: historical expiry does not
    // erase a receipt and cannot be mistaken for fresh delivery permission.
    let published = native.offered.clock.server_time_ms;
    stored.read_provenance.validate_at(published)?;
    if stored.read_provenance.task != spec.task
        || stored.read_provenance.authorized_context_digest != spec.task_context.digest
    {
        return Err(refused());
    }
    spec.validate_retained(offer, &native.offered.artifacts, published)
        .map_err(|_| refused())?;
    let ids:Vec<Uuid>=sqlx::query_scalar("SELECT artifact_id FROM profiled_launch_artifacts WHERE attempt_id=$1 ORDER BY artifact_id").bind(attempt).fetch_all(&mut **tx).await.map_err(|_|AppError::Internal)?;
    if ids.len() != native.offered.artifacts.len() || ids.len() > 126 {
        return Err(refused());
    }
    if native
        .offered
        .artifacts
        .iter()
        .try_fold(0_u64, |sum, a| sum.checked_add(a.bytes))
        .is_none_or(|n| n > 16_777_216)
        || native
            .offered
            .artifacts
            .iter()
            .map(|a| a.artifact.digest.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            != native.offered.artifacts.len()
    {
        return Err(refused());
    }
    let mut remaining = native.offered.artifacts.clone();
    for id in ids {
        let retained = artifacts::load_locked(tx, id).await?;
        let index = remaining
            .iter()
            .position(|v| {
                serde_json::to_value(v).ok() == serde_json::to_value(&retained.descriptor).ok()
            })
            .ok_or_else(refused)?;
        retained
            .descriptor
            .verify(&retained.bytes, published)
            .map_err(|_| refused())?;
        remaining.remove(index);
    }
    if !remaining.is_empty() {
        return Err(refused());
    }
    Ok(native)
}

fn refused() -> AppError {
    AppError::Conflict("original native offer unavailable or inconsistent".into())
}
fn canonical(value: &impl serde::Serialize) -> Result<Vec<u8>, AppError> {
    let mut value = serde_json::to_value(value).map_err(|_| AppError::Internal)?;
    value.sort_all_objects();
    serde_json::to_vec(&value).map_err(|_| AppError::Internal)
}
