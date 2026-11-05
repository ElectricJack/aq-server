//! Fresh pre-grant Graph observations. Original startup bytes remain immutable.
use super::{AppError, AppState, Digest, Postgres, Transaction, refused};
use crate::profiled_execution::{context_projection, native_offer::observation::ReadProvenance};
use aq_admission::execution_spec::ProfiledExecutionSpec;
use serde_json::Value;

pub(super) struct Input {
    pub spec: ProfiledExecutionSpec,
    pub context: Value,
}
impl Input {
    pub async fn spec(
        tx: &mut Transaction<'_, Postgres>,
        attempt: uuid::Uuid,
    ) -> Result<ProfiledExecutionSpec, AppError> {
        let spec: Value = sqlx::query_scalar(
            "SELECT execution_spec FROM profiled_launches WHERE attempt_id=$1 FOR SHARE",
        )
        .bind(attempt)
        .fetch_one(&mut **tx)
        .await
        .map_err(super::db)?;
        let spec: ProfiledExecutionSpec = serde_json::from_value(spec).map_err(|_| refused())?;
        spec.validate().map_err(|_| refused())?;
        Ok(spec)
    }
    pub fn new(spec: ProfiledExecutionSpec, bytes: Vec<u8>) -> Result<Self, AppError> {
        if bytes.len() > 1_048_576 || Digest::of(&bytes) != spec.task_context.digest {
            return Err(refused());
        }
        let context: Value = serde_json::from_slice(&bytes).map_err(|_| refused())?;
        if !aq_agent_api::valid("ContextSnapshot", &context) {
            return Err(refused());
        }
        Ok(Self { spec, context })
    }
}
pub(super) struct Observation {
    original_spec: Digest,
    original_context: Digest,
    provenance: ReadProvenance,
}
impl Observation {
    pub fn expires_at_ms(&self) -> u64 {
        self.provenance
            .ready_expires_at_ms
            .min(self.provenance.context_expires_at_ms)
    }
    pub fn validate_for(&self, input: &Input, now: u64) -> Result<(), AppError> {
        if input.spec.artifact().map_err(|_| refused())?.digest != self.original_spec
            || input.spec.task_context.digest != self.original_context
        {
            return Err(refused());
        }
        self.provenance.validate_at(now)
    }
}
/// Compare authorized semantic content; observation time belongs to each read.
/// Removing it from these temporary values never rewrites stored startup bytes.
fn same_content(original: &Value, current: &Value) -> bool {
    let mut original = original.clone();
    let mut current = current.clone();
    for value in [&mut original, &mut current] {
        let Some(object) = value.as_object_mut() else {
            return false;
        };
        if object.remove("observed_at").is_none() {
            return false;
        }
    }
    original == current
}
fn read_error(error: crate::agent_access::scheduling::SchedulingReadError) -> AppError {
    match error {
        crate::agent_access::scheduling::SchedulingReadError::Unavailable => {
            AppError::UpdatesUnavailable(
                "current graph observation unavailable; preparation remains held".into(),
            )
        }
        _ => refused(),
    }
}
pub(super) async fn observe(
    state: &AppState,
    input: &Input,
    decision: &crate::intelligence::resolver::Decision,
) -> Result<Observation, AppError> {
    let graph = state.agent_access.as_ref().ok_or_else(refused)?;
    let binding = &decision.project.project.binding;
    if binding.project_id != input.spec.lifecycle.project_id
        || binding.graph_workspace_id != input.spec.task.workspace_id
        || decision.project.project.revision != input.spec.project_revision
    {
        return Err(refused());
    }
    // Same bounded profile as publication. A task no longer present in this
    // finite readiness page refuses; no unbounded scan or stale fallback.
    // Read stamps and provenance validation share the current owner clock.
    let ready = graph
        .scheduling_ready_current(input.spec.task.workspace_id, 8, None, state.clock.as_ref())
        .await
        .map_err(read_error)?;
    let context = graph
        .scheduling_context_current(&input.spec.task, 1, 64, 1_048_576, state.clock.as_ref())
        .await
        .map_err(read_error)?;
    let permissions = binding
        .ceilings
        .permissions
        .iter()
        .map(|p| aq_session_api::Permission {
            operation: p.operation.clone(),
            scope_reference: p.scope_reference.clone(),
        })
        .collect::<Vec<_>>();
    let time = super::now(state)?;
    let projection = context_projection::project(
        &context,
        binding.project_id,
        binding.repository_id,
        binding.graph_workspace_id,
        &permissions,
        time,
    )
    .map_err(|_| refused())?;
    if !same_content(&input.context, &projection.context) {
        return Err(refused());
    }
    let provenance = ReadProvenance::capture(
        &input.spec.task,
        &ready,
        &context,
        projection.artifact.digest,
        time,
    )?;
    Ok(Observation {
        original_spec: input.spec.artifact().map_err(|_| refused())?.digest,
        original_context: input.spec.task_context.digest.clone(),
        provenance,
    })
}
