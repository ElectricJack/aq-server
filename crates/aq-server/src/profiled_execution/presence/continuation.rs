//! Consume only the latest scoped fact, never scan back to an older Present.
use super::{anchor, changed, now, storage, unavailable};
use crate::{
    AppState,
    error::AppError,
    profiled_execution::{initial_grant::OriginalGrant, registration},
};
use aq_admission::{
    LaunchBinding, MaterialLease, MaterialReceipt, SessionIdentity,
    scoped_presence::positive_expiry,
};
use aq_selection::CapabilitySnapshot;
use sqlx::{Postgres, Transaction};
/// Caller holds current node→attempt/hot and current registry/principal fences.
/// None denotes legacy inventory only; it never relaxes the legacy negative rule.
pub(crate) struct HeldMaterial<'a> {
    pub binding: &'a LaunchBinding,
    pub grant: uuid::Uuid,
    pub grant_revision: u64,
    pub original: &'a OriginalGrant,
    pub lease: &'a MaterialLease,
    pub installation: &'a MaterialReceipt,
    pub snapshot: &'a CapabilitySnapshot,
}
pub(crate) async fn scoped_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    held: HeldMaterial<'_>,
) -> Result<Option<u64>, AppError> {
    let HeldMaterial {
        binding,
        grant,
        grant_revision: current_grant_revision,
        original,
        lease,
        installation,
        snapshot,
    } = held;
    let SessionIdentity::Remote {
        node_id, boot_id, ..
    } = binding.session
    else {
        return Err(changed());
    };
    let Some(head) = storage::head(tx, node_id).await? else {
        return Ok(None);
    };
    if head.mode != "scoped_v1" {
        return Ok(None);
    }
    if head.boot != boot_id
        || head.inventory_revision != snapshot.revision
        || head.snapshot_digest != aq_selection::digest(snapshot).map_err(|_| AppError::Internal)?
    {
        return Err(changed());
    }
    let current = super::current_inventory_locked(state, tx, node_id, boot_id, snapshot).await?;
    if current.compatibility_epoch != head.epoch {
        return Err(changed());
    }
    let q = current.qualification;
    let runtime = registration::current_runtime_locked(state, tx, node_id, boot_id).await?;
    require_provisionable_binding(snapshot, lease)?;
    if now(state)? >= current.receipt.expires_at_ms {
        return Err(unavailable());
    }
    let fact = storage::latest_fact(tx, node_id, binding.attempt_id, lease.lease_id)
        .await?
        .ok_or_else(unavailable)?;
    if fact.request.binding != *binding
        || fact.request.grant_id != grant
        || fact.request.identity.node_id != node_id
        || fact.request.identity.boot_id != boot_id
        || fact.epoch != head.epoch
        || fact.request.negotiation.qualification != current.request.negotiation.qualification
        || fact.request.runtime != runtime.runtime
        || fact.request.runtime_qualification != runtime.qualification
        || fact.request.authority.grant_revision > current_grant_revision
    {
        return Err(changed());
    }
    fact.request
        .validate_against(lease, installation)
        .map_err(|_| changed())?;
    if !fact.request.presence.is_present() {
        return Err(if now(state)? >= fact.receipt.expires_at_ms {
            unavailable()
        } else {
            changed()
        });
    }
    let original_epoch = super::inventory_epoch_locked(
        tx,
        node_id,
        boot_id,
        fact.request.inventory.revision,
        &fact.request.inventory.snapshot_digest,
    )
    .await?;
    if original_epoch != current.compatibility_epoch {
        return Err(changed());
    }
    let source = storage::accepted(tx, node_id, &fact.request.inventory).await?;
    if source.epoch != fact.epoch
        || source.request.negotiation.qualification != current.request.negotiation.qualification
    {
        return Err(changed());
    }
    let issued = anchor::load(
        tx,
        node_id,
        binding,
        grant,
        &fact.request.authority,
        original,
    )
    .await?;
    let expected = positive_expiry(
        issued.issued_at_ms,
        fact.receipt.recorded_at_ms,
        fact.request.ttl_ms,
        q.maximum_age_ms,
        original.execution_deadline_ms,
    )
    .map_err(|_| changed())?;
    if expected != fact.receipt.expires_at_ms || now(state)? >= expected {
        return Err(unavailable());
    }
    Ok(Some(
        expected
            .min(snapshot.expires_at_ms)
            .min(current.receipt.expires_at_ms),
    ))
}

fn require_provisionable_binding(
    snapshot: &CapabilitySnapshot,
    lease: &MaterialLease,
) -> Result<(), AppError> {
    if !snapshot.credential_bindings.iter().any(|b| {
        b.credential_profile == lease.credential_profile
            && b.credential_revision == lease.credential_revision
            && !b.available
            && b.preparation.is_some()
    }) {
        return Err(changed());
    }
    Ok(())
}
