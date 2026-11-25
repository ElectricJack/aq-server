//! Qualified global provisionability only; never scoped installed presence.
use super::{changed, now, qualification::Loaded, storage, unavailable};
use crate::{AppState, error::AppError, profiled_execution::registration};
use aq_admission::scoped_presence::{InventoryReceipt, InventoryV2, Qualification};
use aq_selection::CapabilitySnapshot;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

pub(crate) struct CurrentInventory {
    pub request: InventoryV2,
    pub receipt: InventoryReceipt,
    pub compatibility_epoch: u64,
    pub qualification: Qualification,
}
/// Caller holds the current node lock and supplies its actual retained normalized
/// snapshot. Every field is rechecked against the immutable operation and head.
pub(crate) async fn current_inventory_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    boot: Uuid,
    snapshot: &CapabilitySnapshot,
) -> Result<CurrentInventory, AppError> {
    let head = storage::head(tx, node).await?.ok_or_else(changed)?;
    if head.mode != "scoped_v1"
        || head.boot != boot
        || head.inventory_revision != snapshot.revision
        || head.snapshot_digest != aq_selection::digest(snapshot).map_err(|_| AppError::Internal)?
    {
        return Err(changed());
    }
    let current = storage::inventory(tx, node, head.operation.ok_or_else(changed)?)
        .await?
        .ok_or_else(changed)?;
    if current.epoch != head.epoch
        || current.receipt.inventory.revision != head.inventory_revision
        || current.receipt.inventory.snapshot_digest != head.snapshot_digest
        || current.request.identity.node_id != node
        || current.request.identity.boot_id != boot
    {
        return Err(changed());
    }
    let q = Loaded::load(state, &current.request.negotiation.qualification)?;
    let runtime = registration::current_runtime_locked(state, tx, node, boot).await?;
    q.matches_runtime(&runtime)?;
    if current.request.runtime != runtime.runtime
        || current.request.runtime_qualification != runtime.qualification
        || snapshot.backend_kind != runtime.backend_kind
    {
        return Err(changed());
    }
    let time = now(state)?;
    if time < snapshot.observed_at_ms
        || time >= snapshot.expires_at_ms
        || time < current.receipt.recorded_at_ms
        || time >= current.receipt.expires_at_ms
    {
        return Err(unavailable());
    }
    Ok(CurrentInventory {
        request: current.request,
        receipt: current.receipt,
        compatibility_epoch: head.epoch,
        qualification: q.body,
    })
}

/// Historical global inventory provenance only. The caller compares this epoch
/// with a freshly validated CurrentInventory; this never supplies freshness.
pub(crate) async fn inventory_epoch_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    boot: Uuid,
    revision: u64,
    digest: &aq_admission::Digest,
) -> Result<u64, AppError> {
    let revision = i64::try_from(revision).map_err(|_| changed())?;
    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT operation_id FROM profiled_presence_operations WHERE node_id=$1 AND boot_id=$2 AND kind='inventory_v2' AND inventory_revision=$3 AND snapshot_digest=$4 ORDER BY operation_id LIMIT 2 FOR SHARE")
        .bind(node).bind(boot).bind(revision).bind(digest.as_str())
        .fetch_all(&mut **tx).await.map_err(|_|unavailable())?;
    if ids.len() != 1 {
        return Err(changed());
    }
    let old = storage::inventory(tx, node, ids[0])
        .await?
        .ok_or_else(changed)?;
    if old.request.identity.node_id != node
        || old.request.identity.boot_id != boot
        || old.receipt.inventory.revision != revision as u64
        || old.receipt.inventory.snapshot_digest != *digest
    {
        return Err(changed());
    }
    Ok(old.epoch)
}
