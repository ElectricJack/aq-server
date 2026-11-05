//! Bounded cold verification of exact immutable inputs plus current authority.
//! No original observation is retimed and no selector is invoked here.
use crate::{
    AppState,
    error::AppError,
    execution::{
        authority::{self, Kind, Policy, hot::HotState},
        profiled::Status,
    },
    profiled_execution::{
        artifacts,
        initial_grant::{self, OriginalGrant},
        registration, source_exports,
    },
    project_bindings::domain::ProjectRecord,
    worker_api::AuthenticatedNode,
};
use aq_admission::{
    Digest, MaterialLease, MaterialReceipt, ProfiledOffer,
    control::Versions,
    execution_spec::{ProfiledExecutionSpec, decode_profiled_spec},
    transport::Renew,
};
use aq_profiles::SelectionPolicy;
use aq_selection::{ExecutionTarget, ResolvedSelection};
use aq_session_protocol::Validate;
use serde_json::Value;
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;
pub struct Pinned {
    pub offer: ProfiledOffer,
    pub spec: ProfiledExecutionSpec,
    pub selection: ResolvedSelection,
    pub original: OriginalGrant,
    pub project: ProjectRecord,
    pub decision_id: Uuid,
    pub source: source_exports::LinkedExport,
    pub context: Value,
    pub policy: SelectionPolicy,
    pub remaining_lineage_ms: u64,
    pub installation: MaterialReceipt,
    pub material: MaterialLease,
    pub hot: HotState,
    pub versions: Versions,
}
fn invalid() -> AppError {
    AppError::Conflict("pinned continuing execution authority is not live or consistent".into())
}
fn json(value: &impl serde::Serialize) -> Result<Value, AppError> {
    serde_json::to_value(value).map_err(|_| AppError::Internal)
}
fn same(a: &impl serde::Serialize, b: &impl serde::Serialize) -> Result<bool, AppError> {
    Ok(json(a)? == json(b)?)
}
pub async fn load_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    request: &Renew,
) -> Result<Pinned, AppError> {
    crate::worker_api::lock_current_node(tx, auth).await?;
    if auth.id() != request.identity.node_id {
        return Err(invalid());
    }
    registration::admitted_runtime_locked(tx, auth.id(), request.identity.boot_id).await?;
    let view = authority::load_locked(state, tx, &request.binding.session, Policy::IdentityOnly)
        .await
        .map_err(AppError::from)?;
    if view.kind != Kind::ProfiledV1
        || view.grant_id != Some(request.grant_id)
        || view.identity.node_id != auth.id()
        || view.identity.boot_id != request.identity.boot_id
    {
        return Err(invalid());
    }
    let row=sqlx::query("SELECT state,record_revision,authority_revision,grant_revision FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE").bind(request.binding.attempt_id).fetch_one(&mut **tx).await.map_err(|_|AppError::Internal)?;
    let hot: HotState = serde_json::from_value(row.get("state")).map_err(|_| invalid())?;
    hot.validate_against(&request.binding)
        .map_err(|_| invalid())?;
    let versions = Versions {
        record_revision: row.get::<i64, _>("record_revision") as u64,
        authority_revision: row.get::<i64, _>("authority_revision") as u64,
        grant_revision: row.get::<i64, _>("grant_revision") as u64,
    };
    versions.validate().map_err(|_| invalid())?;
    let owner = hot.owner.as_ref().ok_or_else(invalid)?;
    let current = hot.grant.as_ref().ok_or_else(invalid)?;
    let time = super::now(state)?;
    if owner.guardian_id != request.guardian_id
        || owner.launch_id != request.launch_id
        || owner.reservation_id != request.reservation_id
        || current.grant_id != request.grant_id
        || current.binding != request.binding
    {
        return Err(invalid());
    }
    if hot.cancellation_id.is_some() {
        return Err(AppError::ExecutionCancelled);
    }
    if !view.holds_task
        || view.status != Status::Granted
        || view.revoked
        || view.stopped
        || time >= current.expires_at_ms
        || hot.execution_deadline_ms.is_none_or(|d| time >= d)
    {
        return Err(invalid());
    }
    registration::current_runtime_locked(state, tx, auth.id(), request.identity.boot_id).await?;
    let agent = sqlx::query("SELECT binding,profile,context,revoked,node_id FROM agent_launch_bindings WHERE attempt_id=$1 FOR SHARE")
        .bind(request.binding.attempt_id).fetch_optional(&mut **tx).await.map_err(|_|AppError::Internal)?.ok_or_else(invalid)?;
    let graph = serde_json::to_value(view.graph.as_ref().ok_or_else(invalid)?)
        .map_err(|_| AppError::Internal)?;
    let expected = serde_json::json!({"task_id":view.task_id,"attempt_id":view.attempt.attempt_id,"session_id":view.attempt.session_id,"generation":view.attempt.generation,"node_id":view.identity.node_id,"boot_id":view.identity.boot_id,"project_id":view.project_id,"workspace_id":graph["workspace_id"],"resolved_spec_digest":view.execution_spec_digest,"profile_digest":graph["profile_digest"],"context_digest":graph["context_digest"]});
    let profile: Value = agent.get("profile");
    let context: Value = agent.get("context");
    if agent.get::<bool, _>("revoked")
        || agent.get::<Uuid, _>("node_id") != auth.id()
        || agent.get::<Value, _>("binding") != expected
        || !aq_agent_api::valid("ResolvedProfile", &profile)
        || !aq_agent_api::valid("ContextSnapshot", &context)
        || aq_selection::digest(&profile)
            .map_err(|_| AppError::Internal)?
            .as_str()
            != graph["profile_digest"].as_str().ok_or_else(invalid)?
        || aq_selection::digest(&context)
            .map_err(|_| AppError::Internal)?
            .as_str()
            != graph["context_digest"].as_str().ok_or_else(invalid)?
    {
        return Err(invalid());
    }
    let original = initial_grant::load_original_grant_locked(
        tx,
        auth.id(),
        &request.binding,
        request.grant_id,
    )
    .await?;
    if original.body.guardian_id != owner.guardian_id
        || original.body.launch_id != owner.launch_id
        || original.body.reservation_id != owner.reservation_id
        || hot.execution_deadline_ms != Some(original.execution_deadline_ms)
        || original.body.materials.len() != 1
    {
        return Err(invalid());
    }
    let material = original.body.materials[0].clone();
    let installation = initial_grant::load_installed_receipt_locked(
        tx,
        &material,
        original.body.clock.server_time_ms,
    )
    .await?;
    let row=sqlx::query("SELECT l.launch_binding,l.launch_manifest,l.execution_spec,l.execution_spec_digest,l.graph_task_observation#>'{native_offer,offered,offer,required_capabilities}' AS required,d.decision#>'{project,project}' AS project,l.decision_id,s.selection,s.selection_digest FROM profiled_launches l JOIN intelligence_decisions d ON d.decision_id=l.decision_id JOIN intelligence_attempt_selections s USING(attempt_id) WHERE l.attempt_id=$1 FOR SHARE OF l,d,s").bind(request.binding.attempt_id).fetch_optional(&mut **tx).await.map_err(|_|AppError::Internal)?.ok_or_else(invalid)?;
    if row.get::<Value, _>("launch_binding") != json(&request.binding)? {
        return Err(invalid());
    }
    let offer = ProfiledOffer {
        binding: request.binding.clone(),
        manifest: serde_json::from_value(row.get("launch_manifest")).map_err(|_| invalid())?,
        required_capabilities: serde_json::from_value(row.get("required"))
            .map_err(|_| invalid())?,
    };
    offer.validate().map_err(|_| invalid())?;
    let spec_value: Value = row.get("execution_spec");
    let mut canonical = spec_value;
    canonical.sort_all_objects();
    let bytes = serde_json::to_vec(&canonical).map_err(|_| AppError::Internal)?;
    if bytes.len() > 65536
        || Digest::of(&bytes) != request.binding.execution_spec_digest
        || row.get::<String, _>("execution_spec_digest")
            != request.binding.execution_spec_digest.as_str()
    {
        return Err(invalid());
    }
    let spec = decode_profiled_spec(&bytes).map_err(|_| invalid())?;
    spec.validate_against(&offer).map_err(|_| invalid())?;
    let selection: ResolvedSelection =
        serde_json::from_value(row.get("selection")).map_err(|_| invalid())?;
    if aq_selection::digest(&selection).map_err(|_| AppError::Internal)? != spec.selection_digest
        || row.get::<String, _>("selection_digest") != spec.selection_digest.as_str()
        || selection.target
            != (ExecutionTarget::Worker {
                node_id: auth.id(),
                boot_id: request.identity.boot_id,
            })
        || selection.lineage.project_id != view.project_id
        || selection.lineage.task_id != view.task_id
        || selection.lineage.workspace_id != spec.task.workspace_id
    {
        return Err(invalid());
    }
    let project: ProjectRecord =
        serde_json::from_value(row.get("project")).map_err(|_| invalid())?;
    if project.revision != spec.project_revision
        || project.revision != selection.project_revision
        || project.binding.project_id != selection.lineage.project_id
        || material.credential_profile != selection.choice.credential_profile
        || material.credential_revision != selection.credential_revision
        || material.provider.as_str() != selection.choice.provider
        || material.account_reference.as_str() != selection.choice.account_reference
        || material.endpoint_reference.as_str() != selection.choice.endpoint_reference
    {
        return Err(invalid());
    }
    let source = source_exports::load_linked_locked(tx, request.binding.attempt_id).await?;
    if source.manifest().attempt != view.attempt
        || source.manifest().source != view.source.ok_or_else(invalid)?
        || source.manifest().resolved_spec_digest != view.execution_spec_digest
        || source.token().request().project_revision != spec.project_revision
    {
        return Err(invalid());
    }
    let refs=sqlx::query_scalar::<_,Uuid>("SELECT a.artifact_id FROM profiled_launch_artifacts x JOIN admission_artifacts a USING(artifact_id) WHERE x.attempt_id=$1 AND a.descriptor->>'kind'='context' AND a.descriptor#>>'{artifact,id}'=$2 AND a.descriptor#>>'{artifact,version}'=$3 ORDER BY a.artifact_id LIMIT 2").bind(request.binding.attempt_id).bind(spec.task_context.id.as_str()).bind(spec.task_context.version.as_str()).fetch_all(&mut **tx).await.map_err(|_|AppError::Internal)?;
    if refs.len() != 1 {
        return Err(invalid());
    }
    let retained = artifacts::load_locked(tx, refs[0]).await?;
    if retained.project_id != view.project_id || retained.descriptor.artifact != spec.task_context {
        return Err(invalid());
    }
    retained
        .descriptor
        .verify(&retained.bytes, retained.descriptor.retained_at_ms)
        .map_err(|_| invalid())?;
    let context: Value = serde_json::from_slice(&retained.bytes).map_err(|_| invalid())?;
    if !aq_agent_api::valid("ContextSnapshot", &context) {
        return Err(invalid());
    }
    let (policy, remaining_lineage_ms) =
        crate::profiled_execution::continuation_policy::load_locked(tx, &request.binding).await?;
    if policy.budget.max_cost_microunits.is_some()
        || policy.budget.require_complete_accounting
        || policy.budget.require_hard_cost_enforcement
        || project.binding.ceilings.cost_limit_microunits.is_some()
        || project.binding.ceilings.require_hard_cost_enforcement
    {
        return Err(AppError::Conflict(
            "qualified continuing cost accounting is not available".into(),
        ));
    }
    let record=sqlx::query("SELECT m.state,m.lease FROM admission_material_leases m JOIN profiled_launch_materials x ON x.lease_id=m.lease_id AND x.attempt_id=$3 WHERE m.lease_id=$1 AND m.node_id=$2 AND m.admitted_attempt_id=$3 FOR SHARE OF m,x").bind(material.lease_id).bind(auth.id()).bind(request.binding.attempt_id).fetch_optional(&mut **tx).await.map_err(|_|AppError::Internal)?.ok_or_else(invalid)?;
    if record.get::<String, _>("state") != "admitted"
        || record.get::<Value, _>("lease") != json(&material)?
    {
        return Err(invalid());
    }
    let counts =
        crate::profiled_execution::no_grant::attempt_counts_locked(tx, view.task_id).await?;
    if counts.started != 1 {
        return Err(AppError::Conflict(
            "continuing lineage budget is not fully known for this profile".into(),
        ));
    }
    Ok(Pinned {
        offer,
        spec,
        selection,
        original,
        project,
        decision_id: row.get("decision_id"),
        source,
        context,
        policy,
        remaining_lineage_ms,
        installation,
        material,
        hot,
        versions,
    })
}
pub fn same_immutable(a: &Pinned, b: &Pinned) -> Result<bool, AppError> {
    Ok(a.offer.binding == b.offer.binding
        && same(&a.original.body, &b.original.body)?
        && same(&a.original.versions, &b.original.versions)?
        && a.original.execution_deadline_ms == b.original.execution_deadline_ms
        && a.original.operation_id == b.original.operation_id
        && a.original.request_digest == b.original.request_digest
        && a.spec.selection_digest == b.spec.selection_digest
        && same(&a.installation, &b.installation)?
        && same(&a.material, &b.material)?
        && a.source.token().request_digest() == b.source.token().request_digest()
        && a.source.fact().manifest_digest() == b.source.fact().manifest_digest()
        && a.project.revision == b.project.revision
        && a.decision_id == b.decision_id)
}
