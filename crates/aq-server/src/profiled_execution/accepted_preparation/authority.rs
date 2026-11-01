//! Actual immutable phase provenance. Never manufactures a legacy plan.
use super::*;
use crate::{
    execution::{
        authority::{self, Policy, hot::HotState},
        profiled::Status,
    },
    worker_api::AuthenticatedNode,
};
use aq_admission::{
    LaunchBinding,
    preparation_v2::{AcceptRequest, AcceptedReceipt, Proposal},
};
use sqlx::Row;
pub(super) async fn proposal_locked(
    tx: &mut Transaction<'_, Postgres>,
    binding: &LaunchBinding,
) -> Result<Proposal, AppError> {
    let row = sqlx::query("SELECT * FROM profiled_v2_proposals WHERE attempt_id=$1 FOR SHARE")
        .bind(binding.attempt_id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?
        .ok_or_else(refused)?;
    let p: Proposal = serde_json::from_value(row.get("proposal")).map_err(|_| refused())?;
    p.validate().map_err(|_| refused())?;
    if (row.get::<i64, _>("published_at_ms") as u64) < p.original.offered.clock.server_time_ms
        || (row.get::<i64, _>("published_at_ms") as u64) >= p.original.offered.clock.expires_at_ms
        || p.original.offered.offer.binding != *binding
        || p.digest().map_err(|_| refused())?.as_str() != row.get::<String, _>("proposal_digest")
        || !matches!(binding.session,aq_admission::SessionIdentity::Remote{node_id,boot_id,..} if row.get::<uuid::Uuid,_>("node_id")==node_id&&row.get::<uuid::Uuid,_>("boot_id")==boot_id)
    {
        return Err(refused());
    }
    Ok(p)
}
pub(super) async fn current_policy_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    p: &Proposal,
) -> Result<(), AppError> {
    let binding = &p.original.offered.offer.binding;
    let runtime = super::super::registration::current_runtime_locked(
        state,
        tx,
        auth.id(),
        match binding.session {
            aq_admission::SessionIdentity::Remote { boot_id, .. } => boot_id,
            _ => return Err(refused()),
        },
    )
    .await?;
    let (pin, policy) = policy_for(state, &runtime.runtime, &runtime.qualification)?;
    let stored: serde_json::Value = sqlx::query_scalar(
        "SELECT policy FROM profiled_v2_proposals WHERE attempt_id=$1 FOR SHARE",
    )
    .bind(binding.attempt_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    if stored != value(&pin)?
        || value(&p.policy)? != value(&policy.phase)?
        || value(&p.owners)? != value(&policy.owners)?
    {
        return Err(refused());
    }
    let config = state
        .config
        .profiled_execution
        .as_ref()
        .ok_or_else(refused)?;
    super::super::policies::fence_configuration_locked(tx, config).await?;
    Ok(())
}
pub(super) async fn accepted_locked(
    tx: &mut Transaction<'_, Postgres>,
    p: &Proposal,
    q: &AcceptRequest,
) -> Result<Option<AcceptedReceipt>, AppError> {
    let rows=sqlx::query("SELECT * FROM profiled_v2_accepts WHERE attempt_id=$1 OR (node_id=$2 AND operation_id=$3) ORDER BY attempt_id LIMIT 2 FOR SHARE").bind(q.original.acceptance.binding.attempt_id).bind(q.original.identity.node_id).bind(q.original.operation_id).fetch_all(&mut **tx).await.map_err(db)?;
    if rows.is_empty() {
        return Ok(None);
    }
    if rows.len() != 1 {
        return Err(refused());
    }
    let row = &rows[0];
    let a: AcceptedReceipt = serde_json::from_value(row.get("receipt")).map_err(|_| refused())?;
    a.validate_for(q, p).map_err(|_| refused())?;
    if row.get::<serde_json::Value, _>("request") != value(q)?
        || row.get::<String, _>("request_digest") != q.digest().map_err(|_| refused())?.as_str()
        || row.get::<String, _>("receipt_digest") != a.digest().map_err(|_| refused())?.as_str()
        || row.get::<uuid::Uuid, _>("operation_id") != q.original.operation_id
        || row.get::<uuid::Uuid, _>("node_id") != q.original.identity.node_id
        || row.get::<uuid::Uuid, _>("attempt_id") != a.lease.binding.attempt_id
        || row.get::<uuid::Uuid, _>("lease_id") != a.lease.materials[0].lease_id
        || row.get::<i64, _>("issued_at_ms") as u64 != a.lease.issued_at_ms
        || row.get::<i64, _>("expires_at_ms") as u64 != a.lease.expires_at_ms
    {
        return Err(refused());
    }
    Ok(Some(a))
}
pub(super) async fn live_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    p: &Proposal,
    q: &AcceptRequest,
    a: &AcceptedReceipt,
) -> Result<(), AppError> {
    crate::worker_api::lock_current_node(tx, auth).await?;
    let b = &a.lease.binding;
    let view = authority::load_locked(state, tx, &b.session, Policy::IdentityOnly).await?;
    let raw: serde_json::Value = sqlx::query_scalar(
        "SELECT state FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE",
    )
    .bind(b.attempt_id)
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    let hot: HotState = serde_json::from_value(raw).map_err(|_| refused())?;
    hot.validate_against(b).map_err(|_| refused())?;
    if !view.holds_task
        || view.revoked
        || view.stopped
        || view.cancellation_id.is_some()
        || view.status != Status::Preparing
        || hot.grant.is_some()
        || hot.preparation_operation_id != Some(q.original.operation_id)
        || view.offer_id != b.offer_id
        || view.execution_spec_digest != b.execution_spec_digest.as_str()
        || view.launch_manifest_digest.as_ref() != Some(&b.launch_manifest_digest)
        || hot.owner.as_ref().is_none_or(|o| {
            o.guardian_id != a.lease.guardian_id
                || o.launch_id != a.lease.launch_id
                || o.reservation_id != a.lease.hold.reservation.reservation_id
        })
    {
        return Err(refused());
    }
    current_policy_locked(state, tx, auth, p).await?;
    let reg = super::super::registration::admitted_runtime_locked(
        tx,
        auth.id(),
        q.original.identity.boot_id,
    )
    .await?;
    if reg.request.identity != q.original.identity {
        return Err(refused());
    }
    let project = sqlx::query(
        "SELECT revision,binding FROM project_execution_bindings WHERE project_id=$1 FOR SHARE",
    )
    .bind(view.project_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(db)?
    .ok_or_else(refused)?;
    let binding: crate::project_bindings::domain::ProjectBinding =
        serde_json::from_value(project.get("binding")).map_err(|_| refused())?;
    if !binding.enabled
        || !binding.eligible_node(&state.config.projects, auth.id())
        || project.get::<i64, _>("revision") as u64 != p.spec.project_revision
        || binding.repository_id != p.spec.source.repository_id
        || binding.repository_binding_generation != p.spec.source.repository_binding_generation
        || state
            .config
            .profiled_execution
            .as_ref()
            .and_then(|c| c.projects.iter().find(|x| x.project_id == view.project_id))
            .is_none_or(|x| !x.enabled)
    {
        return Err(refused());
    }
    crate::intelligence::fence_current_registry(state, tx).await?;
    crate::intelligence::resolver::fence_preparation_input_principal(state, tx, b.attempt_id)
        .await?;
    material_locked(tx, q, a).await?;
    let time = now(state)?;
    if time < a.lease.issued_at_ms || time >= a.lease.expires_at_ms {
        return Err(refused());
    }
    Ok(())
}

async fn material_locked(
    tx: &mut Transaction<'_, Postgres>,
    q: &AcceptRequest,
    a: &AcceptedReceipt,
) -> Result<(), AppError> {
    let actual=sqlx::query("SELECT m.* FROM admission_material_leases m JOIN profiled_launch_materials l ON l.lease_id=m.lease_id AND l.attempt_id=$2 WHERE m.lease_id=$1 FOR SHARE OF m,l").bind(a.lease.materials[0].lease_id).bind(a.lease.binding.attempt_id).fetch_optional(&mut **tx).await.map_err(db)?.ok_or_else(refused)?;
    let m = &a.lease.materials[0];
    if actual.get::<uuid::Uuid, _>("prospective_attempt_id") != a.lease.binding.attempt_id
        || actual.get::<uuid::Uuid, _>("offer_id") != a.lease.binding.offer_id
        || actual.get::<String, _>("credential_identity")
            != format!(
                "{}@{}",
                m.credential_profile.id.as_str(),
                m.credential_profile.version.as_str()
            )
        || actual.get::<i64, _>("credential_revision") as u64 != m.credential_revision
        || actual.get::<String, _>("purpose") != m.purpose.as_str()
        || actual.get::<serde_json::Value, _>("lease") != value(&a.lease.materials[0])?
        || actual.get::<String, _>("state") != "admitted"
        || actual.get::<uuid::Uuid, _>("node_id") != q.original.identity.node_id
        || actual.get::<uuid::Uuid, _>("boot_id") != q.original.identity.boot_id
        || actual.get::<uuid::Uuid, _>("operation_id") != q.original.operation_id
        || actual.get::<String, _>("request_digest") != q.digest().map_err(|_| refused())?.as_str()
        || actual.get::<Option<uuid::Uuid>, _>("admitted_attempt_id")
            != Some(a.lease.binding.attempt_id)
        || actual.get::<i64, _>("issued_at_ms") as u64 != a.lease.materials[0].issued_at_ms
        || actual.get::<i64, _>("expires_at_ms") as u64 != a.lease.materials[0].expires_at_ms
    {
        return Err(refused());
    }
    Ok(())
}

/// Immutable historical origin, with current live retrieval fencing performed by
/// the loader. This is not execution authority and cannot reconstruct bytes.
enum OriginalPreparation {
    Legacy {
        plan: Box<aq_admission::transport::PreparationPlan>,
        receipt_digest: aq_admission::Digest,
    },
    AcceptedV2 {
        proposal: Box<Proposal>,
        request: Box<AcceptRequest>,
        receipt: Box<AcceptedReceipt>,
        receipt_digest: aq_admission::Digest,
    },
}
/// Opaque loaded provenance: DTOs exposed by accessors remain historical data,
/// not execution authority. Only the authenticated locked loader constructs it.
pub struct PreparationAuthority {
    original: OriginalPreparation,
}
impl PreparationAuthority {
    pub fn origin(&self) -> (&'static str, &aq_admission::Digest) {
        match &self.original {
            OriginalPreparation::Legacy { receipt_digest, .. } => ("accept", receipt_digest),
            OriginalPreparation::AcceptedV2 { receipt_digest, .. } => {
                ("accepted_preparation_v2", receipt_digest)
            }
        }
    }
    pub fn phase_context(&self) -> Option<(&Proposal, &AcceptRequest)> {
        match &self.original {
            OriginalPreparation::Legacy { .. } => None,
            OriginalPreparation::AcceptedV2 {
                proposal, request, ..
            } => Some((proposal, request)),
        }
    }

    pub fn accepted_receipt(&self) -> Option<&AcceptedReceipt> {
        match &self.original {
            OriginalPreparation::Legacy { .. } => None,
            OriginalPreparation::AcceptedV2 { receipt, .. } => Some(receipt),
        }
    }
    pub fn window(&self) -> (u64, u64) {
        match &self.original {
            OriginalPreparation::Legacy { plan, .. } => {
                (plan.clock.server_time_ms, plan.clock.expires_at_ms)
            }
            OriginalPreparation::AcceptedV2 { receipt, .. } => {
                (receipt.lease.issued_at_ms, receipt.lease.expires_at_ms)
            }
        }
    }
    pub fn materials(&self) -> &[aq_admission::MaterialLease] {
        match &self.original {
            OriginalPreparation::Legacy { plan, .. } => &plan.materials,
            OriginalPreparation::AcceptedV2 { receipt, .. } => &receipt.lease.materials,
        }
    }
}
/// Caller already holds authenticated node and exact attempt/hot locks. Legacy
/// bytes are never converted into v2, nor v2 into a legacy plan.
pub(crate) async fn load_preparing_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    b: &LaunchBinding,
    hot: &HotState,
) -> Result<PreparationAuthority, AppError> {
    let operation = hot.preparation_operation_id.ok_or_else(refused)?;
    let owner = hot.owner.as_ref().ok_or_else(refused)?;
    if hot.status != Status::Preparing || hot.grant.is_some() {
        return Err(refused());
    }
    if let Some(raw)=sqlx::query_scalar::<_,serde_json::Value>("SELECT response FROM profiled_execution_receipts WHERE node_id=$1 AND operation_id=$2 AND kind='accept' FOR SHARE").bind(auth.id()).bind(operation).fetch_optional(&mut **tx).await.map_err(db)?{
  let doc:aq_admission::transport::TransportDocument=serde_json::from_value(raw.clone()).map_err(|_|refused())?;doc.validate().map_err(|_|refused())?;
  let aq_admission::transport::TransportDocument::PreparationPlan(plan)=doc else{return Err(refused())};
  if plan.binding!=*b||plan.operation_id!=operation||plan.guardian_id!=owner.guardian_id||plan.launch_id!=owner.launch_id||plan.reservation.reservation_id!=owner.reservation_id{return Err(refused())}
  return Ok(PreparationAuthority{original:OriginalPreparation::Legacy{plan:Box::new(plan),receipt_digest:aq_selection::digest(&raw).map_err(|_|AppError::Internal)?}})
 }
    if !super::policy::enabled(state) {
        return Err(refused());
    }
    let p = proposal_locked(tx, b).await?;
    let raw:serde_json::Value=sqlx::query_scalar("SELECT request FROM profiled_v2_accepts WHERE attempt_id=$1 AND node_id=$2 AND operation_id=$3 FOR SHARE").bind(b.attempt_id).bind(auth.id()).bind(operation).fetch_optional(&mut **tx).await.map_err(db)?.ok_or_else(refused)?;
    let q: AcceptRequest = serde_json::from_value(raw).map_err(|_| refused())?;
    let a = accepted_locked(tx, &p, &q).await?.ok_or_else(refused)?;
    live_locked(state, tx, auth, &p, &q, &a).await?;
    let digest = a.digest().map_err(|_| refused())?;
    Ok(PreparationAuthority {
        original: OriginalPreparation::AcceptedV2 {
            proposal: Box::new(p),
            request: Box::new(q),
            receipt: Box::new(a),
            receipt_digest: digest,
        },
    })
}
