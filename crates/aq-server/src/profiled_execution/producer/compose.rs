use super::{now, refused};
use crate::{
    AppState,
    agent_access::scheduling::TaskContext,
    error::AppError,
    profiled_execution::context_projection::{self, Projection},
    project_bindings::domain::{EligibilityRequest, FrozenProjectBinding, ProjectRecord},
};
use aq_admission::{ArtifactRef, Digest, Text};
use aq_profiles::{
    Ceilings, CompileRequest, ContextInput, Delivery, GraphAccessPolicy, ValidatedCatalog,
};
use aq_selection::{CapabilitySnapshot, PreparedComposition};
use aq_session_api::{Permission, ProfileRequest, Source};
use uuid::Uuid;
const PLATFORM: &str = "Work on the exact pinned assignment in the authorized workspace. Use only the scoped tool permissions. Report source changes and evidence; a report is not task completion or authority to expand scope.";
pub struct Compiled {
    pub frozen: FrozenProjectBinding,
    pub projection: Projection,
    pub candidates: Vec<PreparedComposition>,
    pub policies: Vec<GraphAccessPolicy>,
    pub catalog: ValidatedCatalog,
    pub agent: String,
}
fn source(artifact: &ArtifactRef) -> Source {
    Source {
        identity: artifact.id.as_str().into(),
        revision: artifact.version.as_str().into(),
        digest: artifact.digest.as_str().into(),
    }
}
pub async fn compile(
    state: &AppState,
    record: &ProjectRecord,
    node: Uuid,
    snapshot: &CapabilitySnapshot,
    context: &TaskContext,
) -> Result<Compiled, AppError> {
    let (revision, digest) = crate::project_bindings::active_catalog(state).await?;
    let digest = digest.ok_or_else(refused)?;
    if digest != record.binding.catalog_digest || record.binding.allowed_agent_profiles.len() != 1 {
        return Err(refused());
    }
    let raw: Vec<u8> =
        sqlx::query_scalar("SELECT artifact FROM profile_catalog_artifacts WHERE digest=$1")
            .bind(&digest)
            .fetch_one(&state.store.pool)
            .await
            .map_err(|_| AppError::Internal)?;
    if raw.len() > 2_097_152 {
        return Err(refused());
    }
    let catalog = ValidatedCatalog::from_artifact(&raw).map_err(|_| refused())?;
    if catalog.digest() != digest {
        return Err(refused());
    }
    let agent_name = record.binding.allowed_agent_profiles[0].clone();
    let agent = catalog
        .artifact()
        .catalog
        .agents
        .iter()
        .find(|a| aq_profiles::key(&a.id, a.version) == agent_name)
        .ok_or_else(refused)?;
    if !record
        .binding
        .allowed_intelligence_profiles
        .contains(&agent.intelligence)
        || !record
            .binding
            .allowed_execution_profiles
            .contains(&agent.execution)
    {
        return Err(refused());
    }
    let execution = catalog
        .artifact()
        .catalog
        .execution
        .iter()
        .find(|e| aq_profiles::key(&e.id, e.version) == agent.execution)
        .ok_or_else(refused)?;
    let environment = execution
        .environments
        .iter()
        .filter(|e| record.binding.allowed_environments.contains(e))
        .min()
        .ok_or_else(refused)?
        .clone();
    let eligibility = crate::project_bindings::check_eligibility(
        state,
        record.binding.project_id,
        &EligibilityRequest {
            expected_project_revision: record.revision,
            expected_catalog_revision: revision,
            node_id: node,
            environment: environment.clone(),
            agent_profile: agent_name.clone(),
            intelligence_profile: agent.intelligence.clone(),
            execution_profile: agent.execution.clone(),
        },
    )
    .await?;
    let frozen = eligibility
        .frozen_binding
        .filter(|_| eligibility.eligible)
        .ok_or_else(refused)?;
    let permissions = record
        .binding
        .ceilings
        .permissions
        .iter()
        .map(|p| Permission {
            operation: p.operation.clone(),
            scope_reference: p.scope_reference.clone(),
        })
        .collect::<Vec<_>>();
    let projection = context_projection::project(
        context,
        record.binding.project_id,
        record.binding.repository_id,
        record.binding.graph_workspace_id,
        &permissions,
        now(state)?,
    )
    .map_err(|_| refused())?;
    let project_ref = ArtifactRef {
        id: Text::new(&record.binding.policy.identity).map_err(|_| refused())?,
        version: Text::new(&record.binding.policy.revision).map_err(|_| refused())?,
        digest: serde_json::from_value(serde_json::json!(record.binding.policy.digest))
            .map_err(|_| refused())?,
    };
    let config = state
        .config
        .profiled_execution
        .as_ref()
        .ok_or_else(refused)?;
    let project_body = config
        .definition_bodies
        .iter()
        .find(|d| d.artifact() == &project_ref)
        .ok_or_else(refused)?;
    let project_text = format!(
        "Project policy (exact owner definition follows):\n{}",
        project_body.body()
    );
    let project_input = super::artifacts::reference(
        &format!("aq.project-instructions/{}", project_ref.digest.as_str()),
        project_text.as_bytes(),
    )?;
    // Additional opaque project context references need an owned delivery policy;
    // this first complete profile refuses instead of treating unknown tokens as0.
    if !record.binding.context.is_empty() {
        return Err(refused());
    }
    let mut installation = ["task.read", "knowledge.read"]
        .into_iter()
        .map(|operation| Permission {
            operation: operation.into(),
            scope_reference: format!("project:{}", record.binding.project_id),
        })
        .collect::<Vec<_>>();
    installation.push(Permission {
        operation: "source.write".into(),
        scope_reference: format!("repository:{}", record.binding.repository_id),
    });
    let platform_ref = ArtifactRef {
        id: Text::new("aq.platform-native-instructions").map_err(|_| refused())?,
        version: Text::new("1").map_err(|_| refused())?,
        digest: Digest::of(PLATFORM.as_bytes()),
    };
    let mut harnesses = snapshot
        .harnesses
        .iter()
        .filter(|h| {
            execution.harnesses.contains(&h.harness)
                && (h.environment.id.as_str() == environment
                    || format!(
                        "{}@{}",
                        h.environment.id.as_str(),
                        h.environment.version.as_str()
                    ) == environment)
        })
        .collect::<Vec<_>>();
    harnesses.sort_by(|a, b| {
        a.harness
            .cmp(&b.harness)
            .then(a.cli_version.cmp(&b.cli_version))
    });
    if harnesses.is_empty() || harnesses.len() > 16 {
        return Err(refused());
    }
    let mut candidates = Vec::new();
    let mut policies = Vec::new();
    for harness in harnesses {
        let request = CompileRequest {
            profile: ProfileRequest {
                catalog_digest: digest.clone(),
                agent_profile: agent_name.clone(),
                project_binding_id: record.binding.project_id,
                expected_project_revision: record.revision,
                task_context_revision: format!(
                    "{}:{}:{}",
                    context.revision.task_id,
                    context.revision.version,
                    context.revision.content_revision
                ),
            },
            ceilings: Ceilings {
                installation: installation.clone(),
                project: permissions.clone(),
                execution: execution.permissions.clone(),
                caller: permissions.clone(),
            },
            available_tools: vec![
                "task.read".into(),
                "knowledge.read".into(),
                "source.write".into(),
            ],
            available_adapters: vec![harness.harness.clone()],
            harness: harness.harness.clone(),
            environment: environment.clone(),
            platform: ContextInput {
                source: source(&platform_ref),
                text: PLATFORM.into(),
                delivery: Delivery::Startup,
                required: true,
                omission_priority: 0,
            },
            project: ContextInput {
                source: source(&project_input),
                text: project_text.clone(),
                delivery: Delivery::Startup,
                required: true,
                omission_priority: 0,
            },
            assignment: ContextInput {
                source: source(&projection.artifact),
                text: String::from_utf8(projection.bytes.clone()).map_err(|_| refused())?,
                delivery: Delivery::Startup,
                required: true,
                omission_priority: 0,
            },
            context: vec![],
            overrides: vec![],
        };
        let draft = aq_profiles::compile_draft(&catalog, &request).map_err(|_| refused())?;
        if !draft.composition_valid {
            continue;
        }
        let sealed = draft
            .composition
            .ok_or_else(refused)?
            .seal(projection.scope.clone(), &projection.context)
            .map_err(|_| refused())?;
        if sealed
            .composition
            .instruction_order
            .iter()
            .any(|i| i.delivery != Delivery::Startup)
        {
            continue;
        }
        let composition_digest =
            aq_selection::digest(&sealed.composition).map_err(|_| AppError::Internal)?;
        candidates.push(PreparedComposition {
            digest: composition_digest,
            composition: sealed.composition,
        });
        policies.push(sealed.graph_access_policy);
    }
    if candidates.is_empty() {
        return Err(refused());
    }
    Ok(Compiled {
        frozen,
        projection,
        candidates,
        policies,
        catalog,
        agent: agent_name,
    })
}
