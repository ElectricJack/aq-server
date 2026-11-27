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
    pub installation: MaterialReceipt,
    pub material: MaterialLease,
    pub hot: HotState,
    pub versions: Versions,
}
fn invalid() -> AppError {
    AppError::Conflict("pinned continuing execution authority is not live or consistent".into())
}
fn ensure(consistent: bool) -> Result<(), AppError> {
    if consistent { Ok(()) } else { Err(invalid()) }
}
fn json(value: &impl serde::Serialize) -> Result<Value, AppError> {
    serde_json::to_value(value).map_err(|_| AppError::Internal)
}
fn same(a: &impl serde::Serialize, b: &impl serde::Serialize) -> Result<bool, AppError> {
    Ok(json(a)? == json(b)?)
}
/// Revisions as returned by a `profiled_authority_hot` read or update.
pub(super) fn versions(row: &sqlx::postgres::PgRow) -> Versions {
    let revision = |column: &str| row.get::<i64, _>(column) as u64;
    Versions {
        record_revision: revision("record_revision"),
        authority_revision: revision("authority_revision"),
        grant_revision: revision("grant_revision"),
    }
}
pub async fn load_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    request: &Renew,
) -> Result<Pinned, AppError> {
    let (view, hot, versions) = load_live_authority_locked(state, tx, auth, request).await?;
    verify_agent_binding_locked(tx, auth.id(), request, &view).await?;
    let original = initial_grant::load_original_grant_locked(
        tx,
        auth.id(),
        &request.binding,
        request.grant_id,
    )
    .await?;
    ensure(original_matches_hot(&original, &hot)?)?;
    let material = original.body.materials[0].clone();
    let server_time = original.body.clock.server_time_ms;
    let installation =
        initial_grant::load_installed_receipt_locked(tx, &material, server_time).await?;
    let launch = load_launch_inputs_locked(tx, request, &view, &material).await?;
    let source = source_exports::load_linked_locked(tx, request.binding.attempt_id).await?;
    let manifest = source.manifest();
    ensure(
        manifest.attempt == view.attempt
            && view.source.as_ref() == Some(&manifest.source)
            && manifest.resolved_spec_digest == view.execution_spec_digest
            && source.token().request().project_revision == launch.spec.project_revision,
    )?;
    let context = load_context_locked(tx, &view, &launch.spec, request).await?;
    let policy =
        crate::profiled_execution::continuation_policy::load_locked(tx, &request.binding).await?;
    ensure_uncapped_cost(&policy, &launch.project)?;
    verify_admitted_material_locked(tx, auth.id(), request, &material).await?;
    ensure_single_started_attempt_locked(tx, view.task_id).await?;
    let LaunchInputs {
        offer,
        spec,
        selection,
        project,
        decision_id,
    } = launch;
    Ok(Pinned {
        offer,
        spec,
        selection,
        original,
        project,
        decision_id,
        source,
        context,
        policy,
        installation,
        material,
        hot,
        versions,
    })
}
/// The original grant belongs to the live owner, deadline and single material.
fn original_matches_hot(original: &OriginalGrant, hot: &HotState) -> Result<bool, AppError> {
    let owner = hot.owner.as_ref().ok_or_else(invalid)?;
    Ok(original.body.guardian_id == owner.guardian_id
        && original.body.launch_id == owner.launch_id
        && original.body.reservation_id == owner.reservation_id
        && hot.execution_deadline_ms == Some(original.execution_deadline_ms)
        && original.body.materials.len() == 1)
}
fn ensure_uncapped_cost(policy: &SelectionPolicy, project: &ProjectRecord) -> Result<(), AppError> {
    let budget = &policy.budget;
    let ceilings = &project.binding.ceilings;
    if budget.max_cost_microunits.is_some()
        || budget.require_complete_accounting
        || budget.require_hard_cost_enforcement
        || ceilings.cost_limit_microunits.is_some()
        || ceilings.require_hard_cost_enforcement
    {
        return Err(AppError::Conflict(
            "qualified continuing cost accounting is not available".into(),
        ));
    }
    Ok(())
}
async fn verify_admitted_material_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    request: &Renew,
    material: &MaterialLease,
) -> Result<(), AppError> {
    let record=sqlx::query("SELECT m.state,m.lease FROM admission_material_leases m JOIN profiled_launch_materials x ON x.lease_id=m.lease_id AND x.attempt_id=$3 WHERE m.lease_id=$1 AND m.node_id=$2 AND m.admitted_attempt_id=$3 FOR SHARE OF m,x").bind(material.lease_id).bind(node).bind(request.binding.attempt_id).fetch_optional(&mut **tx).await.map_err(|_|AppError::Internal)?.ok_or_else(invalid)?;
    ensure(
        record.get::<String, _>("state") == "admitted"
            && record.get::<Value, _>("lease") == json(material)?,
    )
}
async fn ensure_single_started_attempt_locked(
    tx: &mut Transaction<'_, Postgres>,
    task_id: Uuid,
) -> Result<(), AppError> {
    let counts = crate::profiled_execution::no_grant::attempt_counts_locked(tx, task_id).await?;
    if counts.started != 1 {
        return Err(AppError::Conflict(
            "continuing lineage budget is not fully known for this profile".into(),
        ));
    }
    Ok(())
}
// These helpers share the caller's transaction and preserve its lock order.
async fn load_live_authority_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    auth: &AuthenticatedNode,
    request: &Renew,
) -> Result<(authority::AuthorityView, HotState, Versions), AppError> {
    crate::worker_api::lock_current_node(tx, auth).await?;
    ensure(auth.id() == request.identity.node_id)?;
    registration::admitted_runtime_locked(tx, auth.id(), request.identity.boot_id).await?;
    let view = authority::load_locked(state, tx, &request.binding.session, Policy::IdentityOnly)
        .await
        .map_err(AppError::from)?;
    ensure(
        view.kind == Kind::ProfiledV1
            && view.grant_id == Some(request.grant_id)
            && view.identity.node_id == auth.id()
            && view.identity.boot_id == request.identity.boot_id,
    )?;
    let (hot, versions) = load_hot_locked(tx, request).await?;
    let owner = hot.owner.as_ref().ok_or_else(invalid)?;
    let current = hot.grant.as_ref().ok_or_else(invalid)?;
    let time = super::now(state)?;
    ensure(
        owner.guardian_id == request.guardian_id
            && owner.launch_id == request.launch_id
            && owner.reservation_id == request.reservation_id
            && current.grant_id == request.grant_id
            && current.binding == request.binding,
    )?;
    if hot.cancellation_id.is_some() {
        return Err(AppError::ExecutionCancelled);
    }
    let live = time < current.expires_at_ms && hot.execution_deadline_ms.is_some_and(|d| time < d);
    ensure(
        live && view.holds_task && view.status == Status::Granted && !view.revoked && !view.stopped,
    )?;
    registration::current_runtime_locked(state, tx, auth.id(), request.identity.boot_id).await?;
    Ok((view, hot, versions))
}
async fn load_hot_locked(
    tx: &mut Transaction<'_, Postgres>,
    request: &Renew,
) -> Result<(HotState, Versions), AppError> {
    let row=sqlx::query("SELECT state,record_revision,authority_revision,grant_revision FROM profiled_authority_hot WHERE attempt_id=$1 FOR UPDATE").bind(request.binding.attempt_id).fetch_one(&mut **tx).await.map_err(|_|AppError::Internal)?;
    let hot: HotState = serde_json::from_value(row.get("state")).map_err(|_| invalid())?;
    hot.validate_against(&request.binding)
        .map_err(|_| invalid())?;
    let versions = versions(&row);
    versions.validate().map_err(|_| invalid())?;
    Ok((hot, versions))
}
async fn verify_agent_binding_locked(
    tx: &mut Transaction<'_, Postgres>,
    node: Uuid,
    request: &Renew,
    view: &authority::AuthorityView,
) -> Result<(), AppError> {
    let agent = sqlx::query("SELECT binding,profile,context,revoked,node_id FROM agent_launch_bindings WHERE attempt_id=$1 FOR SHARE")
        .bind(request.binding.attempt_id).fetch_optional(&mut **tx).await.map_err(|_|AppError::Internal)?.ok_or_else(invalid)?;
    let graph = json(view.graph.as_ref().ok_or_else(invalid)?)?;
    let expected = serde_json::json!({"task_id":view.task_id,"attempt_id":view.attempt.attempt_id,"session_id":view.attempt.session_id,"generation":view.attempt.generation,"node_id":view.identity.node_id,"boot_id":view.identity.boot_id,"project_id":view.project_id,"workspace_id":graph["workspace_id"],"resolved_spec_digest":view.execution_spec_digest,"profile_digest":graph["profile_digest"],"context_digest":graph["context_digest"]});
    let (profile, context): (Value, Value) = (agent.get("profile"), agent.get("context"));
    let digest_matches = |value: &Value, column: &str| -> Result<bool, AppError> {
        let actual = aq_selection::digest(value).map_err(|_| AppError::Internal)?;
        Ok(actual.as_str() == graph[column].as_str().ok_or_else(invalid)?)
    };
    ensure(
        !agent.get::<bool, _>("revoked")
            && agent.get::<Uuid, _>("node_id") == node
            && agent.get::<Value, _>("binding") == expected
            && aq_agent_api::valid("ResolvedProfile", &profile)
            && aq_agent_api::valid("ContextSnapshot", &context)
            && digest_matches(&profile, "profile_digest")?
            && digest_matches(&context, "context_digest")?,
    )
}
struct LaunchInputs {
    offer: ProfiledOffer,
    spec: ProfiledExecutionSpec,
    selection: ResolvedSelection,
    project: ProjectRecord,
    decision_id: Uuid,
}
async fn load_launch_inputs_locked(
    tx: &mut Transaction<'_, Postgres>,
    request: &Renew,
    view: &authority::AuthorityView,
    material: &MaterialLease,
) -> Result<LaunchInputs, AppError> {
    let row=sqlx::query("SELECT l.launch_binding,l.launch_manifest,l.execution_spec,l.execution_spec_digest,l.graph_task_observation#>'{native_offer,offered,offer,required_capabilities}' AS required,d.decision#>'{project,project}' AS project,l.decision_id,s.selection,s.selection_digest FROM profiled_launches l JOIN intelligence_decisions d ON d.decision_id=l.decision_id JOIN intelligence_attempt_selections s USING(attempt_id) WHERE l.attempt_id=$1 FOR SHARE OF l,d,s").bind(request.binding.attempt_id).fetch_optional(&mut **tx).await.map_err(|_|AppError::Internal)?.ok_or_else(invalid)?;
    ensure(row.get::<Value, _>("launch_binding") == json(&request.binding)?)?;
    let value = |column: &str| row.get::<Value, _>(column);
    let offer = ProfiledOffer {
        binding: request.binding.clone(),
        manifest: serde_json::from_value(value("launch_manifest")).map_err(|_| invalid())?,
        required_capabilities: serde_json::from_value(value("required")).map_err(|_| invalid())?,
    };
    offer.validate().map_err(|_| invalid())?;
    let spec = decode_spec(&row, request, &offer)?;
    let selection: ResolvedSelection =
        serde_json::from_value(value("selection")).map_err(|_| invalid())?;
    let (choice, lineage) = (&selection.choice, &selection.lineage);
    let target = ExecutionTarget::Worker {
        node_id: request.identity.node_id,
        boot_id: request.identity.boot_id,
    };
    ensure(
        aq_selection::digest(&selection).map_err(|_| AppError::Internal)? == spec.selection_digest
            && row.get::<String, _>("selection_digest") == spec.selection_digest.as_str()
            && selection.target == target
            && lineage.project_id == view.project_id
            && lineage.task_id == view.task_id
            && lineage.workspace_id == spec.task.workspace_id,
    )?;
    let project: ProjectRecord = serde_json::from_value(value("project")).map_err(|_| invalid())?;
    ensure(
        project.revision == spec.project_revision
            && project.revision == selection.project_revision
            && project.binding.project_id == lineage.project_id
            && material.credential_profile == choice.credential_profile
            && material.credential_revision == selection.credential_revision
            && material.provider.as_str() == choice.provider
            && material.account_reference.as_str() == choice.account_reference
            && material.endpoint_reference.as_str() == choice.endpoint_reference,
    )?;
    Ok(LaunchInputs {
        offer,
        spec,
        selection,
        project,
        decision_id: row.get("decision_id"),
    })
}
/// Decode the retained spec only when its canonical bytes carry the bound digest.
fn decode_spec(
    row: &sqlx::postgres::PgRow,
    request: &Renew,
    offer: &ProfiledOffer,
) -> Result<ProfiledExecutionSpec, AppError> {
    let mut canonical: Value = row.get("execution_spec");
    canonical.sort_all_objects();
    let bytes = serde_json::to_vec(&canonical).map_err(|_| AppError::Internal)?;
    let digest = &request.binding.execution_spec_digest;
    ensure(
        bytes.len() <= 65536
            && Digest::of(&bytes) == *digest
            && row.get::<String, _>("execution_spec_digest") == digest.as_str(),
    )?;
    let spec = decode_profiled_spec(&bytes).map_err(|_| invalid())?;
    spec.validate_against(offer).map_err(|_| invalid())?;
    Ok(spec)
}
async fn load_context_locked(
    tx: &mut Transaction<'_, Postgres>,
    view: &authority::AuthorityView,
    spec: &ProfiledExecutionSpec,
    request: &Renew,
) -> Result<Value, AppError> {
    let refs=sqlx::query_scalar::<_,Uuid>("SELECT a.artifact_id FROM profiled_launch_artifacts x JOIN admission_artifacts a USING(artifact_id) WHERE x.attempt_id=$1 AND a.descriptor->>'kind'='context' AND a.descriptor#>>'{artifact,id}'=$2 AND a.descriptor#>>'{artifact,version}'=$3 ORDER BY a.artifact_id LIMIT 2").bind(request.binding.attempt_id).bind(spec.task_context.id.as_str()).bind(spec.task_context.version.as_str()).fetch_all(&mut **tx).await.map_err(|_|AppError::Internal)?;
    ensure(refs.len() == 1)?;
    let retained = artifacts::load_locked(tx, refs[0]).await?;
    ensure(
        retained.project_id == view.project_id && retained.descriptor.artifact == spec.task_context,
    )?;
    retained
        .descriptor
        .verify(&retained.bytes, retained.descriptor.retained_at_ms)
        .map_err(|_| invalid())?;
    let context: Value = serde_json::from_slice(&retained.bytes).map_err(|_| invalid())?;
    ensure(aq_agent_api::valid("ContextSnapshot", &context))?;
    Ok(context)
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
