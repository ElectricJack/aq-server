use super::phase::{ChoicePins, Negotiation, Proposal};
use super::*;
use crate::{
    execution::authority::{self, Policy},
    intelligence::resolver,
    worker_api::AuthenticatedNode,
};
use aq_admission::{
    ArtifactRef, Digest, LaunchBinding, Text, execution_spec::ProfiledExecutionSpec,
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
impl ProposalRequest {
    fn validate_for_node(&self, authenticated_node: uuid::Uuid) -> Result<(), AppError> {
        self.negotiation.validate().map_err(|_| refused())?;
        self.identity
            .validate()
            .map_err(|_| AppError::BadRequest("invalid proposal identity".into()))?;
        self.binding
            .validate()
            .map_err(|_| AppError::BadRequest("invalid proposal binding".into()))?;
        // The raw proposal DTO has no TransportDocument scope validator. Fence the
        // complete authenticated identity BEFORE loading or locking any foreign owner.
        if self.identity.node_id != authenticated_node
            || !matches!(
                &self.binding.session,
                aq_admission::SessionIdentity::Remote { node_id, boot_id, attempt_id, .. }
                    if *node_id == authenticated_node
                        && *boot_id == self.identity.boot_id
                        && *attempt_id == self.binding.attempt_id
            )
        {
            return Err(AppError::Forbidden(
                "proposal assignment scope mismatch".into(),
            ));
        }
        Ok(())
    }
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
    request.validate_for_node(auth.id())?;
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
    let row = sqlx::query(
        "SELECT execution_spec,decision_id,graph_task_observation FROM profiled_launches WHERE attempt_id=$1 FOR SHARE",
    )
        .bind(request.binding.attempt_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(db)?;
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
    let selected = decision
        .outcome
        .resolved
        .as_ref()
        .ok_or_else(|| refused())?;
    let registry = registry_pin_locked(tx).await?;
    validate_registry_at_proposal(&decision, &registry)?;
    let principal = principal_pin_locked(tx, decision.decision_id).await?;
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
            // The attempt's own time window: its execution profile's wall time.
            maximum_lineage_elapsed_ms: original.offered.offer.manifest.resources.wall_time_ms,
        },
        original,
        spec,
        full_context_digest: stored.read_provenance.context_response_digest,
        owners: policy.owners,
    };
    proposal.validate().map_err(|_| refused())?;
    if proposal.policy.schema_version == 3
        && !super::super::producer::interactive_adapter(
            state,
            &proposal.original.offered.offer.manifest.harness.adapter,
        )?
    {
        return Err(refused());
    }
    if now(state)? >= proposal.original.offered.clock.expires_at_ms {
        return Err(refused());
    }
    Ok((policy_ref, proposal))
}
async fn registry_pin_locked(tx: &mut Transaction<'_, Postgres>) -> Result<ArtifactRef, AppError> {
    let reg = sqlx::query(
        "SELECT revision,config_digest FROM intelligence_registry WHERE singleton FOR SHARE",
    )
    .fetch_one(&mut **tx)
    .await
    .map_err(db)?;
    pin(
        "aq.intelligence-registry".into(),
        reg.get::<i64, _>("revision") as u64,
        serde_json::from_value(serde_json::json!(reg.get::<String, _>("config_digest")))
            .map_err(|_| refused())?,
    )
}

async fn principal_pin_locked(
    tx: &mut Transaction<'_, Postgres>,
    decision_id: uuid::Uuid,
) -> Result<ArtifactRef, AppError> {
    let principal: serde_json::Value = sqlx::query_scalar(
        "SELECT jsonb_build_object('kind',authority_kind,'actor',actor_id,'project',authority_project_id,'policy',authority_policy_ref,'revision',authority_policy_revision,'digest',authority_policy_digest) FROM intelligence_decisions WHERE decision_id=$1",
    )
        .bind(decision_id)
        .fetch_one(&mut **tx)
        .await
        .map_err(db)?;
    pin(
        format!("aq.selection-principal/{}", decision_id),
        1,
        aq_selection::digest(&principal).map_err(|_| AppError::Internal)?,
    )
}

pub async fn propose(
    state: &AppState,
    auth: &AuthenticatedNode,
    request: &ProposalRequest,
) -> Result<Proposal, AppError> {
    let _request_permit = request_permit()?;
    let mut tx = begin(state).await?;
    require_schema(&mut tx).await?;
    let (policy, proposal) = build_locked(state, &mut tx, auth, request).await?;
    if let Some(row) = sqlx::query(
        "SELECT policy,proposal,proposal_digest,node_id,boot_id FROM profiled_v2_proposals WHERE attempt_id=$1 FOR SHARE",
    )
        .bind(request.binding.attempt_id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(db)?
    {
        if row.get::<serde_json::Value, _>("policy") != value(&policy)?
            || row.get::<serde_json::Value, _>("proposal") != value(&proposal)?
            || row.get::<String, _>("proposal_digest")
                != proposal.digest().map_err(|_| refused())?.as_str()
            || row.get::<uuid::Uuid, _>("node_id") != auth.id()
            || row.get::<uuid::Uuid, _>("boot_id") != request.identity.boot_id
        {
            return Err(refused());
        }
    } else {
        let proposal_value = value(&proposal)?;
        let policy_value = value(&policy)?;
        let size: i32 = sqlx::query_scalar(
            "SELECT octet_length($1::jsonb::text)+octet_length($2::jsonb::text)",
        )
            .bind(&proposal_value)
            .bind(&policy_value)
            .fetch_one(&mut *tx)
            .await
            .map_err(db)?;
        quota(&mut tx, auth.id(), size as u64, true).await?;
        if now(state)? >= proposal.original.offered.clock.expires_at_ms {
            return Err(refused());
        }
        sqlx::query(
            "INSERT INTO profiled_v2_proposals(attempt_id,node_id,boot_id,policy,proposal,proposal_digest,published_at_ms) VALUES($1,$2,$3,$4,$5,$6,$7)",
        )
            .bind(request.binding.attempt_id)
            .bind(auth.id())
            .bind(request.identity.boot_id)
            .bind(policy_value)
            .bind(proposal_value)
            .bind(proposal.digest().map_err(|_| refused())?.as_str())
            .bind(now(state)? as i64)
            .execute(&mut *tx)
            .await
            .map_err(db)?;
    }
    tx.commit().await.map_err(db)?;
    Ok(proposal)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proposal_scope_requires_the_authenticated_node_boot_and_attempt() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../aq-admission/artifacts/preparation-v2.fixture.json"
        ))
        .unwrap();
        let proposal = &fixture["proposal"];
        let request = ProposalRequest {
            identity: serde_json::from_value(fixture["accept"]["original"]["identity"].clone())
                .unwrap(),
            binding: serde_json::from_value(
                proposal["original"]["offered"]["offer"]["binding"].clone(),
            )
            .unwrap(),
            negotiation: Negotiation {
                contract: phase::contract_artifact().unwrap(),
                features: phase::required_features(),
            },
        };
        let authenticated_node = request.identity.node_id;
        request.validate_for_node(authenticated_node).unwrap();
        assert!(request.validate_for_node(uuid::Uuid::new_v4()).is_err());
        for field in ["node", "boot", "attempt"] {
            let mut changed = request.clone();
            let aq_admission::SessionIdentity::Remote {
                node_id,
                boot_id,
                attempt_id,
                ..
            } = &mut changed.binding.session
            else {
                panic!("remote proposal fixture");
            };
            match field {
                "node" => *node_id = uuid::Uuid::new_v4(),
                "boot" => *boot_id = uuid::Uuid::new_v4(),
                _ => *attempt_id = uuid::Uuid::new_v4(),
            }
            assert!(
                changed.validate_for_node(authenticated_node).is_err(),
                "{field}"
            );
        }
    }
}
