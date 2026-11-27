//! Fixed public Git observations for a configured project source policy.
//! These facts are proposals until retained and fenced with the actual offer.
use super::config::ProjectPolicyRecord;
use crate::{
    AppState, agent_access::scheduling::TaskContext, error::AppError,
    project_bindings::domain::FrozenProjectBinding,
};
use aq_admission::{ArtifactRef, ObjectFormat, ObjectId, SourceAdmission, SourceSelection};
use aq_delivery::source_checks::Repository;
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
    repository: Repository,
    operation: &str,
    params: Value,
) -> Result<Value, AppError> {
    let runtime = state.source_retention.as_ref().ok_or_else(refused)?;
    aq_delivery::offer_source::read(&runtime.config.git_socket, repository, operation, params)
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
    Observer::new(state, project, policy)
        .observe(None, enrolled, None)
        .await
}

pub(super) async fn require_base(
    state: &AppState,
    task: Uuid,
    source: &Observation,
    export: &super::source_exports::Request,
) -> Result<(), AppError> {
    let admission = &source.admission;
    let observed_base = (
        admission.repository_id,
        admission.repository_binding_generation,
        admission.resolved_object.hex.as_str(),
    );
    if observed_base
        == (
            export.repository_id,
            export.binding_generation,
            export.base.hex.as_str(),
        )
    {
        return Ok(());
    }
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

use aq_role_settings::refusals::{DeliveryAncestryWait, OfferRefusal};

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
            aq_role_settings::refusals::RefusedTask { task, project },
            &refusal,
            (state.clock)().timestamp_millis(),
        )
        .await;
    }
    AppError::Conflict(refusal.refusal())
}
/// A fixed commit and its central retention handle; `evidence` names its origin.
struct Fixed {
    hex: String,
    retention_handle: Uuid,
    evidence: Option<(&'static str, Value)>,
}

/// A replacement observes the predecessor's exact retained commit and handle.
/// It does not resolve the configured moving ref again. With `repin` (the predecessor),
/// for an export under a newer placement-only revision, which export admission checks.
pub async fn observe_retained(
    state: &AppState,
    project: &FrozenProjectBinding,
    policy: &ProjectPolicyRecord,
    retained: &super::source_exports::Request,
    repin: Option<Uuid>,
) -> Result<Observation, AppError> {
    retained.validate().map_err(|_| refused())?;
    let binding = &project.project.binding;
    let revision = repin.map_or(project.project.revision, |_| retained.project_revision);
    let project_scope = (binding.project_id, revision);
    let repository = (binding.repository_id, binding.repository_binding_generation);
    let exact = project_scope == (retained.project_id, retained.project_revision)
        && repository == (retained.repository_id, retained.binding_generation);
    if !exact {
        return Err(refused());
    }
    let fixed = Fixed {
        hex: retained.base.hex.clone(),
        retention_handle: retained.operation_id,
        evidence: repin.map(|p| ("retained", json!({ "predecessor": p }))),
    };
    Observer::new(state, project, policy)
        .observe(Some(fixed), false, None)
        .await
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
        evidence: Some(("handoff", handoff)),
    };
    Observer::new(state, project, policy)
        .observe(Some(fixed), false, None)
        .await
}

<<<<<<< HEAD
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
=======
#[derive(Clone, Copy)]
pub(super) struct Observer<'a> {
    state: &'a AppState,
    project: &'a FrozenProjectBinding,
    policy: &'a ProjectPolicyRecord,
}
impl<'a> Observer<'a> {
    pub(super) fn new(
        state: &'a AppState,
        project: &'a FrozenProjectBinding,
        policy: &'a ProjectPolicyRecord,
    ) -> Self {
        Self {
            state,
            project,
            policy,
>>>>>>> refs/remotes/central/main
        }
    }
    pub(super) async fn for_offer(
        self,
        context: &TaskContext,
        enrolled: bool,
    ) -> Result<Observation, AppError> {
        self.observe(None, enrolled, Some(context)).await
    }
    fn repository(self) -> Repository {
        let b = &self.project.project.binding;
        Repository {
            id: b.repository_id,
            generation: b.repository_binding_generation,
        }
<<<<<<< HEAD
        _ => None,
    };
    let mut retention_handle = retention_handle;
    let object = match &selection {
        SourceSelection::Immutable { object } => object.clone(),
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
    // Enrollment selects the result path. A first offer on either path must
    // still prove the chosen base against its completed blockers.
    let object = if let Some(offer) = _offer.filter(|_| !evidence.contains_key("main_sync")) {
        use aq_delivery::main_sync::{OfferAncestry, Scope};
        let task = offer.revision.task_id;
        let project_id = binding.project_id;
        let repository = aq_delivery::commands::GitTarget {
            repository_id: binding.repository_id,
            binding_generation: binding.repository_binding_generation,
        };
        let moving = match &selection {
            SourceSelection::NamedRef { reference }
                if reference.as_str().starts_with("refs/heads/") =>
            {
                Some(Scope::new(
                    project_id,
                    repository.repository_id,
                    repository.binding_generation,
                    reference.as_str(),
                ))
            }
            _ => None,
        };
        let pool = &state.store.pool;
        let clock = || u64::try_from((state.clock)().timestamp_millis()).unwrap_or(0);
        let runtime = state.source_retention.as_ref().ok_or_else(refused)?;
        let verified = async {
            OfferAncestry::load(pool, repository, task, &offer.context)
                .await?
                .verify_or_refresh(
                    pool,
                    &runtime.config.git_socket,
                    &object.hex,
                    moving.as_ref(),
                    clock,
                )
                .await
        }
        .await;
        match verified {
            Ok(None) => object,
            Ok(Some(synced)) => {
                observed = clock();
                if observed >= project.binding_observation.expires_at_ms {
                    return Err(refused());
                }
                retention_handle = synced.retention_handle;
                evidence.remove("reference_observation");
                evidence.remove("publication_head");
                evidence.insert(
                    "main_sync",
                    serde_json::to_value(&synced).map_err(|_| AppError::Internal)?,
                );
                ObjectId {
                    format: ObjectFormat::Sha1,
                    hex: synced.head,
                }
            }
            Err(wait) => return Err(ancestry_wait(state, project_id, task, wait).await),
        }
    } else {
        object
    };
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
=======
    }
    fn now(self) -> Result<u64, AppError> {
        u64::try_from((self.state.clock)().timestamp_millis()).map_err(|_| refused())
    }
    fn admit(self) -> Result<u64, AppError> {
        let Self {
            state,
            project,
            policy,
        } = self;
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
        let observed = self.now()?;
        let bound = binding.enabled && project.project.revision > 0 && !project.node_id.is_nil();
        let repo = !binding.repository_id.is_nil() && binding.repository_binding_generation > 0;
        let current = (project.binding_observation.observed_at_ms
            ..project.binding_observation.expires_at_ms)
            .contains(&observed);
        if ![bound, repo, current, observed > 0]
            .into_iter()
            .all(|valid| valid)
        {
            return Err(refused());
        }
        Ok(observed)
    }
    async fn observe(
        self,
        fixed: Option<Fixed>,
        enrolled: bool,
        _offer: Option<&TaskContext>,
    ) -> Result<Observation, AppError> {
        let _permit = aq_machine_control::limits::SOURCE_READS
            .enter()
            .ok_or_else(refused)?;
        let observed = self.admit()?;
        let mut selected = Selected::new(self.policy, fixed, observed);
        let object = selected.resolve(self, enrolled, _offer).await?;
        object.validate().map_err(|_| refused())?;
        if object.format != ObjectFormat::Sha1 {
            return Err(refused());
        }
        // Enrollment selects the result path. A first offer on either path must
        // still prove the chosen base against its completed blockers.
        #[cfg(feature = "delivery_playbook")]
        let object = selected.verify_ancestry(self, _offer, object).await?;
        selected.inspect(self, &object).await?;
        selected.observation(self, object)
    }
>>>>>>> refs/remotes/central/main
}
/// Spec 0016 §4: the loop syncing each enabled project's followed named ref.
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
    let now_ms = || u64::try_from((state.clock)().timestamp_millis()).unwrap_or(0);
    let follower = aq_delivery::main_sync::MainFollower::new(
        &state.store.pool,
        &runtime.config.git_socket,
        now_ms,
    );
    follower.run(projects.collect(), stop).await
}
/// Resolve the actual configured repository's logical identity, never a path or
/// guessed UUID alias. The export owner subsequently verifies the exact name.
pub(crate) async fn repository_identity(
    state: &AppState,
    repository: Uuid,
    generation: u64,
) -> Result<String, AppError> {
    let value = read(
        state,
        Repository {
            id: repository,
            generation,
        },
        "repo.show",
        json!({}),
    )
    .await?;
    aq_delivery::source_checks::repository_identity(&value, repository, generation)
        .ok_or_else(|| refused())
}
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
mod selection;
use selection::Selected;
mod ownership;
pub(super) use ownership::retain_for;
pub use ownership::{retain_admission, validate_body, validate_stored_body};
