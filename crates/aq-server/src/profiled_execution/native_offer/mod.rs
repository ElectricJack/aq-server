//! Historical original publication only; this function grants no authority.
use crate::{error::AppError, profiled_execution::artifacts};
use aq_admission::{
    Digest, RetainedArtifact,
    execution_spec::{ProfiledExecutionSpec, decode_profiled_spec},
    startup::NativeOffer,
    transport::OfferedLaunch,
};
use aq_profiled_state::git_author::OfferDelivery;
pub use aq_profiled_state::native_offer as observation;
use observation::StoredOffer;
use sqlx::{Postgres, Row, Transaction, postgres::PgRow};
use uuid::Uuid;

/// Most retained artifacts one original offer may carry. The inventory query
/// reads one row more (`LIMIT 127`) so an oversized inventory is detected.
const MAX_OFFER_ARTIFACTS: usize = 126;
/// Upper bound on the summed byte size of one offer's retained artifacts.
const MAX_OFFER_ARTIFACT_BYTES: u64 = 16_777_216;

/// Caller holds authenticated node and exact attempt/hot locks. Missing older
/// wrappers refuse; clocks, startup and retention receipts are never fabricated.
pub async fn load_original_offer_locked(
    tx: &mut Transaction<'_, Postgres>,
    attempt: Uuid,
) -> Result<NativeOffer, AppError> {
    load_original_delivery_locked(tx, attempt)
        .await
        .map(|delivery| delivery.native)
}

pub(super) async fn load_original_delivery_locked(
    tx: &mut Transaction<'_, Postgres>,
    attempt: Uuid,
) -> Result<OfferDelivery, AppError> {
    let row = fetch_launch_locked(tx, attempt).await?;
    let stored = decode_stored_offer(&row)?;
    let native = &stored.native_offer.native;
    verify_launch_row(&row, attempt, &native.offered)?;
    let spec = decode_retained_spec(&row, &native.offered)?;
    verify_spec_provenance(&stored, &spec)?;
    let ids = fetch_artifact_ids_locked(tx, attempt).await?;
    verify_artifact_inventory(&native.offered.artifacts, ids.len())?;
    verify_retained_artifacts(tx, native, ids).await?;
    Ok(stored.native_offer)
}

async fn fetch_launch_locked(
    tx: &mut Transaction<'_, Postgres>,
    attempt: Uuid,
) -> Result<PgRow, AppError> {
    sqlx::query("SELECT launch_binding,launch_manifest,execution_spec,execution_spec_digest,manifest_digest,graph_task_observation,offer_expires_at_ms FROM profiled_launches WHERE attempt_id=$1 FOR SHARE")
        .bind(attempt)
        .fetch_optional(&mut **tx)
        .await
        .map_err(|_| AppError::Internal)?
        .ok_or_else(|| refused())
}

fn decode_stored_offer(row: &PgRow) -> Result<StoredOffer, AppError> {
    let stored: StoredOffer =
        serde_json::from_value(row.get("graph_task_observation")).map_err(|_| refused())?;
    stored.value().map_err(|_| refused())?;
    Ok(stored)
}

/// The launch row's binding, manifest, digests and expiry must equal the
/// stored original offer exactly.
fn verify_launch_row(row: &PgRow, attempt: Uuid, offered: &OfferedLaunch) -> Result<(), AppError> {
    let offer = &offered.offer;
    let binding = &offer.binding;
    require(binding.attempt_id == attempt)?;
    require(row.get::<serde_json::Value, _>("launch_binding") == to_json(binding)?)?;
    require(row.get::<serde_json::Value, _>("launch_manifest") == to_json(&offer.manifest)?)?;
    require(row.get::<String, _>("manifest_digest") == binding.launch_manifest_digest.as_str())?;
    require(
        row.get::<String, _>("execution_spec_digest") == binding.execution_spec_digest.as_str(),
    )?;
    require(row.get::<i64, _>("offer_expires_at_ms") as u64 == offered.clock.expires_at_ms)
}

/// Decodes the retained execution spec after checking its canonical digest
/// against the offer binding.
fn decode_retained_spec(
    row: &PgRow,
    offered: &OfferedLaunch,
) -> Result<ProfiledExecutionSpec, AppError> {
    let spec_bytes = canonical(&row.get::<serde_json::Value, _>("execution_spec"))?;
    require(Digest::of(&spec_bytes) == offered.offer.binding.execution_spec_digest)?;
    decode_profiled_spec(&spec_bytes).map_err(|_| refused())
}

/// Validates the graph read provenance and the spec at the original
/// publication instant: historical expiry does not erase a receipt and cannot
/// be mistaken for fresh delivery permission.
fn verify_spec_provenance(
    stored: &StoredOffer,
    spec: &ProfiledExecutionSpec,
) -> Result<(), AppError> {
    let offered = &stored.native_offer.native.offered;
    let provenance = &stored.read_provenance;
    let published = offered.clock.server_time_ms;
    provenance.validate_at(published).map_err(|_| refused())?;
    let latest_expiry = provenance
        .ready_expires_at_ms
        .min(provenance.context_expires_at_ms)
        .min(spec.source.expires_at_ms);
    require(offered.clock.expires_at_ms <= latest_expiry)?;
    require(provenance.task == spec.task)?;
    require(provenance.authorized_context_digest == spec.task_context.digest)?;
    spec.validate_retained(&offered.offer, &offered.artifacts, published)
        .map_err(|_| refused())
}

async fn fetch_artifact_ids_locked(
    tx: &mut Transaction<'_, Postgres>,
    attempt: Uuid,
) -> Result<Vec<Uuid>, AppError> {
    sqlx::query_scalar("SELECT artifact_id FROM profiled_launch_artifacts WHERE attempt_id=$1 ORDER BY artifact_id LIMIT 127")
        .bind(attempt)
        .fetch_all(&mut **tx)
        .await
        .map_err(|_| AppError::Internal)
}

/// The offered artifact list must match the retained row count, stay within
/// the count and byte bounds, and name each digest once.
fn verify_artifact_inventory(
    offered: &[RetainedArtifact],
    retained_count: usize,
) -> Result<(), AppError> {
    require(retained_count == offered.len())?;
    require(retained_count <= MAX_OFFER_ARTIFACTS)?;
    require(within_byte_bound(offered))?;
    require(digests_unique(offered))
}

fn within_byte_bound(artifacts: &[RetainedArtifact]) -> bool {
    artifacts
        .iter()
        .try_fold(0_u64, |sum, a| sum.checked_add(a.bytes))
        .is_some_and(|n| n <= MAX_OFFER_ARTIFACT_BYTES)
}

fn digests_unique(artifacts: &[RetainedArtifact]) -> bool {
    artifacts
        .iter()
        .map(|a| a.artifact.digest.clone())
        .collect::<std::collections::BTreeSet<_>>()
        .len()
        == artifacts.len()
}

/// Every retained artifact row must match exactly one offered descriptor,
/// verify its bytes at publication, and verify the startup payload when it is
/// that artifact. No offered descriptor may be left unmatched.
async fn verify_retained_artifacts(
    tx: &mut Transaction<'_, Postgres>,
    native: &NativeOffer,
    ids: Vec<Uuid>,
) -> Result<(), AppError> {
    let published = native.offered.clock.server_time_ms;
    let mut remaining = native.offered.artifacts.clone();
    for id in ids {
        let retained = artifacts::load_locked(tx, id).await?;
        let index = position_of(&remaining, &retained.descriptor)?;
        retained
            .descriptor
            .verify(&retained.bytes, published)
            .map_err(|_| refused())?;
        if retained.descriptor.artifact == native.startup.payload {
            native
                .startup
                .verify_payload(&retained.bytes)
                .map_err(|_| refused())?;
        }
        remaining.remove(index);
    }
    require(remaining.is_empty())
}

/// Matches descriptors by their serialized form, as the wire compares them.
fn position_of(
    remaining: &[RetainedArtifact],
    descriptor: &RetainedArtifact,
) -> Result<usize, AppError> {
    let wanted = serde_json::to_value(descriptor).ok();
    remaining
        .iter()
        .position(|v| serde_json::to_value(v).ok() == wanted)
        .ok_or_else(|| refused())
}

/// Refuses unless `holds`; the refusal records the checking caller's location.
#[track_caller]
fn require(holds: bool) -> Result<(), AppError> {
    if holds { Ok(()) } else { Err(refused()) }
}
#[track_caller]
fn refused() -> AppError {
    tracing::debug!(target: "aq_refusal", at = %std::panic::Location::caller(), "profiled refusal");
    AppError::Conflict("original native offer unavailable or inconsistent".into())
}
fn to_json(value: &impl serde::Serialize) -> Result<serde_json::Value, AppError> {
    serde_json::to_value(value).map_err(|_| AppError::Internal)
}
fn canonical(value: &impl serde::Serialize) -> Result<Vec<u8>, AppError> {
    let mut value = to_json(value)?;
    value.sort_all_objects();
    serde_json::to_vec(&value).map_err(|_| AppError::Internal)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn artifact(seed: &str, bytes: u64) -> RetainedArtifact {
        let reference = |label: &str| {
            serde_json::json!({
                "id": label,
                "version": "1",
                "digest": Digest::of(label.as_bytes()),
            })
        };
        serde_json::from_value(serde_json::json!({
            "artifact": reference(seed),
            "kind": "context",
            "bytes": bytes,
            "retention_receipt": reference(&format!("{seed}-receipt")),
            "retained_at_ms": 1,
            "retain_until_ms": 2,
        }))
        .expect("retained artifact fixture")
    }

    #[test]
    fn inventory_accepts_matching_bounded_unique_artifacts() {
        let offered = [
            artifact("a", 1),
            artifact("b", MAX_OFFER_ARTIFACT_BYTES - 1),
        ];
        assert!(verify_artifact_inventory(&offered, 2).is_ok());
        assert!(verify_artifact_inventory(&[], 0).is_ok());
    }

    #[test]
    fn inventory_refuses_count_mismatch_and_excess_rows() {
        let offered = [artifact("a", 1)];
        assert!(verify_artifact_inventory(&offered, 0).is_err());
        assert!(verify_artifact_inventory(&offered, 2).is_err());
        let many: Vec<_> = (0..=MAX_OFFER_ARTIFACTS)
            .map(|i| artifact(&i.to_string(), 1))
            .collect();
        assert!(
            verify_artifact_inventory(&many[..MAX_OFFER_ARTIFACTS], MAX_OFFER_ARTIFACTS).is_ok()
        );
        assert!(verify_artifact_inventory(&many, many.len()).is_err());
    }

    #[test]
    fn inventory_refuses_byte_bound_and_overflow() {
        let exact = [artifact("a", MAX_OFFER_ARTIFACT_BYTES)];
        assert!(verify_artifact_inventory(&exact, 1).is_ok());
        let over = [artifact("a", MAX_OFFER_ARTIFACT_BYTES), artifact("b", 1)];
        assert!(verify_artifact_inventory(&over, 2).is_err());
        let overflow = [artifact("a", u64::MAX), artifact("b", 1)];
        assert!(verify_artifact_inventory(&overflow, 2).is_err());
    }

    #[test]
    fn require_refuses_only_when_the_check_fails() {
        assert!(require(true).is_ok());
        assert!(matches!(require(false), Err(AppError::Conflict(_))));
    }

    #[test]
    fn inventory_refuses_duplicate_digests() {
        let duplicate = [artifact("a", 1), artifact("a", 2)];
        assert!(verify_artifact_inventory(&duplicate, 2).is_err());
    }
}
