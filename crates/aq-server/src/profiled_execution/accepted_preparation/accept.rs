use super::phase::{AcceptRequest, AcceptedDelivery, AcceptedReceipt};
use super::*;
use crate::{
    execution::{authority::hot::LaunchOwner, profiled::Status},
    worker_api::AuthenticatedNode,
};
use aq_admission::{
    Digest, MaterialLease,
    control::{DeliveryStamp, Versions},
    transport::DeliveryClock,
};
use sqlx::Row;
use uuid::Uuid;
pub async fn accept(
    state: &AppState,
    auth: &AuthenticatedNode,
    request: &AcceptRequest,
    nonce: Uuid,
) -> Result<AcceptedDelivery, AppError> {
    if !super::policy::enabled(state) {
        return Err(refused());
    }
    if nonce.is_nil() || request.original.identity.node_id != auth.id() {
        return Err(refused());
    }
    let received = now(state)?;
    let _request_permit = request_permit()?;
    let mut tx = begin(state).await?;
    require_schema(&mut tx).await?;
    crate::worker_api::lock_current_node(&mut tx, auth).await?;
    let proposal =
        super::authority::proposal_locked(&mut tx, &request.original.acceptance.binding).await?;
    request.validate_for(&proposal).map_err(|_| refused())?;
    let runtime = super::super::registration::current_runtime_locked(
        state,
        &mut tx,
        auth.id(),
        request.original.identity.boot_id,
    )
    .await?;
    let (_, policy) = policy_for(state, &runtime.runtime, &runtime.qualification)?;
    if request.hold.source_profile != policy.source_profile
        || request.hold.source_disk_bytes != policy.source_disk_bytes
        || request.hold.resources.disk_bytes > 68_719_476_736
    {
        return Err(refused());
    }

    if let Some(reply) = replay_locked(state, &mut tx, auth, &proposal, request, nonce).await? {
        tx.commit().await.map_err(db)?;
        return Ok(reply);
    }
    let before = super::super::preparation::locked(state, &mut tx, auth, &request.original).await?;
    if before.hot.status != Status::Offered {
        return Err(refused());
    }
    super::authority::current_policy_locked(state, &mut tx, auth, &proposal).await?;
    let (timely, expiry) = acceptance_window(received, before.offer_expiry, request, &proposal)?;
    if now(state)? >= timely || expiry <= received {
        return Err(refused());
    }
    tx.commit().await.map_err(db)?;
    let retention = crate::source_retention::profiled_base::inspect_retained(
        state,
        before.source.token(),
        before.source.fact(),
    )
    .await?;
    let registry = state.config.intelligence.clone().ok_or_else(|| refused())?;
    let selection = before.selection.clone();
    let binding = request.original.acceptance.binding.clone();
    let lease_id = Uuid::new_v5(
        &request.original.operation_id,
        binding.attempt_id.as_bytes(),
    );
    let head = super::super::login_refresh::newest_source(
        &state.store.pool,
        &registry,
        &selection.choice.credential_profile,
        selection.credential_revision,
    )
    .await?;
    let permit =
        super::super::accepted_preparation::material_read_permit().map_err(|_| refused())?;
    let mut material = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            super::super::material::issue_for_selection_from(
                &registry,
                &selection,
                &binding,
                lease_id,
                received,
                expiry,
                None,
                head.as_ref(),
            )
        }),
    )
    .await
    .map_err(|_| refused())?
    .map_err(|_| AppError::Internal)?
    .map_err(|_| refused())?;
    let mut tx = begin(state).await?;
    require_schema(&mut tx).await?;
    let after = super::super::preparation::locked(state, &mut tx, auth, &request.original).await?;
    super::authority::current_policy_locked(state, &mut tx, auth, &proposal).await?;
    let stored =
        super::authority::proposal_locked(&mut tx, &request.original.acceptance.binding).await?;
    if stored.digest().map_err(|_| refused())? != proposal.digest().map_err(|_| refused())?
        || before.source.fact().canonical_bytes()? != after.source.fact().canonical_bytes()?
    {
        return Err(refused());
    }
    retention.validate_for(after.source.token())?;
    let time = now(state)?;
    if time >= timely || time < retention.observed_at_ms() || time >= retention.expires_at_ms() {
        return Err(refused());
    }
    crate::intelligence::fence_current_registry(state, &mut tx).await?;
    let registry_at_accept = registry_at_accept(state, &mut tx, &proposal).await?;
    if let Some(reply) = replay_locked(state, &mut tx, auth, &proposal, request, nonce).await? {
        tx.commit().await.map_err(db)?;
        return Ok(reply);
    }
    if after.hot.status != Status::Offered {
        return Err(refused());
    }
    let conflict: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM profiled_execution_receipts WHERE node_id=$1 AND operation_id=$2) OR EXISTS(SELECT 1 FROM profiled_v2_accepts WHERE node_id=$1 AND operation_id=$2)",
    )
        .bind(auth.id())
        .bind(request.original.operation_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
    if conflict {
        return Err(refused());
    }
    // A newly issued receipt is published once; candidate I/O time is charged.
    material.issued_at_ms = time;
    material.validate().map_err(|_| refused())?;
    let mut hot = after.hot;
    let row = sqlx::query(
        "SELECT record_revision,authority_revision,grant_revision FROM profiled_authority_hot WHERE attempt_id=$1",
    )
        .bind(request.original.acceptance.binding.attempt_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
    let receipt = AcceptedReceipt {
        schema_version: 2,
        negotiation: request.negotiation.clone(),
        operation_id: request.original.operation_id,
        request_digest: request.digest().map_err(|_| refused())?,
        lease: accepted_lease(
            request,
            &proposal,
            &material,
            time,
            expiry,
            lease_id,
            registry_at_accept,
        )?,
        versions: Versions {
            record_revision: row.get::<i64, _>("record_revision") as u64 + 1,
            authority_revision: row.get::<i64, _>("authority_revision") as u64 + 1,
            grant_revision: row.get::<i64, _>("grant_revision") as u64,
        },
    };
    receipt
        .validate_for(request, &proposal)
        .map_err(|_| refused())?;
    let request_value = value(request)?;
    let receipt_value = value(&receipt)?;
    let material_value = value(&material)?;
    let bytes: i32 = sqlx::query_scalar(
        "SELECT octet_length($1::jsonb::text)+octet_length($2::jsonb::text)+octet_length($3::jsonb::text)",
    )
        .bind(&request_value)
        .bind(&receipt_value)
        .bind(&material_value)
        .fetch_one(&mut *tx)
        .await
        .map_err(db)?;
    quota(&mut tx, auth.id(), bytes as u64, false).await?;
    if now(state)? >= timely || now(state)? >= expiry {
        return Err(refused());
    }
    super::super::closure::history::for_attempt_locked(
        &mut tx,
        &request.original.acceptance.binding,
    )
    .await?;
    let owner = LaunchOwner {
        guardian_id: request.original.guardian_id,
        launch_id: request.original.launch_id,
        reservation_id: request.original.reservation.reservation_id,
    };
    hot.status = Status::Preparing;
    hot.owner = Some(owner.clone());
    hot.preparation_operation_id = Some(request.original.operation_id);
    insert_material(&mut tx, auth, request, &material).await?;
    sqlx::query(
        "INSERT INTO profiled_v2_accepts(attempt_id,node_id,operation_id,request,request_digest,receipt,receipt_digest,lease_id,issued_at_ms,expires_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
    )
        .bind(request.original.acceptance.binding.attempt_id)
        .bind(auth.id())
        .bind(request.original.operation_id)
        .bind(request_value)
        .bind(request.digest().map_err(|_| refused())?.as_str())
        .bind(receipt_value)
        .bind(receipt.digest().map_err(|_| refused())?.as_str())
        .bind(material.lease_id)
        .bind(time as i64)
        .bind(expiry as i64)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(request.original.acceptance.binding.attempt_id)
        .bind(value(&hot)?)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    super::super::journal::record_reserved(
        &mut tx,
        &request.original.acceptance.binding,
        &owner,
        request.original.journal_revision,
        &request.original.journal_digest,
    )
    .await?;
    crate::delegated_permits::handoff::issue_locked(state, &mut tx, auth, request, &receipt)
        .await?;
    let reply = delivery(state, request, &proposal, receipt, nonce)?;
    tx.commit().await.map_err(db)?;
    Ok(reply)
}
fn acceptance_window(
    received: u64,
    offer_expiry: u64,
    request: &AcceptRequest,
    proposal: &super::phase::Proposal,
) -> Result<(u64, u64), AppError> {
    let timely = offer_expiry.min(
        received
            .checked_add(request.original.reservation_remaining_ms)
            .ok_or_else(|| refused())?,
    );
    let expiry = received
        .checked_add(
            request
                .hold
                .maximum_preparation_ms
                .min(proposal.policy.maximum_preparation_ms),
        )
        .ok_or_else(|| refused())?
        .min(
            proposal
                .original
                .offered
                .artifacts
                .iter()
                .map(|artifact| artifact.retain_until_ms)
                .min()
                .ok_or_else(|| refused())?,
        );
    Ok((timely, expiry))
}

/// Both replay points recheck live authority and handoff before stamping delivery.
async fn replay_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    proposal: &super::phase::Proposal,
    request: &AcceptRequest,
    nonce: Uuid,
) -> Result<Option<AcceptedDelivery>, AppError> {
    let Some(receipt) = super::authority::accepted_locked(tx, proposal, request).await? else {
        return Ok(None);
    };
    super::authority::live_locked(state, tx, auth, proposal, request, &receipt).await?;
    crate::delegated_permits::handoff::issue_locked(state, tx, auth, request, &receipt).await?;
    delivery(state, request, proposal, receipt, nonce).map(Some)
}

fn delivery(
    state: &AppState,
    request: &AcceptRequest,
    proposal: &super::phase::Proposal,
    receipt: AcceptedReceipt,
    nonce: Uuid,
) -> Result<AcceptedDelivery, AppError> {
    let expires = receipt.lease.expires_at_ms;
    let d = AcceptedDelivery {
        schema_version: 2,
        receipt,
        delivery: DeliveryStamp {
            nonce,
            clock: DeliveryClock::new(now(state)?, expires).map_err(|_| refused())?,
        },
    };
    d.validate_for(request, proposal, nonce)
        .map_err(|_| refused())?;
    Ok(d)
}
async fn insert_material(
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    request: &AcceptRequest,
    material: &MaterialLease,
) -> Result<(), AppError> {
    let binding = &request.original.acceptance.binding;
    sqlx::query(
        "INSERT INTO admission_material_leases(lease_id,operation_id,request_digest,node_id,boot_id,prospective_attempt_id,admitted_attempt_id,offer_id,credential_identity,credential_revision,purpose,lease,verifier,state,issued_at_ms,expires_at_ms) VALUES($1,$2,$3,$4,$5,$6,$6,$7,$8,$9,$10,$11,$12,'admitted',$13,$14)",
    )
        .bind(material.lease_id)
        .bind(request.original.operation_id)
        .bind(request.digest().map_err(|_| refused())?.as_str())
        .bind(auth.id())
        .bind(request.original.identity.boot_id)
        .bind(binding.attempt_id)
        .bind(binding.offer_id)
        .bind(format!(
            "{}@{}",
            material.credential_profile.id.as_str(),
            material.credential_profile.version.as_str()
        ))
        .bind(material.credential_revision as i64)
        .bind(material.purpose.as_str())
        .bind(value(material)?)
        .bind(Digest::of(Uuid::new_v4().as_bytes()).as_str())
        .bind(material.issued_at_ms as i64)
        .bind(material.expires_at_ms as i64)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    sqlx::query("INSERT INTO profiled_launch_materials(attempt_id,lease_id) VALUES($1,$2)")
        .bind(binding.attempt_id)
        .bind(material.lease_id)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (AcceptRequest, phase::Proposal) {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../aq-admission/artifacts/preparation-v2.fixture.json"
        ))
        .unwrap();
        (
            serde_json::from_value(fixture["accept"].clone()).unwrap(),
            serde_json::from_value(fixture["proposal"].clone()).unwrap(),
        )
    }

    #[test]
    fn acceptance_and_preparation_have_independent_deadlines_with_original_caps() {
        let (mut request, mut proposal) = fixture();
        assert_eq!(
            acceptance_window(1500, 2000, &request, &proposal).unwrap(),
            (2000, 31500)
        );
        request.original.reservation_remaining_ms = 100;
        assert_eq!(
            acceptance_window(1500, 2000, &request, &proposal).unwrap(),
            (1600, 31500)
        );
        request.hold.maximum_preparation_ms = 1000;
        assert_eq!(
            acceptance_window(1500, 2000, &request, &proposal).unwrap(),
            (1600, 2500)
        );
        proposal.policy.maximum_preparation_ms = 500;
        assert_eq!(
            acceptance_window(1500, 2000, &request, &proposal).unwrap(),
            (1600, 2000)
        );
        proposal.original.offered.artifacts[0].retain_until_ms = 1700;
        assert_eq!(
            acceptance_window(1500, 2000, &request, &proposal).unwrap(),
            (1600, 1700)
        );
    }

    #[test]
    fn acceptance_window_refuses_overflow_and_missing_retention() {
        let (mut request, mut proposal) = fixture();
        request.original.reservation_remaining_ms = u64::MAX;
        assert!(acceptance_window(1500, 2000, &request, &proposal).is_err());
        request.original.reservation_remaining_ms = 0;
        assert!(acceptance_window(u64::MAX - 1, u64::MAX, &request, &proposal).is_err());
        proposal.original.offered.artifacts.clear();
        assert!(acceptance_window(1500, 2000, &request, &proposal).is_err());
    }
}
