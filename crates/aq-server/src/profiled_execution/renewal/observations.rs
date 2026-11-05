//! Genuinely new bounded current owner reads; no original receipt is retimed.
use super::{now, pinned::Pinned, private_read};
use crate::{
    AppState,
    error::AppError,
    profiled_execution::{context_projection, source},
    project_bindings::domain::{EligibilityRequest, FrozenProjectBinding},
    source_retention::profiled_base::{self, RetentionObservation},
};
use aq_admission::Digest;
use serde_json::Value;
pub struct Task {
    pub response_digest: Digest,
    pub observed_at_ms: u64,
    pub received_at_ms: u64,
    pub expires_at_ms: u64,
}
pub struct Observed {
    pub binding: FrozenProjectBinding,
    pub task: Task,
    pub source: RetentionObservation,
    pub file: private_read::Fingerprint,
    pub file_started_at_ms: u64,
    pub file_received_at_ms: u64,
}
fn unavailable() -> AppError {
    AppError::UpdatesUnavailable("current execution owner observation unavailable".into())
}
fn changed() -> AppError {
    AppError::Conflict("pinned task or source inputs changed".into())
}
fn root(context: &Value, task: uuid::Uuid) -> Result<&Value, AppError> {
    context["nodes"]
        .as_array()
        .and_then(|nodes| nodes.iter().find(|node| node["id"] == task.to_string()))
        .ok_or_else(changed)
}
fn canonical(value: &Value) -> Result<Vec<u8>, AppError> {
    let mut value = value.clone();
    value.sort_all_objects();
    serde_json::to_vec(&value).map_err(|_| AppError::Internal)
}
fn profile(reference: &aq_admission::ArtifactRef, kind: &str) -> Result<String, AppError> {
    reference
        .id
        .as_str()
        .strip_prefix(&format!("{kind}:"))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(changed)
}
async fn binding(state: &AppState, p: &Pinned) -> Result<FrozenProjectBinding, AppError> {
    let (revision, digest) = crate::project_bindings::active_catalog(state).await?;
    if digest.as_deref() != Some(p.selection.catalog_digest.as_str()) {
        return Err(changed());
    }
    let environment = p
        .project
        .binding
        .allowed_environments
        .iter()
        .find(|e| {
            e.as_str() == p.selection.choice.environment.id.as_str()
                || e.as_str()
                    == format!(
                        "{}@{}",
                        p.selection.choice.environment.id.as_str(),
                        p.selection.choice.environment.version.as_str()
                    )
        })
        .ok_or_else(changed)?
        .clone();
    let node = match p.selection.target {
        aq_selection::ExecutionTarget::Worker { node_id, .. } => node_id,
        _ => return Err(changed()),
    };
    let request = EligibilityRequest {
        expected_project_revision: p.spec.project_revision,
        expected_catalog_revision: revision,
        node_id: node,
        environment,
        agent_profile: profile(&p.offer.manifest.agent_profile, "agent")?,
        intelligence_profile: profile(&p.offer.manifest.intelligence_profile, "intelligence")?,
        execution_profile: profile(&p.offer.manifest.execution_profile, "execution")?,
    };
    let result = crate::project_bindings::check_execution_eligibility(
        state,
        p.selection.lineage.project_id,
        &request,
    )
    .await?;
    if !result.eligible {
        return Err(
            if result
                .reasons
                .iter()
                .any(|reason| reason.contains("unavailable"))
            {
                unavailable()
            } else {
                changed()
            },
        );
    }
    result.frozen_binding.ok_or_else(changed)
}
async fn task(state: &AppState, p: &Pinned) -> Result<Task, AppError> {
    let graph = state.agent_access.as_ref().ok_or_else(unavailable)?;
    let current = graph
        .scheduling_context_current(&p.spec.task, 0, 1, 1_048_576, state.clock.as_ref())
        .await
        .map_err(|error| match error {
            crate::agent_access::scheduling::SchedulingReadError::Unavailable => unavailable(),
            _ => changed(),
        })?;
    let permissions = p
        .offer
        .manifest
        .permissions
        .effective
        .iter()
        .map(|scope| aq_session_api::Permission {
            operation: scope.operation.as_str().into(),
            scope_reference: scope.resource.as_str().into(),
        })
        .collect::<Vec<_>>();
    let projected = context_projection::project(
        &current,
        p.selection.lineage.project_id,
        p.spec.source.repository_id,
        p.selection.lineage.workspace_id,
        &permissions,
        now(state)?,
    )
    .map_err(|_| changed())?;
    if canonical(root(&projected.context, p.spec.task.task_id)?)?
        != canonical(root(&p.context, p.spec.task.task_id)?)?
    {
        return Err(changed());
    }
    Ok(Task {
        response_digest: serde_json::from_value(serde_json::json!(current.stamp.response_digest))
            .map_err(|_| changed())?,
        observed_at_ms: current.stamp.requested_at_ms,
        received_at_ms: current.stamp.received_at_ms,
        expires_at_ms: current.stamp.expires_at_ms,
    })
}
pub async fn collect(state: &AppState, p: &Pinned) -> Result<Observed, AppError> {
    let file = async {
        let started = now(state)?;
        let registry = state.config.intelligence.clone().ok_or_else(unavailable)?;
        let node = match p.selection.target {
            aq_selection::ExecutionTarget::Worker { node_id, .. } => node_id,
            _ => return Err(changed()),
        };
        let source = crate::profiled_execution::login_refresh::lease_source(
            &state.store.pool,
            &registry,
            &p.material,
        )
        .await?;
        let fingerprint = private_read::verify(registry, p.material.clone(), node, source).await?;
        let received = now(state)?;
        if received < started || received >= started.checked_add(2000).ok_or_else(unavailable)? {
            return Err(unavailable());
        }
        Ok::<_, AppError>((fingerprint, started, received))
    };
    let source = async {
        let identity = source::repository_identity(
            state,
            p.spec.source.repository_id,
            p.spec.source.repository_binding_generation,
        )
        .await?;
        if identity != p.source.token().request().logical_identity {
            return Err(changed());
        }
        profiled_base::inspect_retained_for_execution(state, p.source.token(), p.source.fact())
            .await
    };
    let (binding, task, source, (file, file_started_at_ms, file_received_at_ms)) =
        tokio::try_join!(binding(state, p), task(state, p), source, file)?;
    Ok(Observed {
        binding,
        task,
        source,
        file,
        file_started_at_ms,
        file_received_at_ms,
    })
}
impl Observed {
    pub fn ceiling(&self) -> Result<u64, AppError> {
        Ok(self
            .binding
            .binding_observation
            .expires_at_ms
            .min(self.task.expires_at_ms)
            .min(self.source.expires_at_ms()))
    }
    pub fn fresh_at(&self, time: u64) -> Result<(), AppError> {
        if time < self.binding.binding_observation.observed_at_ms
            || time < self.task.received_at_ms
            || time < self.source.observed_at_ms()
            || time < self.file_received_at_ms
            || time
                >= self
                    .file_started_at_ms
                    .checked_add(2000)
                    .ok_or_else(unavailable)?
            || time >= self.ceiling()?
        {
            return Err(unavailable());
        }
        Ok(())
    }
}
