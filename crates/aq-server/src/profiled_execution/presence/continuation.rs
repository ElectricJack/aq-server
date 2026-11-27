//! Consume only the latest scoped fact, never scan back to an older Present.
use super::{anchor, changed, current_inventory::CurrentInventory, now, storage, unavailable};
use crate::{
    AppState,
    error::AppError,
    profiled_execution::{
        initial_grant::OriginalGrant,
        registration::{self, RegisteredRuntime},
    },
};
use aq_admission::{
    ArtifactRef, LaunchBinding, MaterialLease, MaterialReceipt, SessionIdentity,
    scoped_presence::positive_expiry,
};
use aq_selection::CapabilitySnapshot;
use sqlx::{Postgres, Transaction};
use uuid::Uuid;
/// Caller holds current node→attempt/hot and current registry/principal fences.
/// None denotes legacy inventory only; it never relaxes the legacy negative rule.
pub(crate) struct HeldMaterial<'a> {
    pub binding: &'a LaunchBinding,
    pub grant: Uuid,
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
        grant_revision,
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
    let Some(head) = scoped_head(tx, node_id, boot_id, snapshot).await? else {
        return Ok(None);
    };
    let current = super::current_inventory_locked(state, tx, node_id, boot_id, snapshot).await?;
    if current.compatibility_epoch != head.epoch {
        return Err(changed());
    }
    let q = &current.qualification;
    let runtime = registration::current_runtime_locked(state, tx, node_id, boot_id).await?;
    require_provisionable_binding(snapshot, lease)?;
    if now(state)? >= current.receipt.expires_at_ms {
        return Err(unavailable());
    }
    let fact = storage::latest_fact(tx, node_id, binding.attempt_id, lease.lease_id)
        .await?
        .ok_or_else(unavailable)?;
    let expected = ExpectedFact {
        binding,
        grant,
        grant_revision,
        node_id,
        boot_id,
        epoch: head.epoch,
        negotiation: &current.request.negotiation.qualification,
        runtime: &runtime,
    };
    if !expected.matches(&fact) {
        return Err(changed());
    }
    fact.request
        .validate_against(lease, installation)
        .map_err(|_| changed())?;
    require_present(state, &fact)?;
    require_unchanged_source(tx, node_id, boot_id, &fact, &current).await?;
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

/// Locks the node's semantic head. None means legacy inventory (no head or a
/// legacy-mode head); a scoped head must name this boot and exact snapshot.
async fn scoped_head(
    tx: &mut Transaction<'_, Postgres>,
    node_id: Uuid,
    boot_id: Uuid,
    snapshot: &CapabilitySnapshot,
) -> Result<Option<storage::Head>, AppError> {
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
    Ok(Some(head))
}

/// Identity the latest presence fact must still carry: the held launch, the
/// current inventory epoch and negotiation, and the registered runtime.
struct ExpectedFact<'a> {
    binding: &'a LaunchBinding,
    grant: Uuid,
    grant_revision: u64,
    node_id: Uuid,
    boot_id: Uuid,
    epoch: u64,
    negotiation: &'a ArtifactRef,
    runtime: &'a RegisteredRuntime,
}
impl ExpectedFact<'_> {
    fn matches(&self, fact: &storage::Fact) -> bool {
        let request = &fact.request;
        fact.epoch == self.epoch
            && request.binding == *self.binding
            && request.grant_id == self.grant
            && request.identity.node_id == self.node_id
            && request.identity.boot_id == self.boot_id
            && request.negotiation.qualification == *self.negotiation
            && request.runtime == self.runtime.runtime
            && request.runtime_qualification == self.runtime.qualification
            && request.authority.grant_revision <= self.grant_revision
    }
}

/// An absent fact is unavailable once expired and a change while still live.
fn require_present(state: &AppState, fact: &storage::Fact) -> Result<(), AppError> {
    if fact.request.presence.is_present() {
        return Ok(());
    }
    Err(if now(state)? >= fact.receipt.expires_at_ms {
        unavailable()
    } else {
        changed()
    })
}

/// The inventory the fact was reported against must still share the current
/// compatibility epoch and negotiated qualification.
async fn require_unchanged_source(
    tx: &mut Transaction<'_, Postgres>,
    node_id: Uuid,
    boot_id: Uuid,
    fact: &storage::Fact,
    current: &CurrentInventory,
) -> Result<(), AppError> {
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
    Ok(())
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
