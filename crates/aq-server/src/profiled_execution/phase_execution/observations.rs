//! New bounded owner reads for phase execution. Admission observations and
//! immutable accepted inputs are never retimed or converted into current facts.
use crate::{
    AppState,
    error::AppError,
    intelligence::resolver::Decision,
    profiled_execution::{
        context_projection, native_offer::observation::ReadProvenance, renewal, source,
        source_exports,
    },
    project_bindings::domain::{EligibilityRequest, FrozenProjectBinding},
    source_retention::profiled_base::{self, RetentionObservation},
};
use aq_admission::{
    ArtifactRef, Digest, MaterialLease, SessionIdentity, execution_spec::ProfiledExecutionSpec,
    startup::NativeOffer,
};
use aq_selection::{ExecutionTarget, ResolvedSelection};
use serde::Serialize;
use serde_json::Value;
use std::time::Duration;

pub(super) struct Input {
    pub spec: ProfiledExecutionSpec,
    pub context: Value,
    pub full_context_digest: Digest,
    pub decision: Decision,
    pub selection: ResolvedSelection,
    pub offer: NativeOffer,
    pub source: source_exports::LinkedExport,
    pub material: MaterialLease,
}
fn refused() -> AppError {
    AppError::Conflict("pinned phase execution inputs changed".into())
}
fn unavailable() -> AppError {
    AppError::UpdatesUnavailable("fresh phase execution owner observation unavailable".into())
}
fn now(state: &AppState) -> Result<u64, AppError> {
    u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(unavailable)
}
fn canonical(value: &impl Serialize) -> Result<Vec<u8>, AppError> {
    let mut value = serde_json::to_value(value).map_err(|_| AppError::Internal)?;
    value.sort_all_objects();
    serde_json::to_vec(&value).map_err(|_| AppError::Internal)
}
fn digest(value: &impl Serialize) -> Result<Digest, AppError> {
    Ok(Digest::of(&canonical(value)?))
}
impl Input {
    fn node(&self) -> Result<uuid::Uuid, AppError> {
        match (&self.selection.target, &self.spec.lifecycle.session) {
            (
                ExecutionTarget::Worker { node_id, boot_id },
                SessionIdentity::Remote {
                    node_id: session_node,
                    boot_id: session_boot,
                    ..
                },
            ) if node_id == session_node && boot_id == session_boot => Ok(*node_id),
            _ => Err(refused()),
        }
    }
    fn association(&self) -> Result<Digest, AppError> {
        self.spec
            .validate_against(&self.offer.offered.offer)
            .map_err(|_| refused())?;
        self.offer.validate().map_err(|_| refused())?;
        self.material.validate().map_err(|_| refused())?;
        let context = canonical(&self.context)?;
        let selected = self
            .decision
            .outcome
            .resolved
            .as_ref()
            .ok_or_else(refused)?;
        let request = self.source.token().request();
        let selection = digest(&self.selection)?;
        if context.len() > 1_048_576
            || !aq_agent_api::valid("ContextSnapshot", &self.context)
            || Digest::of(&context) != self.spec.task_context.digest
            || selection != self.spec.selection_digest
            || digest(selected)? != selection
            || digest(&self.decision.inputs)? != self.selection.snapshot_digest
            || self.selection.lineage.project_id != self.spec.lifecycle.project_id
            || self.selection.lineage.workspace_id != self.spec.task.workspace_id
            || self.selection.lineage.task_id != self.spec.task.task_id
            || self.selection.project_revision != self.spec.project_revision
            || self.material.binding != self.offer.offered.offer.binding
            || self.material.credential_profile != self.selection.choice.credential_profile
            || self.material.credential_revision != self.selection.credential_revision
            || self.material.provider.as_str() != self.selection.choice.provider
            || self.material.account_reference.as_str() != self.selection.choice.account_reference
            || self.material.endpoint_reference.as_str() != self.selection.choice.endpoint_reference
            || self.decision.project.project.binding.project_id != self.spec.lifecycle.project_id
            || self.decision.project.project.revision != self.spec.project_revision
            || self.decision.project.project.binding.repository_id != self.spec.source.repository_id
            || self.decision.project.project.binding.graph_workspace_id
                != self.spec.task.workspace_id
            || self.decision.project.node_id != self.node()?
            || request.project_id != self.spec.lifecycle.project_id
            || request.project_revision != self.spec.project_revision
            || request.repository_id != self.spec.source.repository_id
            || request.binding_generation != self.spec.source.repository_binding_generation
            || request.base.hex != self.spec.source.resolved_object.hex
        {
            return Err(refused());
        }
        self.source.fact().validate_for(self.source.token())?;
        digest(&(
            self.spec.artifact().map_err(|_| refused())?.digest,
            self.full_context_digest.clone(),
            self.decision.decision_id,
            digest(&self.decision.project)?,
            selection,
            digest(&self.offer)?,
            self.source.token().request_digest(),
            self.source.fact().manifest_digest(),
            digest(&self.material)?,
        ))
    }
}
/// Actual observations plus private original associations. No public constructor
/// or caller-selected success/freshness flag can mint this value.
pub(super) struct Observed {
    association: Digest,
    pub binding: FrozenProjectBinding,
    pub graph: ReadProvenance,
    pub source: RetentionObservation,
    pub file: renewal::Fingerprint,
    pub file_started_at_ms: u64,
    pub file_received_at_ms: u64,
}
impl Observed {
    pub fn ceiling(&self) -> Result<u64, AppError> {
        Ok(self
            .binding
            .binding_observation
            .expires_at_ms
            .min(self.graph.ready_expires_at_ms)
            .min(self.graph.context_expires_at_ms)
            .min(self.source.expires_at_ms()))
    }
    pub fn fresh_at(&self, time: u64) -> Result<(), AppError> {
        self.graph.validate_at(time).map_err(|_| unavailable())?;
        if time < self.binding.binding_observation.observed_at_ms
            || time < self.source.observed_at_ms()
            || time < self.file_received_at_ms
            || self.file_received_at_ms < self.file_started_at_ms
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
    pub fn validate_for(&self, input: &Input, time: u64) -> Result<(), AppError> {
        if self.association != input.association()?
            || self.graph.task != input.spec.task
            || self.graph.context_response_digest != input.full_context_digest
            || self.file.digest != input.material.material_digest
            || self.file.bytes != input.material.material_bytes
        {
            return Err(refused());
        }
        self.source.validate_for(input.source.token())?;
        self.fresh_at(time)
    }
}
fn profile(reference: &ArtifactRef, kind: &str) -> Result<String, AppError> {
    reference
        .id
        .as_str()
        .strip_prefix(&format!("{kind}:"))
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(refused)
}
async fn binding(state: &AppState, input: &Input) -> Result<FrozenProjectBinding, AppError> {
    let (revision, catalog) = crate::project_bindings::active_catalog(state).await?;
    if catalog.as_deref() != Some(input.selection.catalog_digest.as_str()) {
        return Err(refused());
    }
    let project = &input.decision.project.project.binding;
    let choice = &input.selection.choice.environment;
    let environment = project
        .allowed_environments
        .iter()
        .find(|e| {
            e.as_str() == choice.id.as_str()
                || e.as_str() == format!("{}@{}", choice.id.as_str(), choice.version.as_str())
        })
        .cloned()
        .ok_or_else(refused)?;
    let manifest = &input.offer.offered.offer.manifest;
    let result = crate::project_bindings::check_execution_eligibility(
        state,
        input.spec.lifecycle.project_id,
        &EligibilityRequest {
            expected_project_revision: input.spec.project_revision,
            expected_catalog_revision: revision,
            node_id: input.node()?,
            environment,
            agent_profile: profile(&manifest.agent_profile, "agent")?,
            intelligence_profile: profile(&manifest.intelligence_profile, "intelligence")?,
            execution_profile: profile(&manifest.execution_profile, "execution")?,
        },
    )
    .await?;
    if !result.eligible {
        return Err(
            if result.reasons.iter().any(|r| r.contains("unavailable")) {
                unavailable()
            } else {
                refused()
            },
        );
    }
    result.frozen_binding.ok_or_else(refused)
}
/// Observation time is the only authorized projection field excluded. Unknown
/// fields, revisions, coverage, edges and neighboring content remain significant.
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
fn exact_body(expected: &Digest, reported: &str, raw: &str) -> bool {
    reported == expected.as_str() && Digest::of(raw.as_bytes()) == *expected
}
fn read_error(error: crate::agent_access::scheduling::SchedulingReadError) -> AppError {
    match error {
        crate::agent_access::scheduling::SchedulingReadError::Unavailable => unavailable(),
        _ => refused(),
    }
}
async fn graph(state: &AppState, input: &Input) -> Result<ReadProvenance, AppError> {
    let graph = state.agent_access.as_ref().ok_or_else(unavailable)?;
    let ready = super::observe_ready(state, graph, input.spec.task.workspace_id)
        .await
        .map_err(read_error)?;
    let context = graph
        .scheduling_context_current(&input.spec.task, 1, 64, 1_048_576, state.clock.as_ref())
        .await
        .map_err(read_error)?;
    if !exact_body(
        &input.full_context_digest,
        &context.stamp.response_digest,
        &context.raw_json,
    ) {
        return Err(refused());
    }
    let project = &input.decision.project.project.binding;
    let permissions = project
        .ceilings
        .permissions
        .iter()
        .map(|p| aq_session_api::Permission {
            operation: p.operation.clone(),
            scope_reference: p.scope_reference.clone(),
        })
        .collect::<Vec<_>>();
    let time = now(state)?;
    let projection = context_projection::project(
        &context,
        project.project_id,
        project.repository_id,
        project.graph_workspace_id,
        &permissions,
        time,
    )
    .map_err(|_| refused())?;
    if !same_content(&input.context, &projection.context) {
        return Err(refused());
    }
    ReadProvenance::capture(
        &input.spec.task,
        &ready,
        &context,
        projection.artifact.digest,
        time,
    )
}
/// No SQL locks are held by this read owner. Cancellation of the private read
/// retains its existing bounded worker ownership through I/O and zeroization.
pub(super) async fn collect(state: &AppState, input: &Input) -> Result<Observed, AppError> {
    collect_with_lane(state, input, false).await
}
pub(super) async fn collect_continuing(
    state: &AppState,
    input: &Input,
) -> Result<Observed, AppError> {
    collect_with_lane(state, input, true).await
}
async fn collect_with_lane(
    state: &AppState,
    input: &Input,
    continuing: bool,
) -> Result<Observed, AppError> {
    let association = input.association()?;
    let file = async {
        let started = now(state)?;
        let registry = state.config.intelligence.clone().ok_or_else(unavailable)?;
        let source = crate::profiled_execution::login_refresh::lease_source(
            &state.store.pool,
            &registry,
            &input.material,
        )
        .await?;
        let file = if continuing {
            renewal::verify_continuing_material_backing(
                registry,
                input.material.clone(),
                input.node()?,
                source,
            )
            .await?
        } else {
            renewal::verify_phase_material_backing(
                registry,
                input.material.clone(),
                input.node()?,
                source,
            )
            .await?
        };
        let received = now(state)?;
        if received < started || received >= started.checked_add(2000).ok_or_else(unavailable)? {
            return Err(unavailable());
        }
        Ok::<_, AppError>((file, started, received))
    };
    let source = async {
        let identity = source::repository_identity(
            state,
            input.spec.source.repository_id,
            input.spec.source.repository_binding_generation,
        )
        .await?;
        if identity != input.source.token().request().logical_identity {
            return Err(refused());
        }
        profiled_base::inspect_retained(state, input.source.token(), input.source.fact()).await
    };
    let (binding, graph, source, (file, file_started_at_ms, file_received_at_ms)) =
        tokio::time::timeout(Duration::from_secs(3), async {
            tokio::try_join!(binding(state, input), graph(state, input), source, file)
        })
        .await
        .map_err(|_| unavailable())??;
    let observed = Observed {
        association,
        binding,
        graph,
        source,
        file,
        file_started_at_ms,
        file_received_at_ms,
    };
    observed.validate_for(input, now(state)?)?;
    Ok(observed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn only_top_level_observation_time_is_excluded_from_semantic_context() {
        let original = json!({"observed_at":"old","nodes":[{"version":4,"content_revision":7,"body":"original","observed_at":"nested"}],"edges":[],"coverage":{"status":"complete"}});
        let mut current = original.clone();
        current["observed_at"] = json!("new");
        assert!(same_content(&original, &current));
        for (path, value) in [
            ("version", json!(5)),
            ("content_revision", json!(8)),
            ("body", json!("changed")),
            ("observed_at", json!("changed nested")),
        ] {
            let mut changed = current.clone();
            changed["nodes"][0][path] = value;
            assert!(!same_content(&original, &changed));
        }
        current["extra"] = json!(true);
        assert!(!same_content(&original, &current));
    }
    #[test]
    fn malformed_or_missing_observation_time_is_not_an_equal_projection() {
        assert!(!same_content(&json!({"nodes":[]}), &json!({"nodes":[]})));
        assert!(!same_content(&json!([]), &json!([])));
    }
    #[test]
    fn exact_raw_context_identity_is_separate_from_semantic_projection() {
        let raw = r#"{"node":{"body":"same authorized task"},"neighbor":"original"}"#;
        let expected = Digest::of(raw.as_bytes());
        assert!(exact_body(&expected, expected.as_str(), raw));
        let differently_formatted = format!("{raw}\n");
        assert_eq!(
            serde_json::from_str::<Value>(raw).unwrap(),
            serde_json::from_str::<Value>(&differently_formatted).unwrap()
        );
        assert!(!exact_body(
            &expected,
            expected.as_str(),
            &differently_formatted
        ));
        let changed = raw.replace("original", "changed neighbor");
        assert!(!exact_body(&expected, expected.as_str(), &changed));
        assert!(!exact_body(
            &expected,
            Digest::of(b"different stamp").as_str(),
            raw
        ));
    }
}
