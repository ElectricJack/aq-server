//! Actual current task-only Graph and retention observations, outside SQL locks.
use super::eligibility::{CurrentApplicability, PilotPolicy};
use crate::{
    AppState,
    error::AppError,
    profiled_execution::{artifacts, context_projection, results},
    project_bindings::domain::ProjectBinding,
    source_retention::{
        self, RecordedSourceConfirmation, result_retention::ResultRetentionObservation,
    },
};
use aq_admission::{ArtifactKind, ArtifactRef, Digest};
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

fn changed() -> AppError {
    AppError::Conflict("task-only settlement inputs or policy changed".into())
}
fn unavailable() -> AppError {
    AppError::UpdatesUnavailable("current settlement inputs unavailable".into())
}
fn now(state: &AppState) -> Result<u64, AppError> {
    u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|v| *v > 0)
        .ok_or_else(unavailable)
}
pub(super) fn source_digest(source: &RecordedSourceConfirmation) -> Result<Digest, AppError> {
    Ok(Digest::of(&aq_agent_api::canonical(&json!({"manifest":source.manifest,"receipt":source.receipt,"acknowledgement":source.acknowledgement})).map_err(|_|AppError::Internal)?))
}
fn no_required_neighbors(context: &Value, task: &aq_admission::TaskRevision) -> bool {
    let version = task.version.to_string();
    let content_revision = task.content_revision.to_string();
    aq_agent_api::valid("ContextSnapshot", context)
        && context["root_ids"] == json!([task.task_id])
        && context["nodes"].as_array().is_some_and(|v| {
            v.len() == 1
                && v[0]["id"] == json!(task.task_id)
                && v[0]["version"].as_str() == Some(version.as_str())
                && v[0]["content_revision"].as_str() == Some(content_revision.as_str())
        })
        && context["edges"].as_array().is_some_and(Vec::is_empty)
        && context["pinned_revisions"]
            .as_array()
            .is_some_and(Vec::is_empty)
        && context["coverage"]["status"] == "complete"
        && context["coverage"]["reasons"] == json!([])
}
fn same_content(a: &Value, b: &Value) -> bool {
    let mut a = a.clone();
    let mut b = b.clone();
    for v in [&mut a, &mut b] {
        let Some(o) = v.as_object_mut() else {
            return false;
        };
        if o.remove("observed_at").is_none() {
            return false;
        }
    }
    a == b
}

struct Original {
    report: ArtifactRef,
    source: RecordedSourceConfirmation,
    context: Value,
    project: ProjectBinding,
    project_digest: String,
    permissions: Vec<aq_session_api::Permission>,
    task: aq_admission::TaskRevision,
    source_operation: Uuid,
}
impl Original {
    fn validate_context(
        &self,
        state: &AppState,
        graph: &crate::agent_access::scheduling::TaskContext,
        project: &ProjectBinding,
        task: &aq_admission::TaskRevision,
    ) -> Result<(), AppError> {
        let projection = context_projection::project(
            graph,
            project.project_id,
            project.repository_id,
            project.graph_workspace_id,
            &self.permissions,
            now(state)?,
        )
        .map_err(|_| changed())?;
        if !no_required_neighbors(&projection.context, task)
            || !same_content(&self.context, &projection.context)
        {
            return Err(changed());
        }
        Ok(())
    }
}

pub(super) async fn fence_project_locked(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    result: &results::RecordedResult,
) -> Result<ProjectBinding, AppError> {
    let config = state
        .config
        .profiled_execution
        .as_ref()
        .ok_or_else(unavailable)?;
    crate::profiled_execution::policies::fence_configuration_locked(tx, config).await?;
    let row = sqlx::query(
        "SELECT revision,binding FROM project_execution_bindings WHERE project_id=$1 FOR SHARE",
    )
    .bind(result.spec.lifecycle.project_id)
    .fetch_optional(&mut **tx)
    .await
    .map_err(|_| AppError::Internal)?
    .ok_or_else(changed)?;
    let p: ProjectBinding =
        serde_json::from_value(row.get("binding")).map_err(|_| AppError::Internal)?;
    if row.get::<i64, _>("revision") as u64 != result.spec.project_revision
        || !p.enabled
        || p.project_id != result.spec.lifecycle.project_id
        || p.graph_workspace_id != result.spec.task.workspace_id
        || p.repository_id != result.spec.source.repository_id
        || p.repository_binding_generation != result.spec.source.repository_binding_generation
        || p.policy.identity != result.outcome.validation_policy.id.as_str()
        || p.policy.revision != result.outcome.validation_policy.version.as_str()
        || p.policy.digest != result.outcome.validation_policy.digest.as_str()
    {
        return Err(changed());
    }
    // The attempt ran on the role snapshot it launched with (spec 0014
    // §7.1); what is active now does not decide whether its result settles.
    Ok(p)
}
pub(crate) fn policy(
    state: &AppState,
    result: &results::RecordedResult,
) -> Result<PilotPolicy, AppError> {
    let d = state
        .config
        .profiled_execution
        .as_ref()
        .and_then(|c| {
            c.definition_bodies
                .iter()
                .find(|d| d.artifact() == &result.outcome.validation_policy)
        })
        .ok_or_else(unavailable)?;
    PilotPolicy::from_definition(d)
}
async fn load_task_context(
    tx: &mut Transaction<'_, Postgres>,
    r: &results::RecordedResult,
) -> Result<Option<Value>, AppError> {
    let mut context = None;
    let mut refs = r.outcome.inputs.clone();
    refs.push(r.spec.task_context.clone());
    refs.push(r.spec.task_observation.clone());
    refs.sort_by_key(|r| (r.id.clone(), r.version.clone()));
    refs.dedup();
    if refs.len() > 66 {
        return Err(changed());
    }
    let mut bytes = 0usize;
    for pin in refs {
        let id:Uuid=sqlx::query_scalar("SELECT a.artifact_id FROM admission_artifacts a JOIN profiled_launch_artifacts l USING(artifact_id) WHERE l.attempt_id=$1 AND a.descriptor->'artifact'=$2 ORDER BY a.artifact_id LIMIT 1").bind(r.report.binding.attempt_id).bind(serde_json::to_value(&pin).map_err(|_|AppError::Internal)?).fetch_optional(&mut **tx).await.map_err(|_|AppError::Internal)?.ok_or_else(changed)?;
        let retained = artifacts::load_locked(tx, id).await?;
        bytes = bytes
            .checked_add(retained.bytes.len())
            .filter(|n| *n <= 8388608)
            .ok_or_else(changed)?;
        if retained.project_id != r.spec.lifecycle.project_id
            || retained.descriptor.artifact != pin
            || Digest::of(&retained.bytes) != pin.digest
        {
            return Err(changed());
        }
        if pin == r.spec.task_context || pin == r.spec.task_observation {
            let value: Value = serde_json::from_slice(&retained.bytes).map_err(|_| changed())?;
            if retained.descriptor.kind != ArtifactKind::Context
                || !no_required_neighbors(&value, &r.spec.task)
            {
                return Err(changed());
            }
            if pin == r.spec.task_context {
                context = Some(value)
            }
        } else if !matches!(
            retained.descriptor.kind,
            ArtifactKind::Source
                | ArtifactKind::Definition
                | ArtifactKind::Composition
                | ArtifactKind::Selection
        ) {
            return Err(changed());
        }
    }
    Ok(context)
}

async fn load_original(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    report: Uuid,
    source_operation: Uuid,
) -> Result<Original, AppError> {
    let r = results::load_locked(state, tx, report).await?;
    if !r.is_current_report_head {
        return Err(changed());
    }
    policy(state, &r)?;
    let project = fence_project_locked(state, tx, &r).await?;
    let context = load_task_context(tx, &r).await?;
    let source = source_retention::load_confirmed_locked(tx, source_operation, &r).await?;
    let permissions = project
        .ceilings
        .permissions
        .iter()
        .map(|p| aq_session_api::Permission {
            operation: p.operation.clone(),
            scope_reference: p.scope_reference.clone(),
        })
        .collect();
    let project_digest =
        aq_agent_api::digest(&serde_json::to_value(&project).map_err(|_| AppError::Internal)?)
            .map_err(|_| AppError::Internal)?;
    Ok(Original {
        report: r.receipt_artifact,
        source,
        context: context.ok_or_else(changed)?,
        project,
        project_digest,
        permissions,
        task: r.spec.task,
        source_operation,
    })
}
/// Actual observer product; no Deserialize, caller freshness flag, or granted DTO.
pub(crate) struct Observation {
    original: Original,
    graph: crate::agent_access::scheduling::TaskContext,
    retention: ResultRetentionObservation,
}
/// The only positive applicability constructor consumes this final owner product.
pub(crate) struct VerifiedApplicability {
    report: ArtifactRef,
    source: Digest,
    cancelled: bool,
}
impl VerifiedApplicability {
    pub(super) fn report(&self) -> &ArtifactRef {
        &self.report
    }
    pub(super) fn source(&self) -> &Digest {
        &self.source
    }
    pub(super) fn cancelled(&self) -> bool {
        self.cancelled
    }
}
impl Observation {
    pub(crate) async fn observe(
        state: &AppState,
        report: Uuid,
        source_operation: Uuid,
    ) -> Result<Self, AppError> {
        let mut tx = state
            .store
            .pool
            .begin()
            .await
            .map_err(|_| AppError::Internal)?;
        sqlx::query("SET LOCAL lock_timeout='500ms'")
            .execute(&mut *tx)
            .await
            .map_err(|_| AppError::Internal)?;
        let original = load_original(state, &mut tx, report, source_operation).await?;
        tx.commit().await.map_err(|_| AppError::Internal)?;
        let rt = state.agent_access.as_deref().ok_or_else(unavailable)?;
        let (graph, retention) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::try_join!(
                async {
                    rt.scheduling_context_current(
                        &original.task,
                        1,
                        64,
                        1048576,
                        state.clock.as_ref(),
                    )
                    .await
                    .map_err(|_| changed())
                },
                source_retention::result_retention::inspect_current(state, &original.source)
            )
        })
        .await
        .map_err(|_| unavailable())??;
        original.validate_context(state, &graph, &original.project, &original.task)?;
        Ok(Self {
            original,
            graph,
            retention,
        })
    }
    pub(crate) async fn verify_locked(
        &self,
        state: &AppState,
        tx: &mut Transaction<'_, Postgres>,
        result: &results::RecordedResult,
        source: &RecordedSourceConfirmation,
    ) -> Result<CurrentApplicability, AppError> {
        if self.original.report != result.receipt_artifact
            || self.original.source_operation != source.receipt.receipt_id
            || source_digest(&self.original.source)? != source_digest(source)?
        {
            return Err(changed());
        }
        let project = fence_project_locked(state, tx, result).await?;
        if aq_agent_api::digest(&serde_json::to_value(&project).map_err(|_| AppError::Internal)?)
            .map_err(|_| AppError::Internal)?
            != self.original.project_digest
        {
            return Err(changed());
        }
        self.original
            .validate_context(state, &self.graph, &project, &result.spec.task)?;
        self.retention.validate_current(state, source)?;
        let view = crate::execution::authority::load_locked(
            state,
            tx,
            &result.report.binding.session,
            crate::execution::authority::Policy::IdentityOnly,
        )
        .await?;
        if !view.stopped || !view.holds_task || view.offer_id != result.report.binding.offer_id {
            return Err(changed());
        }
        let fact = VerifiedApplicability {
            report: result.receipt_artifact.clone(),
            source: source_digest(source)?,
            cancelled: view.cancellation_id.is_some() || view.revoked,
        };
        Ok(CurrentApplicability::from_owner(&fact))
    }
    /// CPU-only last fence, after every SQL/user/review wait and immediately
    /// before commit. It does not turn saved graph bytes into a new request.
    pub(crate) fn validate_fresh(&self, state: &AppState) -> Result<(), AppError> {
        let p = &self.original.project;
        self.original
            .validate_context(state, &self.graph, p, &self.original.task)?;
        self.retention
            .validate_current(state, &self.original.source)
    }
    pub(crate) fn evidence(&self) -> Result<Value, AppError> {
        Ok(
            json!({"policy":"task_only_mutable_inputs_v1","report":self.original.report,"task":self.original.task,"source":source_digest(&self.original.source)?,"graph_response_digest":self.graph.digest,"graph_requested_at_ms":self.graph.stamp.requested_at_ms,"graph_received_at_ms":self.graph.stamp.received_at_ms,"graph_expires_at_ms":self.graph.stamp.expires_at_ms,"project_digest":self.original.project_digest,"scope":"initial/current task-only context observed; original task CAS fences admitted mutable inputs, not all future incoming citations"}),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task_context() -> (Value, aq_admission::TaskRevision) {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../../protocol/agent-access/v1/fixtures/base.json"
        ))
        .unwrap();
        let mut context = fixture["context"]["context"].clone();
        context["nodes"].as_array_mut().unwrap().truncate(1);
        context["edges"] = json!([]);
        context["pinned_revisions"] = json!([]);
        context["coverage"]["status"] = json!("complete");
        context["coverage"]["reasons"] = json!([]);
        let node = &context["nodes"][0];
        let task = aq_admission::TaskRevision {
            workspace_id: Uuid::new_v4(),
            task_id: node["id"].as_str().unwrap().parse().unwrap(),
            version: node["version"].as_str().unwrap().parse().unwrap(),
            content_revision: node["content_revision"].as_str().unwrap().parse().unwrap(),
        };
        assert!(no_required_neighbors(&context, &task));
        (context, task)
    }

    #[test]
    fn settlement_context_ignores_only_observation_time() {
        let (context, _) = task_context();
        let mut observed = context.clone();
        observed["observed_at"] = json!("2026-11-26T18:00:00Z");
        assert!(same_content(&context, &observed));
        observed["nodes"][0]["body"] = json!("changed task content");
        assert!(!same_content(&context, &observed));
        let mut missing_time = context.clone();
        missing_time.as_object_mut().unwrap().remove("observed_at");
        assert!(!same_content(&context, &missing_time));
        assert!(!same_content(&missing_time, &missing_time));
    }

    #[test]
    fn settlement_context_refuses_neighbors_incomplete_coverage_and_changed_revisions() {
        let (context, task) = task_context();
        let mut neighbor = context.clone();
        neighbor["nodes"]
            .as_array_mut()
            .unwrap()
            .push(context["nodes"][0].clone());
        assert!(!no_required_neighbors(&neighbor, &task));
        let mut incomplete = context.clone();
        incomplete["coverage"]["status"] = json!("incomplete");
        assert!(!no_required_neighbors(&incomplete, &task));
        let mut changed_task = task.clone();
        changed_task.content_revision += 1;
        assert!(!no_required_neighbors(&context, &changed_task));
        changed_task = task.clone();
        changed_task.version += 1;
        assert!(!no_required_neighbors(&context, &changed_task));
    }
}
