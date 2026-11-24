//! Compose startup instructions and permissions from the active role snapshot and project binding.
use super::{now, refused, result_startup::ReportingInstruction};
use crate::{
    AppState,
    agent_access::scheduling::TaskContext,
    error::AppError,
    profiled_execution::context_projection::{self, Projection},
    project_bindings::domain::{EligibilityRequest, FrozenProjectBinding, ProjectRecord},
};
use aq_profiles::{
    GraphAccessPolicy, GraphAccessPolicyKind,
    roles::{self, Snapshot, StartupInput, StartupText},
};
use aq_role_settings::refusals;
use aq_selection::{CapabilitySnapshot, PreparedLaunch};
use aq_session_api::Permission;
use serde_json::Value;
use uuid::Uuid;

/// The launch rendered for one installed harness.
pub struct Prepared {
    pub launch: PreparedLaunch,
    pub startup: StartupText,
    pub policy: GraphAccessPolicy,
    pub reporting: Option<ReportingInstruction>,
}
pub struct Compiled {
    pub frozen: FrozenProjectBinding,
    pub projection: Projection,
    /// The active role snapshot the launch was resolved from.
    pub snapshot: Snapshot,
    pub role: String,
    pub level: String,
    pub prepared: Vec<Prepared>,
    /// What the role asks for, and what the project's ceilings leave of it.
    pub requested: Vec<Permission>,
    pub effective: Vec<Permission>,
    /// The identity of the task thread in the startup text, when it has one.
    pub thread_identity: Option<String>,
    pub work_kind: Option<String>,
    /// A routed task's harness and provider limits (spec 0011 §4).
    pub route: Option<aq_routing::RouteLimits>,
}
impl Compiled {
    pub fn launch(&self) -> Result<roles::Launch<'_>, AppError> {
        self.snapshot
            .resolve(&self.role, &self.level)
            .map_err(|_| refused())
    }
}
/// The level a task runs on: the class it was routed to (a level's id is the
/// router's class, spec 0014 §5.2), otherwise the project's default.
pub fn level(route: Option<&aq_routing::CompactRoute>, default_intelligence: &str) -> String {
    route.map_or_else(|| default_intelligence.to_string(), |r| r.c.to_string())
}
/// The operations the graph access policy speaks of. The other operations a
/// role lists are agent grants, sealed from the manifest's permission plan.
const GRAPH_OPERATIONS: [&str; 3] = ["task.read", "knowledge.read", roles::SOURCE_WRITE];

pub const INELIGIBLE: &str = "project binding does not admit this node now: ";
/// `predecessor` is the attempt a replacement continues (spec 0017 §5.4).
#[allow(clippy::too_many_arguments)]
#[cfg_attr(not(feature = "project_environments"), allow(unused_variables))]
pub async fn compile(
    state: &AppState,
    record: &ProjectRecord,
    node: Uuid,
    snapshot: &CapabilitySnapshot,
    context: &TaskContext,
    metadata: &Value,
    route: Option<&aq_routing::CompactRoute>,
    predecessor: Option<Uuid>,
) -> Result<Compiled, AppError> {
    let binding = &record.binding;
    let active = crate::role_settings::active(&state.store.pool)
        .await?
        .ok_or_else(|| refused())?;
    let role = aq_role_settings::tasks::effective_role(metadata, &binding.default_role).id;
    let level = level(route, &binding.default_intelligence);
    let launch = active.resolve(&role, &level).map_err(|_| refused())?;
    if !launch.role.enabled {
        return Err(refused());
    }
    let execution = launch.execution;
    #[cfg(not(feature = "project_environments"))]
    let environment = execution.environment.clone();
    #[cfg(feature = "project_environments")]
    let (catalog, environment) = {
        let catalog = super::super::definitions::catalog(state, &state.store.pool).await?;
        let (pool, pinned) = (&state.store.pool, execution.environment.as_deref());
        let routed = route.and_then(|r| r.e.as_deref());
        let placed = aq_environments::place(
            pool,
            &catalog,
            binding,
            pinned,
            routed,
            metadata,
            predecessor,
        );
        match placed.await {
            Ok(environment) => (catalog, environment),
            Err(hold) => {
                let (task, project) = (context.revision.task_id, binding.project_id);
                return Err(crate::role_settings::offer_refused(state, task, project, &hold).await);
            }
        }
    };
    if !binding.allowed_environments.contains(&environment) {
        return Err(refused());
    }
    let eligibility = crate::project_bindings::check_eligibility(
        state,
        binding.project_id,
        &EligibilityRequest {
            expected_project_revision: record.revision,
            node_id: node,
            environment: environment.clone(),
            role: role.clone(),
            intelligence_profile: level.clone(),
            execution_profile: execution.id.clone(),
            snapshot_digest: active.digest().to_string(),
        },
    )
    .await?;
    let ineligible = || AppError::Conflict(format!("{INELIGIBLE}{:?}", eligibility.reasons));
    let frozen = (eligibility.frozen_binding.clone())
        .filter(|_| eligibility.eligible)
        .ok_or_else(ineligible)?;
    // The scheduler holds the project's own ceilings (spec 0014 §6.2).
    let ceilings: Vec<Permission> = (binding.ceilings.permissions.iter())
        .map(|p| Permission {
            operation: p.operation.clone(),
            scope_reference: p.scope_reference.clone(),
        })
        .collect();
    let (project, repository) = (binding.project_id, binding.repository_id);
    #[cfg(feature = "delivery_playbook")]
    let retry_check = {
        let scope = (
            project,
            binding.graph_workspace_id,
            context.revision.task_id,
        );
        aq_playbook_domain::retry_check::read(&state.store.pool, scope, predecessor).await
    };
    let requested = roles::requested_permissions(launch.role, project, repository);
    let effective = requested.effective(&ceilings, &ceilings);
    let requested = requested.into_permissions();
    let projection = context_projection::project(
        context,
        project,
        repository,
        binding.graph_workspace_id,
        &effective,
        now(state)?,
    )
    .map_err(|_| refused())?;
    // Additional opaque project context references need an owned delivery policy.
    if !binding.context.is_empty() {
        return Err(refused());
    }
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
    harnesses.dedup_by(|a, b| a.harness == b.harness);
    #[cfg(feature = "project_environments")]
    if harnesses.is_empty() {
        let (task, project) = (context.revision.task_id, binding.project_id);
        return Err(unlisted(state, task, project, &environment).await);
    }
    if harnesses.is_empty() || harnesses.len() > 16 {
        return Err(refused());
    }
    let task = String::from_utf8(projection.bytes.clone()).map_err(|_| refused())?;
    let work_kind =
        aq_agent_grants::store::ensured_work(&state.store.pool, &context.context["node"], project)
            .await
            .map_err(crate::error::db)?
            .and_then(|work| work.kind);
    let thread = crate::agent_access::task_thread::startup_input(
        state,
        project,
        context.revision.task_id,
        work_kind.as_deref(),
    )
    .await;
    let thread_identity = thread.as_ref().map(|input| input.identity.clone());
    let thread = thread.map(|input| input.text);
    let limit = execution
        .limits
        .startup_context_bytes
        .min(binding.ceilings.context_bytes);
    let graph: Vec<Permission> = (effective.iter())
        .filter(|p| GRAPH_OPERATIONS.contains(&p.operation.as_str()))
        .cloned()
        .collect();
    let context_revision = format!(
        "{}:{}:{}",
        context.revision.task_id, context.revision.version, context.revision.content_revision
    );
    let platform = roles::platform_text(&environment);
    // An admitted version's own platform text wins over the shipped one.
    #[cfg(feature = "project_environments")]
    let platform = catalog.platform_text(&environment).unwrap_or(platform);
    let mut prepared = Vec::new();
    for harness in harnesses {
        let reporting = super::result_startup::for_adapter(state, &harness.adapter)?;
        let assembled = roles::assemble_startup(
            &StartupInput {
                platform,
                role: launch.role,
                project_notes: binding.role_notes.get(&role).map(String::as_str),
                // The platform text directs agents to the repository's notes.
                repository_notes: None,
                task: &task,
                task_thread: thread.as_deref(),
                reporting: reporting.as_ref().map(|r| r.text.as_str()),
            },
            limit,
        );
        let startup = match assembled {
            Ok(startup) => startup,
            Err(error) => {
                let ceiling = binding.ceilings.context_bytes;
                let Some(refusal) = refusals::of_startup(&error, execution, ceiling, &environment)
                else {
                    return Err(refused());
                };
                let task = context.revision.task_id;
                return Err(
                    crate::role_settings::offer_refused(state, task, project, &refusal).await,
                );
            }
        };
        #[cfg(feature = "delivery_playbook")]
        let startup =
            startup.with_retry_evidence(|used| retry_check.as_ref()?.startup(limit, used));
        let policy = GraphAccessPolicy {
            schema_version: 1,
            kind: GraphAccessPolicyKind::PreselectionGraphAccessPolicyV1,
            project_binding_id: project,
            project_revision: record.revision,
            source_context_revision: context_revision.clone(),
            context_digest: digest(&projection.context)?,
            instructions_digest: digest(&startup.rendered)?,
            tool_policy_digest: digest(&(&requested, &effective))?,
            effective_permissions: graph.clone(),
            scope: projection.scope.clone(),
        };
        policy
            .validate_context(&projection.context)
            .map_err(|_| refused())?;
        let launch_digest = aq_selection::digest(&(
            active.digest(),
            (&role, &level, &execution.id),
            (&harness.harness, &environment),
            &startup.rendered,
            (&requested, &effective),
            &context_revision,
        ))
        .map_err(|_| AppError::Internal)?;
        prepared.push(Prepared {
            launch: PreparedLaunch {
                digest: launch_digest,
                harness: harness.harness.clone(),
                environment: environment.clone(),
                startup_bytes: startup.rendered.len() as u64,
                startup_limit_bytes: limit,
            },
            startup,
            policy,
            reporting,
        });
    }
    Ok(Compiled {
        frozen,
        projection,
        role,
        level,
        prepared,
        requested,
        effective,
        thread_identity,
        work_kind,
        route: route.map(aq_routing::CompactRoute::limits),
        snapshot: active,
    })
}
/// This node does not list `environment`: another node may take the task, or,
/// when no node lists it, the task waits with that reason (spec 0017 §5.4).
#[cfg(feature = "project_environments")]
async fn unlisted(state: &AppState, task: Uuid, project: Uuid, environment: &str) -> AppError {
    let now = (state.clock)().timestamp_millis();
    if aq_role_settings::environments::listed(&state.store.pool, environment, now).await {
        return AppError::Conflict(format!("{INELIGIBLE}environment_not_on_node"));
    }
    let refusal = refusals::OfferRefusal::NoNodeHasEnvironment {
        environment: environment.into(),
    };
    crate::role_settings::offer_refused(state, task, project, &refusal).await
}
fn digest<T: serde::Serialize>(value: &T) -> Result<String, AppError> {
    aq_profiles::digest(value).map_err(|_| AppError::Internal)
}
