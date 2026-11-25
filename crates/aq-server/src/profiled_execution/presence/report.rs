//! Same-grant observations only. Every fresh positive needs current authority;
//! newer scoped negatives cannot be hidden by a later renewal revision.
use super::{anchor, begin, changed, now, qualification::Loaded, storage, unavailable};
use crate::{
    AppState,
    error::AppError,
    execution::{
        authority::{self, Kind, Policy, hot::HotState},
        profiled::Status,
    },
    profiled_execution::{initial_grant, registration},
    worker_api::AuthenticatedNode,
};
use aq_admission::{
    control::Versions,
    scoped_presence::{Disposition, PresenceReceipt, PresenceReport, positive_expiry},
};
use sqlx::Row;
async fn replay(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    request: &PresenceReport,
) -> Result<Option<PresenceReceipt>, AppError> {
    if let Some(old) = storage::fact(tx, request.identity.node_id, request.operation_id).await? {
        if old.request.digest().map_err(|_| changed())?
            != request.digest().map_err(|_| changed())?
        {
            return Err(changed());
        }
        return Ok(Some(old.receipt));
    }
    Ok(None)
}
pub async fn record(
    state: &AppState,
    auth: &AuthenticatedNode,
    request: &PresenceReport,
) -> Result<PresenceReceipt, AppError> {
    request
        .validate()
        .map_err(|_| AppError::BadRequest("invalid scoped material report".into()))?;
    if auth.id() != request.identity.node_id {
        return Err(changed());
    }
    let reserved = state.presence_view();
    let state = &reserved;
    let mut tx = begin(state).await?;
    crate::worker_api::lock_current_node(&mut tx, auth).await?;
    if let Some(old) = replay(&mut tx, request).await? {
        tx.commit().await.map_err(|_| unavailable())?;
        return Ok(old);
    }
    let runtime =
        registration::current_runtime_locked(state, &mut tx, auth.id(), request.identity.boot_id)
            .await?;
    let q = Loaded::load(state, &request.negotiation.qualification)?;
    q.matches_runtime(&runtime)?;
    if request.runtime != runtime.runtime
        || request.runtime_qualification != runtime.qualification
        || request.ttl_ms > q.body.maximum_ttl_ms
    {
        return Err(changed());
    }
    let view = authority::load_locked(
        state,
        &mut tx,
        &request.binding.session,
        Policy::IdentityOnly,
    )
    .await?;
    if view.kind != Kind::ProfiledV1
        || view.identity.node_id != auth.id()
        || view.identity.boot_id != request.identity.boot_id
    {
        return Err(changed());
    }
    let row=sqlx::query("SELECT state,record_revision,authority_revision,grant_revision FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE").bind(request.binding.attempt_id).fetch_optional(&mut *tx).await.map_err(|_|AppError::Internal)?.ok_or_else(changed)?;
    let hot: HotState = serde_json::from_value(row.get("state")).map_err(|_| changed())?;
    hot.validate_against(&request.binding)
        .map_err(|_| changed())?;
    let owner = hot.owner.as_ref().ok_or_else(changed)?;
    let grant = hot.grant.as_ref().ok_or_else(changed)?;
    if owner.guardian_id != request.guardian_id
        || owner.launch_id != request.launch_id
        || owner.reservation_id != request.reservation_id
        || grant.grant_id != request.grant_id
        || grant.binding != request.binding
    {
        return Err(changed());
    }
    let versions = Versions {
        record_revision: row.get::<i64, _>("record_revision") as u64,
        authority_revision: row.get::<i64, _>("authority_revision") as u64,
        grant_revision: row.get::<i64, _>("grant_revision") as u64,
    };
    versions.validate().map_err(|_| changed())?;
    let original = initial_grant::load_original_grant_locked(
        &mut tx,
        auth.id(),
        &request.binding,
        request.grant_id,
    )
    .await?;
    if original.body.materials.len() != 1
        || hot.execution_deadline_ms != Some(original.execution_deadline_ms)
    {
        return Err(changed());
    }
    let lease = &original.body.materials[0];
    if lease.lease_id != request.lease_id {
        return Err(changed());
    }
    let installation = initial_grant::load_installed_receipt_locked(
        &mut tx,
        lease,
        original.body.clock.server_time_ms,
    )
    .await?;
    request
        .validate_against(lease, &installation)
        .map_err(|_| changed())?;
    let issuance = anchor::load(
        &mut tx,
        auth.id(),
        &request.binding,
        request.grant_id,
        &request.authority,
        &original,
    )
    .await?;
    if issuance.execution_deadline_ms != original.execution_deadline_ms
        || request.authority.grant_revision > versions.grant_revision
    {
        return Err(changed());
    }
    let positive = request.presence.is_present();
    if positive {
        if request.authority.grant_revision != versions.grant_revision
            || issuance.expires_at_ms != grant.expires_at_ms
            || !view.holds_task
            || hot.status != Status::Granted
            || hot.revoked
            || hot.cancellation_id.is_some()
            || hot.stop.known_stopped()
        {
            return Err(changed());
        }
        super::super::closure::ensure_launch_open_locked(&mut tx, &request.binding).await?;
    }
    crate::intelligence::fence_current_registry(state, &mut tx).await?;
    let material=sqlx::query("SELECT state,lease FROM admission_material_leases WHERE lease_id=$1 AND node_id=$2 AND admitted_attempt_id=$3 FOR SHARE").bind(lease.lease_id).bind(auth.id()).bind(request.binding.attempt_id).fetch_optional(&mut *tx).await.map_err(|_|AppError::Internal)?.ok_or_else(changed)?;
    if material.get::<String, _>("state") != "admitted"
        || material.get::<serde_json::Value, _>("lease")
            != serde_json::to_value(lease).map_err(|_| AppError::Internal)?
    {
        return Err(changed());
    }
    let epoch = require_inventory_locked(&mut tx, auth, request, positive).await?;
    require_new_sample_locked(&mut tx, auth, request, positive).await?;
    let recorded = now(state)?;
    if positive {
        view.active_at(recorded)?;
        if recorded >= grant.expires_at_ms {
            return Err(changed());
        }
    }
    let expires = if positive {
        positive_expiry(
            issuance.issued_at_ms,
            recorded,
            request.ttl_ms,
            q.body.maximum_age_ms,
            original.execution_deadline_ms,
        )
        .map_err(|_| changed())?
    } else {
        recorded
            .checked_add(request.ttl_ms)
            .filter(|n| *n <= i64::MAX as u64)
            .ok_or_else(unavailable)?
    };
    let receipt = PresenceReceipt {
        schema_version: 1,
        operation_id: request.operation_id,
        identity: request.identity.clone(),
        negotiation: request.negotiation.clone(),
        binding: request.binding.clone(),
        grant_id: request.grant_id,
        lease_id: request.lease_id,
        sequence: request.sequence,
        request_digest: request.digest().map_err(|_| changed())?,
        inventory_at_record: request.inventory.clone(),
        recorded_at_ms: recorded,
        expires_at_ms: expires,
        disposition: Disposition::Recorded,
    };
    storage::retain_fact(&mut tx, request, &receipt, epoch).await?;
    let final_time = now(state)?;
    if final_time < recorded
        || final_time >= expires
        || (positive
            && (final_time >= grant.expires_at_ms || final_time >= original.execution_deadline_ms))
    {
        return Err(changed());
    }
    // No hot state, lease, journal, graph or source/effect/claim mutation occurs.
    tx.commit().await.map_err(|_| unavailable())?;
    Ok(receipt)
}

async fn require_inventory_locked(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    auth: &AuthenticatedNode,
    request: &PresenceReport,
    positive: bool,
) -> Result<u64, AppError> {
    let accepted = storage::accepted(tx, auth.id(), &request.inventory).await?;
    if accepted.request.identity != request.identity
        || accepted.request.runtime != request.runtime
        || accepted.request.runtime_qualification != request.runtime_qualification
        || accepted.request.negotiation.qualification != request.negotiation.qualification
    {
        return Err(changed());
    }
    let head = storage::head(tx, auth.id()).await?.ok_or_else(changed)?;
    if head.boot != request.identity.boot_id || request.inventory.revision > head.inventory_revision
    {
        return Err(changed());
    }
    if positive
        && (head.mode != "scoped_v1"
            || head.operation != Some(request.inventory.operation_id)
            || head.epoch != accepted.epoch
            || head.snapshot_digest != request.inventory.snapshot_digest)
    {
        return Err(changed());
    }
    Ok(head.epoch)
}
async fn require_new_sample_locked(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    auth: &AuthenticatedNode,
    request: &PresenceReport,
    positive: bool,
) -> Result<(), AppError> {
    // A fresh report must not supersede a newer actual same-boot sample. Negative
    // dominance is retained even after its receipt deadline; no old-positive scan.
    if let Some(previous) =
        storage::latest_fact(tx, auth.id(), request.binding.attempt_id, request.lease_id).await?
    {
        require_sample_order(&previous.request, request, positive)?;
    }
    Ok(())
}

fn require_sample_order(
    previous: &PresenceReport,
    request: &PresenceReport,
    positive: bool,
) -> Result<(), AppError> {
    if previous.binding != request.binding
        || previous.grant_id != request.grant_id
        || request.sequence <= previous.sequence
    {
        return Err(changed());
    }
    if previous.observation.kernel_boot_id == request.observation.kernel_boot_id {
        if request.observation.read_started_ns < previous.observation.read_completed_ns {
            return Err(changed());
        }
    } else if positive {
        return Err(changed());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn previous_report() -> PresenceReport {
        serde_json::from_str(include_str!(
            "../../../../aq-admission/artifacts/scoped-presence-report.fixture.json"
        ))
        .unwrap()
    }

    #[test]
    fn a_new_sequence_cannot_hide_an_overlapping_observation() {
        let previous = previous_report();
        let mut next = previous.clone();
        next.sequence += 1;
        next.observation.read_started_ns = previous.observation.read_completed_ns - 1;
        for positive in [false, true] {
            assert!(require_sample_order(&previous, &next, positive).is_err());
        }
        next.observation.read_started_ns = previous.observation.read_completed_ns;
        for positive in [false, true] {
            assert!(require_sample_order(&previous, &next, positive).is_ok());
            next.sequence = previous.sequence;
            assert!(require_sample_order(&previous, &next, positive).is_err());
            next.sequence += 1;
        }
    }

    #[test]
    fn a_kernel_reboot_allows_only_a_same_grant_negative_sample() {
        let previous = previous_report();
        let mut next = previous.clone();
        next.sequence += 1;
        next.observation.kernel_boot_id = uuid::Uuid::new_v4();
        assert!(require_sample_order(&previous, &next, false).is_ok());
        assert!(require_sample_order(&previous, &next, true).is_err());
        next.grant_id = uuid::Uuid::new_v4();
        assert!(require_sample_order(&previous, &next, false).is_err());
        next.grant_id = previous.grant_id;
        next.binding.attempt_id = uuid::Uuid::new_v4();
        assert!(require_sample_order(&previous, &next, false).is_err());
    }
}
