//! Typed dashboard task and knowledge edits over the configured Graph mutation API.
//!
//! Each kind is one fresh-read, version-checked Graph `MutationRequest` under a
//! fixed per-operation idempotency key. Graph remains the authority for every
//! domain rule (admin-only verify/restore, supersession, structural edges).
use super::*;
use crate::dashboard_api::{
    actions::{self, ActionKind, ActionRequest, Disposition, Outcome, Registry, existing},
    contract::Scope,
    errors::ApiError,
};
use aq_human_work::knowledge_actions::{
    ContentType, KnowledgeAuthor, KnowledgeCorrect, KnowledgeRestore, KnowledgeRetire,
    KnowledgeVerify,
};
use async_trait::async_trait;
use axum::http::StatusCode;
use schemars::JsonSchema;
use std::collections::{BTreeMap, BTreeSet};
mod admission;
pub(super) use admission::{Claim, OwnerRead};
mod dispatch;
mod journal;
mod subtree;

type ApiResult<T> = std::result::Result<T, ApiError>;

/// Current subject facts the operations are built against.
pub(crate) struct Current<'a> {
    pub(crate) subject: Uuid,
    pub(crate) actor: Uuid,
    pub(crate) supervisor_actor: Option<Uuid>,
    /// `(attempt, task)` when an agent creates the task (spec 0006 §6.7).
    pub(crate) agent: Option<(Uuid, Uuid)>,
    pub(crate) project: Uuid,
    pub(crate) version: u64,
    pub(crate) node: &'a Value,
}

pub(crate) trait GraphPayload:
    Clone + serde::de::DeserializeOwned + Serialize + JsonSchema + Send + Sync + 'static
{
    const KIND: &'static str;
    const GRANT: &'static str;
    const TARGET: aq_command_registry::TargetRule = aq_command_registry::TargetRule::Subject;
    /// Subject entity type, which is also the Graph node kind ("project" for author).
    const ENTITY: &'static str;
    const ACTIVATE_SUBTREE: bool = false;
    fn operation_id(&self) -> Uuid;
    fn project_id(&self) -> Uuid;
    fn expected(&self) -> &str;
    fn reason_required() -> bool {
        false
    }
    /// Bounds serde cannot express; the published schema states the same limits.
    fn valid(&self) -> bool;
    /// Whether the task must never have had an attempt. A task with one
    /// carries execution history the scheduler will not start over.
    fn never_attempted() -> bool {
        false
    }
    /// Whether no attempt may hold the task. A holding attempt would keep
    /// running, and its result could no longer be settled.
    fn not_held() -> bool {
        false
    }
    /// Whether no attempt of the task may ever have been granted. An offer
    /// that was declined or never granted does not count.
    fn never_granted() -> bool {
        false
    }
    /// Additional task references that must belong to the subject's project.
    fn tasks(&self) -> Vec<Uuid> {
        vec![]
    }
    /// The roles the payload names; each must exist (spec 0014 §5.5).
    fn roles(&self) -> Vec<&str> {
        vec![]
    }
    fn operations(&self, current: &Current<'_>) -> ApiResult<Vec<Value>>;
    fn retirement(&self) -> Option<aq_human_work::epic_retirement::dispatch::Action<'_>> {
        None
    }
    /// A routing kind writes through `routing` instead of a Graph patch.
    fn route_change(&self) -> Option<RouteChange> {
        None
    }
    /// The blocker a held subject waits on instead (`task.dependency.add` with `resume_held`).
    fn resume_held(&self) -> Option<Uuid> {
        None
    }
}

pub(crate) enum RouteChange {
    Set(Box<aq_routing::Route>),
    Clear,
}

pub(super) fn version(text: &str) -> Option<u64> {
    (!text.starts_with('0') && text.len() <= 19 && text.bytes().all(|b| b.is_ascii_digit()))
        .then(|| text.parse::<u64>().ok())
        .flatten()
        .filter(|n| *n >= 1 && *n <= i64::MAX as u64)
}
fn text(v: &str, min: usize, max: usize) -> bool {
    v.len() >= min && v.len() <= max && !v.contains('\0')
}
fn labels(v: &Option<Vec<String>>) -> bool {
    v.as_ref().is_none_or(|l| {
        l.len() <= 16
            && l.iter().all(|s| text(s, 1, 128) && !s.trim().is_empty())
            && l.iter().collect::<BTreeSet<_>>().len() == l.len()
    })
}
fn update(c: &Current<'_>, patch: serde_json::Map<String, Value>) -> Vec<Value> {
    vec![
        json!({"op":"update_node","id":c.subject,"expected_version":c.version,"patch":Value::Object(patch)}),
    ]
}
fn put(patch: &mut serde_json::Map<String, Value>, key: &str, value: Option<impl Serialize>) {
    if let Some(v) = value {
        patch.insert(key.into(), json!(v));
    }
}

use aq_human_work::TaskDelete;

macro_rules! common {
    ($kind:literal, $entity:literal, $grant:expr) => {
        const KIND: &'static str = $kind;
        const GRANT: &'static str = $grant;
        const ENTITY: &'static str = $entity;
        fn operation_id(&self) -> Uuid {
            self.operation_id
        }
        fn project_id(&self) -> Uuid {
            self.project_id
        }
        fn expected(&self) -> &str {
            &self.expected_version
        }
    };
}

mod adapter;
mod knowledge;
mod retirement;
mod task_payloads;
pub(super) use aq_human_work::{
    DependencyAdd, DependencyRemove, TaskEdit, TaskReprioritize, TaskRoleSet, TaskRouteClear,
    TaskRouteSet,
};

/// Sets (or, with `None`, removes) one key of the task's Graph metadata. A
/// metadata patch replaces the object, so every other key is kept.
fn metadata_patch(c: &Current<'_>, key: &str, value: Option<Value>) -> ApiResult<Vec<Value>> {
    let metadata = c.node.get("metadata").and_then(Value::as_object);
    let mut metadata = metadata.cloned().ok_or_else(ApiError::unavailable)?;
    match value {
        Some(value) => metadata.insert(key.into(), value),
        None => metadata.remove(key),
    };
    let mut patch = serde_json::Map::new();
    patch.insert("metadata".into(), Value::Object(metadata));
    Ok(update(c, patch))
}

/// Spec 0017 §5.4: what a task needs from its environment, set or cleared by
/// the owner before admission, or for a successor after stopped work is reconciled.
#[cfg(feature = "project_environments")]
#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskNeedsSet {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
    /// The capability labels the task needs, stored as Graph metadata
    /// `aq:needs`. Null, absent or empty clears them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub needs: Option<super::enqueue::Needs>,
}

pub(crate) struct GraphAction<P> {
    runtime: Arc<ReadRuntime>,
    supervisor: Option<super::super::supervisor_conversation::reply::SupervisorPrincipal>,
    /// `(attempt, task, project)` of an agent creating under its own grants.
    agent: Option<aq_agent_grants::AttemptIds>,
    guards: BTreeMap<Uuid, u64>,
    agent_workspace: Option<Uuid>,
    payload: std::marker::PhantomData<P>,
}
impl<P> GraphAction<P> {
    pub(super) fn for_supervisor(
        state: &AppState,
        principal: super::super::supervisor_conversation::reply::SupervisorPrincipal,
    ) -> Self {
        let mut adapter = Self::new(state);
        adapter.supervisor = Some(principal);
        adapter
    }
    /// An agent's create; the command surface and the agent route have
    /// checked its grant, scope and quota.
    pub(crate) fn for_agent(
        state: &AppState,
        ids: aq_agent_grants::AttemptIds,
        guards: BTreeMap<Uuid, u64>,
        workspace: Option<Uuid>,
    ) -> Self {
        let mut adapter = Self::new(state);
        adapter.agent = Some(ids);
        adapter.guards = guards;
        adapter.agent_workspace = workspace;
        adapter
    }
    fn machine(&self) -> bool {
        self.supervisor.is_some() || self.agent.is_some()
    }
    async fn project(&self, state: &AppState, user: &User, project: Uuid) -> Result<ProjectRecord> {
        if self.machine() {
            let supervisor = self.supervisor.as_ref();
            let may_enqueue = supervisor.is_none_or(|p| p.may_enqueue(user.id, project));
            let agent_matches = self
                .agent
                .is_none_or(|ids| user.id == ids.attempt_id && ids.project_id == project);
            if !may_enqueue || !agent_matches {
                return Err(invalid());
            }
            let record = project_bindings::current(state, project).await?;
            let workspace = record.binding.graph_workspace_id;
            let matches_adapter = !workspace.is_nil()
                && record.binding.graph_adapter_reference == "aq.graph.configured";
            if !matches_adapter
                || self
                    .agent_workspace
                    .is_some_and(|expected| expected != workspace)
            {
                return Err(unavailable());
            }
            Ok(record)
        } else {
            Ok(access(state, &self.runtime, user.clone(), project)
                .await?
                .record)
        }
    }
    pub(crate) fn new(state: &AppState) -> Self {
        Self {
            runtime: Arc::new(ReadRuntime::new(state)),
            supervisor: None,
            agent: None,
            guards: BTreeMap::new(),
            agent_workspace: None,
            payload: std::marker::PhantomData,
        }
    }
}

pub(crate) fn register(state: &AppState, r: &mut Registry) {
    retirement::register(state, r);
    r.register(GraphAction::<super::enqueue::GraphCreate>::new(state))
        .expect("graph create adapter");
    r.register(GraphAction::<TaskDelete>::new(state))
        .expect("task delete adapter");
    r.register(GraphAction::<aq_human_work::Execution>::new(state))
        .expect("task cancel adapter");
    r.register(GraphAction::<aq_human_work::TaskActivate>::new(state))
        .expect("task activate adapter");
    r.register(GraphAction::<aq_human_work::TaskActivateSubtree>::new(
        state,
    ))
    .expect("subtree activation adapter");
    r.register(GraphAction::<aq_human_work::TaskReopen>::new(state))
        .expect("task reopen adapter");
    r.register(GraphAction::<aq_human_work::TaskComplete>::new(state))
        .expect("task complete adapter");
    r.register(GraphAction::<TaskEdit>::new(state))
        .expect("task edit adapter");
    r.register(GraphAction::<TaskReprioritize>::new(state))
        .expect("task reprioritize adapter");
    r.register(GraphAction::<TaskRoleSet>::new(state))
        .expect("task role set adapter");
    #[cfg(feature = "project_environments")]
    r.register(GraphAction::<TaskNeedsSet>::new(state))
        .expect("task needs set adapter");
    r.register(GraphAction::<TaskRouteSet>::new(state))
        .expect("task route set adapter");
    r.register(GraphAction::<TaskRouteClear>::new(state))
        .expect("task route clear adapter");
    r.register(GraphAction::<DependencyAdd>::new(state))
        .expect("dependency add adapter");
    r.register(GraphAction::<DependencyRemove>::new(state))
        .expect("dependency remove adapter");
    r.register(GraphAction::<KnowledgeAuthor>::new(state))
        .expect("knowledge author adapter");
    r.register(GraphAction::<KnowledgeCorrect>::new(state))
        .expect("knowledge correct adapter");
    r.register(GraphAction::<KnowledgeVerify>::new(state))
        .expect("knowledge verify adapter");
    r.register(GraphAction::<KnowledgeRetire>::new(state))
        .expect("knowledge retire adapter");
    r.register(GraphAction::<KnowledgeRestore>::new(state))
        .expect("knowledge restore adapter");
}
pub(crate) fn schemas(r: &mut Registry) {
    retirement::schemas(r);
    r.register_schema::<GraphAction<super::enqueue::GraphCreate>>()
        .expect("graph create schema");
    r.register_schema::<GraphAction<TaskDelete>>()
        .expect("task delete schema");
    r.register_schema::<GraphAction<aq_human_work::Execution>>()
        .expect("task cancel schema");
    r.register_schema::<GraphAction<aq_human_work::TaskActivate>>()
        .expect("task activate schema");
    r.register_schema::<GraphAction<aq_human_work::TaskActivateSubtree>>()
        .expect("subtree activation schema");
    r.register_schema::<GraphAction<aq_human_work::TaskReopen>>()
        .expect("task reopen schema");
    r.register_schema::<GraphAction<aq_human_work::TaskComplete>>()
        .expect("task complete schema");
    r.register_schema::<GraphAction<TaskEdit>>()
        .expect("task edit schema");
    r.register_schema::<GraphAction<TaskReprioritize>>()
        .expect("task reprioritize schema");
    r.register_schema::<GraphAction<TaskRoleSet>>()
        .expect("task role set schema");
    #[cfg(feature = "project_environments")]
    r.register_schema::<GraphAction<TaskNeedsSet>>()
        .expect("task needs set schema");
    r.register_schema::<GraphAction<TaskRouteSet>>()
        .expect("task route set schema");
    r.register_schema::<GraphAction<TaskRouteClear>>()
        .expect("task route clear schema");
    r.register_schema::<GraphAction<DependencyAdd>>()
        .expect("dependency add schema");
    r.register_schema::<GraphAction<DependencyRemove>>()
        .expect("dependency remove schema");
    r.register_schema::<GraphAction<KnowledgeAuthor>>()
        .expect("knowledge author schema");
    r.register_schema::<GraphAction<KnowledgeCorrect>>()
        .expect("knowledge correct schema");
    r.register_schema::<GraphAction<KnowledgeVerify>>()
        .expect("knowledge verify schema");
    r.register_schema::<GraphAction<KnowledgeRetire>>()
        .expect("knowledge retire schema");
    r.register_schema::<GraphAction<KnowledgeRestore>>()
        .expect("knowledge restore schema");
}

/// Fresh current node version, validated for workspace, kind and liveness.
pub(crate) async fn current(rt: &Runtime, workspace: Uuid, id: Uuid, kind: &str) -> Result<Value> {
    let raw = rt
        .human_get(
            &format!("/v1/workspaces/{workspace}/nodes/{id}"),
            &[],
            1_000_000,
        )
        .await?;
    if raw["id"] != json!(id) {
        return Err(unavailable());
    }
    node(&raw, workspace, kind)?;
    Ok(raw)
}
fn stale(entity: &str, version: u64) -> Outcome {
    Outcome::conflict(BTreeMap::from([(entity.into(), version.to_string())]))
}
fn confirmed(raw: &Value, operations: usize, guards: usize) -> Outcome {
    if !super::super::settlement_response_valid(raw) {
        return Outcome::unknown();
    }
    let Some(results) = raw["results"].as_array().filter(|r| r.len() == operations) else {
        return Outcome::unknown();
    };
    let mut affected = Vec::with_capacity(results.len());
    for r in results.iter().skip(guards) {
        let (Some(id), Some(v)) = (r["id"].as_str(), r["version"].as_u64()) else {
            return Outcome::unknown();
        };
        let kind = if r.get("edge_type").is_some() {
            "graph_edge"
        } else {
            match r["kind"].as_str() {
                Some(k @ ("task" | "knowledge")) => k,
                _ => return Outcome::unknown(),
            }
        };
        affected.push(existing::entity(kind, id, Some(v.to_string())));
    }
    let Some(change_set) = raw["change_set_id"].as_str() else {
        return Outcome::unknown();
    };
    Outcome::confirmed(
        vec![existing::entity("graph_change_set", change_set, None)],
        affected,
    )
}
fn code(raw: &Value) -> Option<&str> {
    raw.get("error")?.get("code")?.as_str()
}

#[cfg(feature = "project_environments")]
pub(crate) mod needs;
