use super::*;
use crate::{
    execution::{authority::hot::LaunchOwner, profiled::Status},
    worker_api::AuthenticatedNode,
};
use aq_admission::{
    Digest, MaterialLease,
    control::{DeliveryStamp, Versions},
    preparation_v2::{AcceptRequest, AcceptedDelivery, AcceptedPreparationLease, AcceptedReceipt},
    transport::DeliveryClock,
};
use sqlx::Row;
use std::sync::{Arc, OnceLock};
use uuid::Uuid;
pub async fn accept(
    state: &AppState,
    auth: &AuthenticatedNode,
    q: &AcceptRequest,
    nonce: Uuid,
) -> Result<AcceptedDelivery, AppError> {
    if !super::policy::enabled(state) {
        return Err(refused());
    }
    if nonce.is_nil() || q.original.identity.node_id != auth.id() {
        return Err(refused());
    }
    let received = now(state)?;
    let _request_permit = request_permit()?;
    let mut tx = begin(state).await?;
    require_schema(&mut tx).await?;
    crate::worker_api::lock_current_node(&mut tx, auth).await?;
    let p = super::authority::proposal_locked(&mut tx, &q.original.acceptance.binding).await?;
    q.validate_for(&p).map_err(|_| refused())?;
    let runtime = super::super::registration::current_runtime_locked(
        state,
        &mut tx,
        auth.id(),
        q.original.identity.boot_id,
    )
    .await?;
    let (_, policy) = policy_for(state, &runtime.runtime, &runtime.qualification)?;
    if q.hold.source_profile != policy.source_profile
        || q.hold.source_disk_bytes != policy.source_disk_bytes
        || q.hold.resources.disk_bytes > 68_719_476_736
    {
        return Err(refused());
    }

    if let Some(old) = super::authority::accepted_locked(&mut tx, &p, q).await? {
        super::authority::live_locked(state, &mut tx, auth, &p, q, &old).await?;
        let reply = delivery(state, q, &p, old, nonce)?;
        tx.commit().await.map_err(db)?;
        return Ok(reply);
    }
    let before = super::super::preparation::locked(state, &mut tx, auth, &q.original).await?;
    if before.hot.status != Status::Offered {
        return Err(refused());
    }
    super::authority::current_policy_locked(state, &mut tx, auth, &p).await?;
    let timely = before.offer_expiry.min(
        received
            .checked_add(q.original.reservation_remaining_ms)
            .ok_or_else(refused)?,
    );
    let expiry = received
        .checked_add(
            q.hold
                .maximum_preparation_ms
                .min(p.policy.maximum_preparation_ms),
        )
        .ok_or_else(refused)?
        .min(
            p.original
                .offered
                .artifacts
                .iter()
                .map(|a| a.retain_until_ms)
                .min()
                .ok_or_else(refused)?,
        );
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
    let registry = state.config.intelligence.clone().ok_or_else(refused)?;
    let selection = before.selection.clone();
    let binding = q.original.acceptance.binding.clone();
    let lease_id = Uuid::new_v5(&q.original.operation_id, binding.attempt_id.as_bytes());
    static READS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();
    let permit = READS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(2)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| refused())?;
    let mut material = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            super::super::material::issue_for_selection(
                &registry, &selection, &binding, lease_id, received, expiry,
            )
        }),
    )
    .await
    .map_err(|_| refused())?
    .map_err(|_| AppError::Internal)?
    .map_err(|_| refused())?;
    let mut tx = begin(state).await?;
    require_schema(&mut tx).await?;
    let after = super::super::preparation::locked(state, &mut tx, auth, &q.original).await?;
    super::authority::current_policy_locked(state, &mut tx, auth, &p).await?;
    let stored = super::authority::proposal_locked(&mut tx, &q.original.acceptance.binding).await?;
    if stored.digest().map_err(|_| refused())? != p.digest().map_err(|_| refused())?
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
    if let Some(old) = super::authority::accepted_locked(&mut tx, &p, q).await? {
        super::authority::live_locked(state, &mut tx, auth, &p, q, &old).await?;
        let reply = delivery(state, q, &p, old, nonce)?;
        tx.commit().await.map_err(db)?;
        return Ok(reply);
    }
    if after.hot.status != Status::Offered {
        return Err(refused());
    }
    let conflict:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM profiled_execution_receipts WHERE node_id=$1 AND operation_id=$2) OR EXISTS(SELECT 1 FROM profiled_v2_accepts WHERE node_id=$1 AND operation_id=$2)").bind(auth.id()).bind(q.original.operation_id).fetch_one(&mut *tx).await.map_err(db)?;
    if conflict {
        return Err(refused());
    }
    // A newly issued receipt is published once; candidate I/O time is charged.
    material.issued_at_ms = time;
    material.validate().map_err(|_| refused())?;
    let mut hot = after.hot;
    let row=sqlx::query("SELECT record_revision,authority_revision,grant_revision FROM profiled_authority_hot WHERE attempt_id=$1").bind(q.original.acceptance.binding.attempt_id).fetch_one(&mut *tx).await.map_err(db)?;
    let receipt = AcceptedReceipt {
        schema_version: 2,
        negotiation: q.negotiation.clone(),
        operation_id: q.original.operation_id,
        request_digest: q.digest().map_err(|_| refused())?,
        lease: AcceptedPreparationLease {
            lease_id: Uuid::new_v5(&lease_id, b"accepted-preparation-v2"),
            revision: 1,
            identity: q.original.identity.clone(),
            binding: q.original.acceptance.binding.clone(),
            guardian_id: q.original.guardian_id,
            launch_id: q.original.launch_id,
            hold: q.hold.clone(),
            proposal_digest: q.proposal_digest.clone(),
            policy_digest: p.policy.digest().map_err(|_| refused())?,
            choice_digest: p.choice.digest().map_err(|_| refused())?,
            issued_at_ms: time,
            expires_at_ms: expiry,
            materials: vec![material.clone()],
        },
        versions: Versions {
            record_revision: row.get::<i64, _>("record_revision") as u64 + 1,
            authority_revision: row.get::<i64, _>("authority_revision") as u64 + 1,
            grant_revision: row.get::<i64, _>("grant_revision") as u64,
        },
    };
    receipt.validate_for(q, &p).map_err(|_| refused())?;
    let request_value = value(q)?;
    let receipt_value = value(&receipt)?;
    let material_value = value(&material)?;
    let bytes:i32=sqlx::query_scalar("SELECT octet_length($1::jsonb::text)+octet_length($2::jsonb::text)+octet_length($3::jsonb::text)").bind(&request_value).bind(&receipt_value).bind(&material_value).fetch_one(&mut *tx).await.map_err(db)?;
    quota(&mut tx, auth.id(), bytes as u64, false).await?;
    if now(state)? >= timely || now(state)? >= expiry {
        return Err(refused());
    }
    super::super::closure::history::for_attempt_locked(&mut tx, &q.original.acceptance.binding)
        .await?;
    let owner = LaunchOwner {
        guardian_id: q.original.guardian_id,
        launch_id: q.original.launch_id,
        reservation_id: q.original.reservation.reservation_id,
    };
    hot.status = Status::Preparing;
    hot.owner = Some(owner.clone());
    hot.preparation_operation_id = Some(q.original.operation_id);
    insert_material(&mut tx, auth, q, &material).await?;
    sqlx::query("INSERT INTO profiled_v2_accepts(attempt_id,node_id,operation_id,request,request_digest,receipt,receipt_digest,lease_id,issued_at_ms,expires_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)").bind(q.original.acceptance.binding.attempt_id).bind(auth.id()).bind(q.original.operation_id).bind(request_value).bind(q.digest().map_err(|_|refused())?.as_str()).bind(receipt_value).bind(receipt.digest().map_err(|_|refused())?.as_str()).bind(material.lease_id).bind(time as i64).bind(expiry as i64).execute(&mut *tx).await.map_err(db)?;
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(q.original.acceptance.binding.attempt_id)
        .bind(value(&hot)?)
        .execute(&mut *tx)
        .await
        .map_err(db)?;
    super::super::journal::record_reserved(
        &mut tx,
        &q.original.acceptance.binding,
        &owner,
        q.original.journal_revision,
        &q.original.journal_digest,
    )
    .await?;
    let reply = delivery(state, q, &p, receipt, nonce)?;
    tx.commit().await.map_err(db)?;
    Ok(reply)
}
fn delivery(
    state: &AppState,
    q: &AcceptRequest,
    p: &aq_admission::preparation_v2::Proposal,
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
    d.validate_for(q, p, nonce).map_err(|_| refused())?;
    Ok(d)
}
async fn insert_material(
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    q: &AcceptRequest,
    m: &MaterialLease,
) -> Result<(), AppError> {
    let b = &q.original.acceptance.binding;
    sqlx::query("INSERT INTO admission_material_leases(lease_id,operation_id,request_digest,node_id,boot_id,prospective_attempt_id,admitted_attempt_id,offer_id,credential_identity,credential_revision,purpose,lease,verifier,state,issued_at_ms,expires_at_ms) VALUES($1,$2,$3,$4,$5,$6,$6,$7,$8,$9,$10,$11,$12,'admitted',$13,$14)").bind(m.lease_id).bind(q.original.operation_id).bind(q.digest().map_err(|_|refused())?.as_str()).bind(auth.id()).bind(q.original.identity.boot_id).bind(b.attempt_id).bind(b.offer_id).bind(format!("{}@{}",m.credential_profile.id.as_str(),m.credential_profile.version.as_str())).bind(m.credential_revision as i64).bind(m.purpose.as_str()).bind(value(m)?).bind(Digest::of(Uuid::new_v4().as_bytes()).as_str()).bind(m.issued_at_ms as i64).bind(m.expires_at_ms as i64).execute(&mut **tx).await.map_err(db)?;
    sqlx::query("INSERT INTO profiled_launch_materials(attempt_id,lease_id) VALUES($1,$2)")
        .bind(b.attempt_id)
        .bind(m.lease_id)
        .execute(&mut **tx)
        .await
        .map_err(db)?;
    Ok(())
}
