//! Fixed public Git observations for a configured project source policy.
//! These facts are proposals until retained and fenced with the actual offer.
use super::config::ProjectPolicyRecord;
use crate::{
    AppState, agent_access::scheduling::TaskContext, error::AppError,
    project_bindings::domain::FrozenProjectBinding,
};
use aq_admission::{
    ArtifactRef, Digest, ObjectFormat, ObjectId, SourceAdmission, SourceSelection, Text,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;
#[track_caller]
fn refused() -> AppError {
    tracing::debug!(target: "aq_refusal", at = %std::panic::Location::caller(), "profiled refusal");
    AppError::Conflict("configured source observation unavailable or inconsistent".into())
}
pub struct Observation {
    pub admission: SourceAdmission,
    pub retention_handle: Uuid,
    pub artifact: ArtifactRef,
    pub bytes: Vec<u8>,
}
async fn read(
    state: &AppState,
    repository: Uuid,
    generation: u64,
    operation: &str,
    params: Value,
) -> Result<Value, AppError> {
    let runtime = state.source_retention.as_ref().ok_or_else(refused)?;
    aq_delivery::offer_source::read(
        &runtime.config.git_socket,
        repository,
        generation,
        operation,
        params,
    )
    .await
    .map_err(|_| refused())
}
/// Initial source mode requires an exact retained SHA1 commit. Annotated tags,
/// SHA256 and retention of an ancestor through a descendant handle refuse here.
/// An enrolled named-ref offer refreshes its retained remote head as needed.
pub async fn observe(
    state: &AppState,
    project: &FrozenProjectBinding,
    policy: &ProjectPolicyRecord,
    enrolled: bool,
) -> Result<Observation, AppError> {
    observe_source(state, project, policy, None, enrolled, None).await
}

pub(super) async fn observe_for_offer(
    state: &AppState,
    project: &FrozenProjectBinding,
    policy: &ProjectPolicyRecord,
    context: &TaskContext,
    enrolled: bool,
) -> Result<Observation, AppError> {
    observe_source(state, project, policy, None, enrolled, Some(context)).await
}

pub(super) async fn require_base(
    state: &AppState,
    task: Uuid,
    source: &Observation,
    export: &super::source_exports::Request,
) -> Result<(), AppError> {
    let admission = &source.admission;
    if admission.resolved_object.hex == export.base.hex
        && admission.repository_id == export.repository_id
        && admission.repository_binding_generation == export.binding_generation
    {
        return Ok(());
    }
    #[cfg(feature = "delivery_playbook")]
    if serde_json::from_slice::<Value>(&source.bytes)
        .is_ok_and(|body| body["evidence"].get("main_sync").is_some())
    {
        return Err(ancestry_wait(
            state,
            admission.project_id,
            task,
            DeliveryAncestryWait::SourceBaseChanged,
        )
        .await);
    }
    let _ = (state, task);
    Err(refused())
}

#[cfg(feature = "delivery_playbook")]
use aq_role_settings::refusals::{DeliveryAncestryWait, OfferRefusal};

#[cfg(feature = "delivery_playbook")]
async fn ancestry_wait(
    state: &AppState,
    project: Uuid,
    task: Uuid,
    wait: DeliveryAncestryWait,
) -> AppError {
    let refusal = OfferRefusal::DeliveryAncestry { wait };
    tracing::info!(%task, %project, ?refusal, "waiting for delivery ancestry");
    if crate::role_settings::tables(state) {
        aq_role_settings::refusals::record(
            &state.store.pool,
            task,
            project,
            &refusal,
            (state.clock)().timestamp_millis(),
        )
        .await;
    }
    AppError::Conflict(refusal.refusal())
}
/// A fixed commit and its central retention handle; `handoff` is evidence.
struct Fixed {
    hex: String,
    retention_handle: Uuid,
    handoff: Option<Value>,
}

/// A replacement observes the predecessor's exact retained commit and handle.
/// It does not resolve the configured moving ref again or mint another export.
pub async fn observe_retained(
    state: &AppState,
    project: &FrozenProjectBinding,
    policy: &ProjectPolicyRecord,
    retained: &super::source_exports::Request,
) -> Result<Observation, AppError> {
    retained.validate().map_err(|_| refused())?;
    let binding = &project.project.binding;
    if retained.project_id != binding.project_id
        || retained.project_revision != project.project.revision
        || retained.repository_id != binding.repository_id
        || retained.binding_generation != binding.repository_binding_generation
    {
        return Err(refused());
    }
    let fixed = Fixed {
        hex: retained.base.hex.clone(),
        retention_handle: retained.operation_id,
        handoff: None,
    };
    observe_source(state, project, policy, Some(fixed), false, None).await
}

/// A handoff successor (spec 0007) observes the predecessor's saved commit,
/// hydrated from its candidate's central retention handle.
pub async fn observe_handoff(
    state: &AppState,
    project: &FrozenProjectBinding,
    policy: &ProjectPolicyRecord,
    base: &super::handoff::HandoffBase,
) -> Result<Observation, AppError> {
    let handoff =
        json!({"predecessor": base.predecessor, "candidate_operation_id": base.retention_handle});
    let fixed = Fixed {
        hex: base.oid.clone(),
        retention_handle: base.retention_handle,
        handoff: Some(handoff),
    };
    observe_source(state, project, policy, Some(fixed), false, None).await
}

async fn observe_source(
    state: &AppState,
    project: &FrozenProjectBinding,
    policy: &ProjectPolicyRecord,
    fixed: Option<Fixed>,
    enrolled: bool,
    _offer: Option<&TaskContext>,
) -> Result<Observation, AppError> {
    let _permit = aq_machine_control::limits::SOURCE_READS.enter();
    let _permit = _permit.ok_or_else(|| refused())?;
    let configured = state
        .config
        .profiled_execution
        .as_deref()
        .ok_or_else(|| refused())?
        .project_policy_record(project.project.binding.project_id)
        .map_err(|_| refused())?;
    if !policy.enabled
        || serde_json::to_value(policy).map_err(|_| AppError::Internal)?
            != serde_json::to_value(configured).map_err(|_| AppError::Internal)?
    {
        return Err(refused());
    }
    let binding = &project.project.binding;
    let mut observed = u64::try_from((state.clock)().timestamp_millis()).map_err(|_| refused())?;
    if observed == 0
        || !binding.enabled
        || project.project.revision == 0
        || binding.repository_id.is_nil()
        || binding.repository_binding_generation == 0
        || project.node_id.is_nil()
        || observed < project.binding_observation.observed_at_ms
        || observed >= project.binding_observation.expires_at_ms
    {
        return Err(refused());
    }
    let mut evidence = BTreeMap::new();
    let selection = fixed.as_ref().map_or_else(
        || policy.source.clone(),
        |fixed| SourceSelection::Immutable {
            object: ObjectId {
                format: ObjectFormat::Sha1,
                hex: fixed.hex.clone(),
            },
        },
    );
    // The export transfer (or handoff candidate) is the retained byte owner.
    // The checkpoint handle originally used may have since been released.
    let retention_handle = fixed
        .as_ref()
        .map_or(policy.retention_handle, |f| f.retention_handle);
    if let Some(handoff) = fixed.as_ref().and_then(|f| f.handoff.clone()) {
        evidence.insert("handoff", handoff);
    }
    // Spec 0016 §4: a named ref follows the head central aq-git last synced
    // from the remote and retained, synced now when older than Settings allow.
    #[cfg(feature = "delivery_playbook")]
    let synced = match &selection {
        SourceSelection::NamedRef { reference } if enrolled => {
            let runtime = state.source_retention.as_ref().ok_or_else(|| refused())?;
            let (id, g) = (binding.repository_id, binding.repository_binding_generation);
            let scope =
                aq_delivery::main_sync::Scope::new(binding.project_id, id, g, reference.as_str());
            let clock = || u64::try_from((state.clock)().timestamp_millis()).unwrap_or(0);
            let (pool, socket) = (&state.store.pool, &runtime.config.git_socket);
            let synced = if let Some(offer) = _offer {
                let task = offer.revision.task_id;
                match aq_delivery::main_sync::dependent_current(
                    pool,
                    socket,
                    &scope,
                    task,
                    &offer.context,
                    clock,
                )
                .await
                {
                    Ok(synced) => synced,
                    Err(wait) => {
                        return Err(ancestry_wait(state, binding.project_id, task, wait).await);
                    }
                }
            } else {
                aq_delivery::main_sync::current(pool, socket, &scope, clock)
                    .await
                    .map_err(|_| refused())?
            };
            observed = clock(); // this read of a record `current` found fresh
            if observed >= project.binding_observation.expires_at_ms {
                return Err(refused());
            }
            let value = serde_json::to_value(&synced).map_err(|_| AppError::Internal)?;
            evidence.insert("main_sync", value);
            Some((
                ObjectId {
                    format: ObjectFormat::Sha1,
                    hex: synced.head,
                },
                synced.retention_handle,
            ))
        }
        _ => None,
    };
    // D1: a named ref that AQ itself publishes follows the recorded source
    // head (the publication receipt is the evidence) and hydrates from the
    // candidate's central retention handle. Without one, a fresh observation.
    let head = match &selection {
        SourceSelection::NamedRef { reference } if !enrolled => {
            crate::profiled_execution::settlement::publication::current_head(
                &state.store.pool,
                binding.project_id,
                binding.repository_id,
                binding.repository_binding_generation,
                reference.as_str(),
            )
            .await
            .map_err(|_| refused())?
        }
        _ => None,
    };
    let mut retention_handle = retention_handle;
    let object = match &selection {
        SourceSelection::Immutable { object } => object.clone(),
        #[cfg(feature = "delivery_playbook")]
        SourceSelection::NamedRef { .. } if synced.is_some() => {
            let (object, handle) = synced.ok_or_else(refused)?;
            retention_handle = handle;
            object
        }
        SourceSelection::NamedRef { .. } if head.is_some() => {
            let head = head.as_ref().ok_or_else(|| refused())?;
            retention_handle = head.retention_handle;
            evidence.insert(
                "publication_head",
                serde_json::to_value(head).map_err(|_| AppError::Internal)?,
            );
            ObjectId {
                format: ObjectFormat::Sha1,
                hex: head.oid.clone(),
            }
        }
        SourceSelection::NamedRef { reference } => {
            let response = read(
                state,
                binding.repository_id,
                binding.repository_binding_generation,
                "repo.observation",
                json!({"refs":[reference.as_str()],"max_age_ms":30000}),
            )
            .await?;
            let received =
                u64::try_from((state.clock)().timestamp_millis()).map_err(|_| refused())?;
            if received < observed {
                return Err(refused());
            }
            let object = resolve_ref(
                &response,
                reference.as_str(),
                binding.repository_binding_generation,
                received,
            )?;
            observed = observed.min(
                response["started_at_ms"]
                    .as_u64()
                    .ok_or_else(|| refused())?,
            );
            evidence.insert("reference_observation", response);
            object
        }
    };
    object.validate().map_err(|_| refused())?;
    if object.format != ObjectFormat::Sha1 {
        return Err(refused());
    }
    let inspected = read(
        state,
        binding.repository_id,
        binding.repository_binding_generation,
        "inspect",
        json!({"oid":{"algorithm":"sha1","hex":object.hex}}),
    )
    .await?;
    if inspected["oid"] != json!({"algorithm":"sha1","hex":object.hex})
        || inspected["tree"]["algorithm"] != "sha1"
        || !hex40(inspected["tree"]["hex"].as_str().unwrap_or(""))
    {
        return Err(refused());
    }
    evidence.insert("commit_inspection", inspected);
    let retained = read(
        state,
        binding.repository_id,
        binding.repository_binding_generation,
        "retention.inspect",
        json!({"handle_id":retention_handle,"limit":1}),
    )
    .await?;
    verify_retention(
        &retained,
        retention_handle,
        binding.repository_id,
        binding.repository_binding_generation,
        &object,
    )?;
    evidence.insert("retention_inspection", retained);
    let now = u64::try_from((state.clock)().timestamp_millis()).map_err(|_| refused())?;
    if now < observed || now >= observed.checked_add(30000).ok_or_else(|| refused())? {
        return Err(refused());
    }
    let admission = SourceAdmission {
        schema_version: 1,
        project_id: binding.project_id,
        project_revision: project.project.revision,
        repository_id: binding.repository_id,
        repository_binding_generation: binding.repository_binding_generation,
        policy: policy.artifact().map_err(|_| refused())?,
        selection,
        resolved_object: object,
        observed_at_ms: observed,
        expires_at_ms: observed + 30000,
    };
    admission.validate().map_err(|_| refused())?;
    let mut body = json!({"schema_version":1,"admission":admission,"retention_handle":retention_handle,"evidence":evidence});
    body.sort_all_objects();
    let bytes = serde_json::to_vec(&body).map_err(|_| AppError::Internal)?;
    if bytes.len() > 1_048_576 {
        return Err(refused());
    }
    let observation_digest = Digest::of(&bytes);
    let artifact = ArtifactRef {
        id: Text::new(format!(
            "aq.source-admission-observation/{}",
            observation_digest.as_str()
        ))
        .map_err(|_| AppError::Internal)?,
        version: Text::new("1").map_err(|_| AppError::Internal)?,
        digest: observation_digest,
    };
    Ok(Observation {
        admission,
        retention_handle,
        artifact,
        bytes,
    })
}
/// Spec 0016 §4: the loop syncing each enabled project's followed named ref.
#[cfg(feature = "delivery_playbook")]
pub async fn follow_main(state: AppState, stop: tokio::sync::watch::Receiver<bool>) {
    let (Some(runtime), Some(config), Some(delivery)) = (
        &state.source_retention,
        &state.config.profiled_execution,
        &state.config.delivery,
    ) else {
        return;
    };
    // Task-limited enrollment only needs a sync when its selected task is offered.
    if delivery.task_allowlist.is_some() {
        return;
    }
    let projects = config
        .projects
        .iter()
        .filter(|p| p.enabled && delivery.project_allowlist.contains(&p.project_id))
        .filter_map(|p| match &p.source {
            SourceSelection::NamedRef { reference } => {
                Some((p.project_id, reference.as_str().to_owned()))
            }
            SourceSelection::Immutable { .. } => None,
        });
    let (pool, socket) = (state.store.pool.clone(), runtime.config.git_socket.clone());
    let clock = state.clock.clone();
    aq_delivery::main_sync::run(pool, socket, projects.collect(), move || clock(), stop).await
}
/// Resolve the actual configured repository's logical identity, never a path or
/// guessed UUID alias. The export owner subsequently verifies the exact name.
pub(crate) async fn repository_identity(
    state: &AppState,
    repository: Uuid,
    generation: u64,
) -> Result<String, AppError> {
    let value = read(state, repository, generation, "repo.show", json!({})).await?;
    aq_delivery::source_checks::repository_identity(&value, repository, generation)
        .ok_or_else(|| refused())
}
use aq_delivery::source_checks::hex40;
fn resolve_ref(
    v: &Value,
    reference: &str,
    generation: u64,
    now: u64,
) -> Result<ObjectId, AppError> {
    let hex = aq_delivery::source_checks::resolve_ref(v, reference, generation, now);
    Ok(ObjectId {
        format: ObjectFormat::Sha1,
        hex: hex.ok_or_else(|| refused())?,
    })
}
fn verify_retention(
    v: &Value,
    handle: Uuid,
    repository: Uuid,
    generation: u64,
    object: &ObjectId,
) -> Result<(), AppError> {
    aq_delivery::source_checks::retention_holds(v, handle, repository, generation, &object.hex)
        .then_some(())
        .ok_or_else(|| refused())
}

mod ownership;
pub use ownership::{retain_admission, validate_body, validate_stored_body};
