use super::*;
use crate::{
    execution::authority::{self, Policy},
    intelligence::resolver,
    worker_api::AuthenticatedNode,
};
use aq_admission::{
    ArtifactRef, Digest, LaunchBinding, Text,
    execution_spec::ProfiledExecutionSpec,
    preparation_v2::{ChoicePins, Negotiation, Proposal},
    transport::WorkerIdentity,
};
use aq_session_protocol::Validate;
use serde::{Deserialize, Serialize};
use sqlx::Row;
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProposalRequest {
    pub identity: WorkerIdentity,
    pub binding: LaunchBinding,
    pub negotiation: Negotiation,
}
fn pin(id: String, version: u64, digest: Digest) -> Result<ArtifactRef, AppError> {
    Ok(ArtifactRef {
        id: Text::new(id).map_err(|_| refused())?,
        version: Text::new(version.to_string()).map_err(|_| refused())?,
        digest,
    })
}
pub(super) async fn build_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    request: &ProposalRequest,
) -> Result<(ArtifactRef, Proposal), AppError> {
    request.negotiation.validate().map_err(|_| refused())?;
    request
        .identity
        .validate()
        .map_err(|_| AppError::BadRequest("invalid proposal identity".into()))?;
    request
        .binding
        .validate()
        .map_err(|_| AppError::BadRequest("invalid proposal binding".into()))?;
    // The raw proposal DTO has no TransportDocument scope validator. Fence the
    // complete authenticated identity BEFORE loading or locking any foreign owner.
    if request.identity.node_id != auth.id()
        || !matches!(&request.binding.session,aq_admission::SessionIdentity::Remote{node_id,boot_id,attempt_id,..}
            if *node_id==auth.id() && *boot_id==request.identity.boot_id && *attempt_id==request.binding.attempt_id)
    {
        return Err(AppError::Forbidden(
            "proposal assignment scope mismatch".into(),
        ));
    }
    crate::worker_api::lock_current_node(tx, auth).await?;
    let runtime = super::super::registration::current_runtime_locked(
        state,
        tx,
        auth.id(),
        request.identity.boot_id,
    )
    .await?;
    let registered = super::super::registration::admitted_runtime_locked(
        tx,
        auth.id(),
        request.identity.boot_id,
    )
    .await?;
    if registered.request.identity != request.identity {
        return Err(refused());
    }
    let (policy_ref, policy) = policy_for(state, &runtime.runtime, &runtime.qualification)?;
    let view =
        authority::load_locked(state, tx, &request.binding.session, Policy::IdentityOnly).await?;
    if !view.holds_task
        || view.revoked
        || view.stopped
        || view.cancellation_id.is_some()
        || view.grant_id.is_some()
        || view.status != crate::execution::profiled::Status::Offered
    {
        return Err(refused());
    }
    let original =
        super::super::native_offer::load_original_offer_locked(tx, request.binding.attempt_id)
            .await?;
    if original.offered.offer.binding != request.binding {
        return Err(refused());
    }
    let row=sqlx::query("SELECT execution_spec,decision_id,graph_task_observation FROM profiled_launches WHERE attempt_id=$1 FOR SHARE").bind(request.binding.attempt_id).fetch_one(&mut **tx).await.map_err(db)?;
    let spec: ProfiledExecutionSpec =
        serde_json::from_value(row.get("execution_spec")).map_err(|_| refused())?;
    let stored: super::super::native_offer::observation::StoredOffer =
        serde_json::from_value(row.get("graph_task_observation")).map_err(|_| refused())?;
    let decision: resolver::Decision = serde_json::from_value(
        sqlx::query_scalar::<_, serde_json::Value>(
            "SELECT decision FROM intelligence_decisions WHERE decision_id=$1 FOR SHARE",
        )
        .bind(row.get::<uuid::Uuid, _>("decision_id"))
        .fetch_one(&mut **tx)
        .await
        .map_err(db)?,
    )
    .map_err(|_| refused())?;
    resolver::fence_for_preparation(state, tx, &decision, &request.binding).await?;
    let selected = decision.outcome.resolved.as_ref().ok_or_else(refused)?;
    let reg = sqlx::query(
        "SELECT revision,config_digest FROM intelligence_registry WHERE singleton FOR SHARE",
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    let registry = pin(
        "aq.intelligence-registry".into(),
        reg.get::<i64, _>("revision") as u64,
        serde_json::from_value(serde_json::json!(reg.get::<String, _>("config_digest")))
            .map_err(|_| refused())?,
    )?;
    let principal:serde_json::Value=sqlx::query_scalar("SELECT jsonb_build_object('kind',authority_kind,'actor',actor_id,'project',authority_project_id,'policy',authority_policy_ref,'revision',authority_policy_revision,'digest',authority_policy_digest) FROM intelligence_decisions WHERE decision_id=$1").bind(decision.decision_id).fetch_one(&mut **tx).await.map_err(db)?;
    let principal = pin(
        format!("aq.selection-principal/{}", decision.decision_id),
        1,
        aq_selection::digest(&principal).map_err(|_| AppError::Internal)?,
    )?;
    let policy_budget =
        super::super::continuation_policy::load_locked(tx, &request.binding).await?;
    let proposal = Proposal {
        schema_version: 2,
        negotiation: request.negotiation.clone(),
        policy: policy.phase,
        choice: ChoicePins {
            selection_digest: aq_selection::digest(selected).map_err(|_| AppError::Internal)?,
            registry,
            principal,
            permission_digest: aq_selection::digest(&original.offered.offer.manifest.permissions)
                .map_err(|_| AppError::Internal)?,
            credential_profile: selected.choice.credential_profile.clone(),
            credential_revision: selected.credential_revision,
            account_reference: Text::new(&selected.choice.account_reference)
                .map_err(|_| refused())?,
            endpoint_reference: Text::new(&selected.choice.endpoint_reference)
                .map_err(|_| refused())?,
            model: original.offered.offer.manifest.model.clone(),
            harness: original.offered.offer.manifest.harness.clone(),
            maximum_lineage_elapsed_ms: policy_budget.0.max_lineage_elapsed_ms,
        },
        original,
        spec,
        full_context_digest: stored.read_provenance.context_response_digest,
        owners: policy.owners,
    };
    proposal.validate().map_err(|_| refused())?;
    if now(state)? >= proposal.original.offered.clock.expires_at_ms {
        return Err(refused());
    }
    Ok((policy_ref, proposal))
}
pub async fn propose(
    state: &AppState,
    auth: &AuthenticatedNode,
    request: &ProposalRequest,
) -> Result<Proposal, AppError> {
    let _request_permit = request_permit()?;
    let mut tx = begin(state).await?;
    require_schema(&mut tx).await?;
    let (policy, p) = build_locked(state, &mut tx, auth, request).await?;
    if let Some(row)=sqlx::query("SELECT policy,proposal,proposal_digest,node_id,boot_id FROM profiled_v2_proposals WHERE attempt_id=$1 FOR SHARE").bind(request.binding.attempt_id).fetch_optional(&mut *tx).await.map_err(db)?{
  if row.get::<serde_json::Value,_>("policy")!=value(&policy)? ||row.get::<serde_json::Value,_>("proposal")!=value(&p)?||row.get::<String,_>("proposal_digest")!=p.digest().map_err(|_|refused())?.as_str()||row.get::<uuid::Uuid,_>("node_id")!=auth.id()||row.get::<uuid::Uuid,_>("boot_id")!=request.identity.boot_id{return Err(refused())}
 }else{
  let v=value(&p)?;let pol=value(&policy)?;
  let size:i32=sqlx::query_scalar("SELECT octet_length($1::jsonb::text)+octet_length($2::jsonb::text)").bind(&v).bind(&pol).fetch_one(&mut *tx).await.map_err(db)?;
  quota(&mut tx,auth.id(),size as u64,true).await?;
  if now(state)? >=p.original.offered.clock.expires_at_ms{return Err(refused())}
  sqlx::query("INSERT INTO profiled_v2_proposals(attempt_id,node_id,boot_id,policy,proposal,proposal_digest,published_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7)").bind(request.binding.attempt_id).bind(auth.id()).bind(request.identity.boot_id).bind(pol).bind(v).bind(p.digest().map_err(|_|refused())?.as_str()).bind(now(state)? as i64).execute(&mut *tx).await.map_err(db)?;
 }
    tx.commit().await.map_err(db)?;
    Ok(p)
}
