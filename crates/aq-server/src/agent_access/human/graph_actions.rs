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
use async_trait::async_trait;
use axum::http::StatusCode;
use schemars::JsonSchema;
use std::collections::{BTreeMap, BTreeSet};
mod journal;

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
    /// Additional task references that must belong to the subject's project.
    fn tasks(&self) -> Vec<Uuid> {
        vec![]
    }
    fn operations(&self, current: &Current<'_>) -> ApiResult<Vec<Value>>;
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

// Owner task lifecycle never waits for a policy gate or a dependent task.
use aq_human_work::TaskDelete;
impl GraphPayload for TaskDelete {
    const KIND: &'static str = "task.delete";
    const GRANT: &'static str = "task.delete";
    const ENTITY: &'static str = "task";
    fn operation_id(&self) -> Uuid {
        self.operation_id
    }
    fn project_id(&self) -> Uuid {
        self.project_id
    }
    fn expected(&self) -> &str {
        &self.expected_version
    }
    fn valid(&self) -> bool {
        true
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        Ok(vec![
            json!({"op":"delete_node","id":c.subject,"expected_version":c.version}),
        ])
    }
}
impl GraphPayload for aq_human_work::TaskActivate {
    const KIND: &'static str = "task.activate";
    const GRANT: &'static str = "task.activate";
    const ENTITY: &'static str = "task";
    fn operation_id(&self) -> Uuid {
        self.operation_id
    }
    fn project_id(&self) -> Uuid {
        self.project_id
    }
    fn expected(&self) -> &str {
        &self.expected_version
    }
    fn valid(&self) -> bool {
        true
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        // Only a draft: it has never had an attempt, so there is nothing to
        // reconcile. Reopening a finished task is a different question.
        if c.node["task_state"] != "draft" {
            return Err(ApiError::invalid());
        }
        Ok(vec![
            json!({"op":"update_node","id":c.subject,"expected_version":c.version,"patch":{"task_state":"active"}}),
        ])
    }
}
impl GraphPayload for aq_human_work::TaskComplete {
    const KIND: &'static str = "task.complete";
    const GRANT: &'static str = "task.complete";
    const ENTITY: &'static str = "task";
    fn operation_id(&self) -> Uuid {
        self.operation_id
    }
    fn project_id(&self) -> Uuid {
        self.project_id
    }
    fn expected(&self) -> &str {
        &self.expected_version
    }
    fn valid(&self) -> bool {
        true
    }
    fn reason_required() -> bool {
        true
    }
    fn not_held() -> bool {
        true
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        // Settlement completes work that ran. This is for work finished
        // outside AQ, so never for a task a playbook manages: the playbook
        // would read the completion as an accepted result it has no receipt for.
        if c.node["task_state"] != "active"
            || c.node["metadata"].get("aq:playbook_namespace").is_some()
        {
            return Err(ApiError::invalid());
        }
        Ok(vec![
            json!({"op":"update_node","id":c.subject,"expected_version":c.version,"patch":{"task_state":"completed"}}),
        ])
    }
}
impl GraphPayload for aq_human_work::TaskReopen {
    const KIND: &'static str = "task.reopen";
    const GRANT: &'static str = "task.reopen";
    const ENTITY: &'static str = "task";
    fn operation_id(&self) -> Uuid {
        self.operation_id
    }
    fn project_id(&self) -> Uuid {
        self.project_id
    }
    fn expected(&self) -> &str {
        &self.expected_version
    }
    fn valid(&self) -> bool {
        true
    }
    fn never_attempted() -> bool {
        true
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        if !matches!(
            c.node["task_state"].as_str(),
            Some("completed" | "canceled")
        ) {
            return Err(ApiError::invalid());
        }
        Ok(vec![
            json!({"op":"update_node","id":c.subject,"expected_version":c.version,"patch":{"task_state":"active"}}),
        ])
    }
}
impl GraphPayload for aq_human_work::Execution {
    const KIND: &'static str = "task.cancel";
    const GRANT: &'static str = "task.cancel";
    const ENTITY: &'static str = "task";
    fn operation_id(&self) -> Uuid {
        self.operation_id
    }
    fn project_id(&self) -> Uuid {
        self.project_id
    }
    fn expected(&self) -> &str {
        &self.expected_version
    }
    fn valid(&self) -> bool {
        true
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        Ok(vec![
            json!({"op":"update_node","id":c.subject,"expected_version":c.version,"patch":{"task_state":"canceled"}}),
        ])
    }
}

#[derive(Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ContentType {
    Fact,
    Decision,
    Policy,
    Procedure,
    Incident,
    Reference,
    Note,
}
#[derive(Clone, Copy, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Verdict {
    Verified,
    Disputed,
}

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

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskEdit {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 512))]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 32768))]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 8192))]
    pub acceptance_criteria: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 16))]
    pub labels: Option<Vec<String>>,
}
impl GraphPayload for TaskEdit {
    common!("task.edit", "task", aq_agent_grants::op::TASK_EDIT);
    fn valid(&self) -> bool {
        (self.title.is_some()
            || self.body.is_some()
            || self.acceptance_criteria.is_some()
            || self.labels.is_some())
            && self
                .title
                .as_deref()
                .is_none_or(|t| text(t, 1, 512) && !t.trim().is_empty())
            && self.body.as_deref().is_none_or(|t| text(t, 0, 32768))
            && self
                .acceptance_criteria
                .as_deref()
                .is_none_or(|t| text(t, 0, 8192))
            && labels(&self.labels)
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        let mut patch = serde_json::Map::new();
        put(&mut patch, "title", self.title.as_ref());
        put(&mut patch, "body", self.body.as_ref());
        put(
            &mut patch,
            "acceptance_criteria",
            self.acceptance_criteria.as_ref(),
        );
        put(&mut patch, "labels", self.labels.as_ref());
        Ok(update(c, patch))
    }
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct TaskReprioritize {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
    /// Stored as Graph metadata `aq:priority`; the scheduler sorts descending.
    #[schemars(range(min = -1_000_000, max = 1_000_000))]
    pub priority: i64,
}
impl GraphPayload for TaskReprioritize {
    common!("task.reprioritize", "task", aq_agent_grants::op::TASK_EDIT);
    fn valid(&self) -> bool {
        (-1_000_000..=1_000_000).contains(&self.priority)
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        // A Graph metadata patch replaces the object; keep every other key.
        let mut metadata = c
            .node
            .get("metadata")
            .and_then(Value::as_object)
            .cloned()
            .ok_or_else(ApiError::unavailable)?;
        metadata.insert("aq:priority".into(), json!(self.priority));
        let mut patch = serde_json::Map::new();
        patch.insert("metadata".into(), Value::Object(metadata));
        Ok(update(c, patch))
    }
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DependencyAdd {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
    /// The task that must finish first: `blocker blocks subject`.
    pub blocker_id: Uuid,
}
impl GraphPayload for DependencyAdd {
    common!(
        "task.dependency.add",
        "task",
        aq_agent_grants::op::TASK_EDIT
    );
    fn valid(&self) -> bool {
        !self.blocker_id.is_nil()
    }
    fn tasks(&self) -> Vec<Uuid> {
        vec![self.blocker_id]
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        if self.blocker_id == c.subject {
            return Err(ApiError::invalid());
        }
        // Structural edges reject revision pins; a duplicate link is a Graph no-op.
        Ok(vec![
            json!({"op":"add_edge","id":self.operation_id,"source":self.blocker_id,"target":c.subject,"edge_type":"blocks","source_revision":null,"target_revision":null}),
        ])
    }
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct DependencyRemove {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
    pub edge_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub edge_version: String,
}
impl GraphPayload for DependencyRemove {
    common!(
        "task.dependency.remove",
        "task",
        aq_agent_grants::op::TASK_EDIT
    );
    fn valid(&self) -> bool {
        !self.edge_id.is_nil() && version(&self.edge_version).is_some()
    }
    fn operations(&self, _: &Current<'_>) -> ApiResult<Vec<Value>> {
        let v = version(&self.edge_version).ok_or_else(ApiError::invalid)?;
        Ok(vec![
            json!({"op":"remove_edge","id":self.edge_id,"expected_version":v}),
        ])
    }
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KnowledgeAuthor {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_project_revision: String,
    #[schemars(length(min = 1, max = 512))]
    pub title: String,
    #[schemars(length(max = 32768))]
    pub body: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 16))]
    pub labels: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<ContentType>,
}
impl GraphPayload for KnowledgeAuthor {
    const KIND: &'static str = "knowledge.author";
    const GRANT: &'static str = aq_agent_grants::op::KNOWLEDGE_AUTHOR;
    const TARGET: aq_command_registry::TargetRule =
        aq_command_registry::TargetRule::Project("project_id");
    const ENTITY: &'static str = "project";
    fn operation_id(&self) -> Uuid {
        self.operation_id
    }
    fn project_id(&self) -> Uuid {
        self.project_id
    }
    fn expected(&self) -> &str {
        &self.expected_project_revision
    }
    fn valid(&self) -> bool {
        text(&self.title, 1, 512)
            && !self.title.trim().is_empty()
            && text(&self.body, 0, 32768)
            && labels(&self.labels)
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        // Authoring never asserts verification; review is a separate admin verify.
        Ok(vec![json!({
            "op":"create_node","id":self.operation_id,"kind":"knowledge",
            "title":self.title,"body":self.body,
            "labels":self.labels.clone().unwrap_or_default(),
            "content_type":self.content_type.unwrap_or(ContentType::Note),
            "verification":"unverified",
            "metadata":{"aq:human_actor_id":c.actor,"aq:human_operation_id":self.operation_id,"aq:project_id":c.project}
        })])
    }
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KnowledgeCorrect {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(min = 1, max = 512))]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 32768))]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schemars(length(max = 16))]
    pub labels: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_type: Option<ContentType>,
}
impl GraphPayload for KnowledgeCorrect {
    common!(
        "knowledge.correct",
        "knowledge",
        aq_agent_grants::op::KNOWLEDGE_AUTHOR
    );
    fn valid(&self) -> bool {
        (self.title.is_some()
            || self.body.is_some()
            || self.labels.is_some()
            || self.content_type.is_some())
            && self
                .title
                .as_deref()
                .is_none_or(|t| text(t, 1, 512) && !t.trim().is_empty())
            && self.body.as_deref().is_none_or(|t| text(t, 0, 32768))
            && labels(&self.labels)
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        let mut patch = serde_json::Map::new();
        put(&mut patch, "title", self.title.as_ref());
        put(&mut patch, "body", self.body.as_ref());
        put(&mut patch, "labels", self.labels.as_ref());
        put(&mut patch, "content_type", self.content_type);
        Ok(update(c, patch))
    }
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KnowledgeVerify {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
    pub verification: Verdict,
    /// Graph `Source` objects; each is validated against the pinned Graph schema.
    #[schemars(length(min = 1, max = 64))]
    pub evidence: Vec<serde_json::Map<String, Value>>,
}
impl GraphPayload for KnowledgeVerify {
    common!("knowledge.verify", "knowledge", "knowledge.verify");
    fn reason_required() -> bool {
        true
    }
    fn valid(&self) -> bool {
        (1..=64).contains(&self.evidence.len())
            && self
                .evidence
                .iter()
                .all(|s| contract::upstream_valid("Source", &Value::Object(s.clone())))
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        Ok(vec![
            json!({"op":"verify_node","id":c.subject,"expected_version":c.version,"verification":self.verification,"evidence":self.evidence}),
        ])
    }
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KnowledgeRetire {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
    #[schemars(length(min = 1, max = 4096))]
    pub retirement_reason: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub successor_id: Option<Uuid>,
}
impl GraphPayload for KnowledgeRetire {
    common!("knowledge.retire", "knowledge", "knowledge.retire");
    fn valid(&self) -> bool {
        text(&self.retirement_reason, 1, 4096)
            && !self.retirement_reason.trim().is_empty()
            && self.successor_id.is_none_or(|s| !s.is_nil())
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        // Retiring again would append a revision and overwrite the reason (N-03).
        if self.successor_id == Some(c.subject) || c.node["retired"] != false {
            return Err(ApiError::invalid());
        }
        let mut patch = serde_json::Map::new();
        patch.insert("retired".into(), json!(true));
        patch.insert("retirement_reason".into(), json!(self.retirement_reason));
        put(&mut patch, "successor_id", self.successor_id);
        Ok(update(c, patch))
    }
}

#[derive(Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub(crate) struct KnowledgeRestore {
    pub operation_id: Uuid,
    pub project_id: Uuid,
    #[schemars(regex(pattern = "^[1-9][0-9]{0,18}$"))]
    pub expected_version: String,
    #[schemars(range(min = 1))]
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub include_lifecycle: Option<bool>,
}
impl GraphPayload for KnowledgeRestore {
    common!("knowledge.restore", "knowledge", "knowledge.restore");
    fn valid(&self) -> bool {
        self.revision >= 1 && self.revision <= i64::MAX as u64
    }
    fn operations(&self, c: &Current<'_>) -> ApiResult<Vec<Value>> {
        let mut op = json!({"op":"restore_node","id":c.subject,"expected_version":c.version,"revision":self.revision});
        if let Some(lifecycle) = self.include_lifecycle {
            op["include_lifecycle"] = json!(lifecycle);
        }
        Ok(vec![op])
    }
}

pub(crate) struct GraphAction<P> {
    runtime: Arc<ReadRuntime>,
    supervisor: Option<super::super::supervisor_conversation::reply::SupervisorPrincipal>,
    /// `(attempt, task, project)` of an agent creating under its own grants.
    agent: Option<(Uuid, Uuid, Uuid)>,
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
    pub(crate) fn for_agent(state: &AppState, attempt: Uuid, task: Uuid, project: Uuid) -> Self {
        let mut adapter = Self::new(state);
        adapter.agent = Some((attempt, task, project));
        adapter
    }
    fn machine(&self) -> bool {
        self.supervisor.is_some() || self.agent.is_some()
    }
    async fn project(&self, state: &AppState, user: &User, project: Uuid) -> Result<ProjectRecord> {
        if self.machine() {
            let supervisor = self.supervisor.as_ref().is_none_or(|p| {
                user.id == p.session_id
                    && p.scopes.iter().any(|s| s == "enqueue")
                    && p.project_id.is_none_or(|id| id == project)
            });
            if !supervisor
                || self
                    .agent
                    .is_some_and(|(a, _, p)| user.id != a || p != project)
            {
                return Err(invalid());
            }
            let record = project_bindings::current(state, project).await?;
            if record.binding.graph_workspace_id.is_nil()
                || record.binding.graph_adapter_reference != "aq.graph.configured"
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
            payload: std::marker::PhantomData,
        }
    }
}

pub(crate) fn register(state: &AppState, r: &mut Registry) {
    r.register(GraphAction::<super::enqueue::GraphCreate>::new(state))
        .expect("graph create adapter");
    r.register(GraphAction::<TaskDelete>::new(state))
        .expect("task delete adapter");
    r.register(GraphAction::<aq_human_work::Execution>::new(state))
        .expect("task cancel adapter");
    r.register(GraphAction::<aq_human_work::TaskActivate>::new(state))
        .expect("task activate adapter");
    r.register(GraphAction::<aq_human_work::TaskReopen>::new(state))
        .expect("task reopen adapter");
    r.register(GraphAction::<aq_human_work::TaskComplete>::new(state))
        .expect("task complete adapter");
    r.register(GraphAction::<TaskEdit>::new(state))
        .expect("task edit adapter");
    r.register(GraphAction::<TaskReprioritize>::new(state))
        .expect("task reprioritize adapter");
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
    r.register_schema::<GraphAction<super::enqueue::GraphCreate>>()
        .expect("graph create schema");
    r.register_schema::<GraphAction<TaskDelete>>()
        .expect("task delete schema");
    r.register_schema::<GraphAction<aq_human_work::Execution>>()
        .expect("task cancel schema");
    r.register_schema::<GraphAction<aq_human_work::TaskActivate>>()
        .expect("task activate schema");
    r.register_schema::<GraphAction<aq_human_work::TaskReopen>>()
        .expect("task reopen schema");
    r.register_schema::<GraphAction<aq_human_work::TaskComplete>>()
        .expect("task complete schema");
    r.register_schema::<GraphAction<TaskEdit>>()
        .expect("task edit schema");
    r.register_schema::<GraphAction<TaskReprioritize>>()
        .expect("task reprioritize schema");
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

/// Shared typed-subject admission: owner, current binding, fresh subject read.
#[allow(clippy::too_many_arguments)]
pub(super) async fn admit(
    state: &AppState,
    runtime: &ReadRuntime,
    user: &User,
    request: &ActionRequest,
    operation: Uuid,
    project: Uuid,
    entity: &str,
    expected: &str,
    tasks: &[Uuid],
) -> ApiResult<()> {
    if request.operation_id != operation || version(expected).is_none() {
        return Err(ApiError::invalid());
    }
    let subject = existing::subject(request, entity, entity, expected)?;
    if entity == "project" && subject != project {
        return Err(ApiError::invalid());
    }
    existing::owner(state, user).await?;
    if entity == "project" || request.kind == "task.delete" {
        access(state, runtime, user.clone(), project)
            .await
            .map_err(ApiError::from)?;
    } else {
        let _ = detail(state, runtime, user.clone(), project, subject, entity, None)
            .await
            .map_err(ApiError::from)?;
    }
    for task in tasks {
        let _ = detail(state, runtime, user.clone(), project, *task, "task", None)
            .await
            .map_err(ApiError::from)?;
    }
    if !task_create_available(state) {
        return Err(ApiError::unavailable());
    }
    Ok(())
}
/// Lookup rechecks owner, the current project binding and the subject type.
pub(super) async fn lookup(
    state: &AppState,
    runtime: &ReadRuntime,
    user: &User,
    scope: &Scope,
    operation: Option<&actions::Operation>,
    entity: &str,
) -> ApiResult<()> {
    existing::owner(state, user).await?;
    let Scope::Project { project_id } = scope else {
        return Err(ApiError::invalid());
    };
    access(state, runtime, user.clone(), *project_id)
        .await
        .map_err(ApiError::from)?;
    if let Some(op) = operation
        && (op.subject_refs.len() != 1
            || op.subject_refs[0].entity_type != entity
            || (entity == "project" && op.subject_refs[0].id != project_id.to_string()))
    {
        return Err(ApiError::invalid());
    }
    Ok(())
}
/// Fresh current node version, validated for workspace, kind and liveness.
pub(super) async fn current(rt: &Runtime, workspace: Uuid, id: Uuid, kind: &str) -> Result<Value> {
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
fn confirmed(raw: &Value, operations: usize) -> Outcome {
    if !super::super::settlement_response_valid(raw) {
        return Outcome::unknown();
    }
    let Some(results) = raw["results"].as_array().filter(|r| r.len() == operations) else {
        return Outcome::unknown();
    };
    let mut affected = Vec::with_capacity(results.len());
    for r in results {
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

#[async_trait]
impl<P: GraphPayload> ActionKind for GraphAction<P> {
    type Payload = P;
    const KIND: &'static str = P::KIND;
    const GRANT: &'static str = P::GRANT;
    const TARGET: aq_command_registry::TargetRule = P::TARGET;
    async fn authorize(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        payload: &P,
    ) -> ApiResult<Scope> {
        if !payload.valid()
            || (P::reason_required()
                && request
                    .reason
                    .as_deref()
                    .is_none_or(|r| r.trim().is_empty()))
        {
            return Err(ApiError::invalid());
        }
        if !self.machine() {
            admit(
                state,
                &self.runtime,
                user,
                request,
                payload.operation_id(),
                payload.project_id(),
                P::ENTITY,
                payload.expected(),
                &payload.tasks(),
            )
            .await?;
        } else if request.operation_id != payload.operation_id()
            || version(payload.expected()).is_none()
            || existing::subject(request, P::ENTITY, P::ENTITY, payload.expected())?
                != payload.project_id()
        {
            return Err(ApiError::invalid());
        }
        if self.machine() || P::KIND == "task.graph.create" {
            let record = self
                .project(state, user, payload.project_id())
                .await
                .map_err(ApiError::from)?;
            let rt = runtime(state).map_err(ApiError::from)?;
            for id in payload.tasks() {
                let node = current(rt, record.binding.graph_workspace_id, id, "task")
                    .await
                    .map_err(ApiError::from)?;
                if node["metadata"]["aq:project_id"] != json!(payload.project_id()) {
                    return Err(ApiError::for_status(StatusCode::FORBIDDEN));
                }
            }
        }
        Ok(Scope::Project {
            project_id: payload.project_id(),
        })
    }
    async fn authorize_lookup(
        &self,
        state: &AppState,
        user: &User,
        scope: &Scope,
        operation: Option<&actions::Operation>,
    ) -> ApiResult<()> {
        if !self.machine() {
            return lookup(state, &self.runtime, user, scope, operation, P::ENTITY).await;
        }
        let Scope::Project { project_id } = scope else {
            return Err(ApiError::invalid());
        };
        self.project(state, user, *project_id)
            .await
            .map_err(ApiError::from)?;
        if operation.is_some_and(|op| {
            op.subject_refs.len() != 1
                || op.subject_refs[0].entity_type != "project"
                || op.subject_refs[0].id != project_id.to_string()
        }) {
            return Err(ApiError::invalid());
        }
        Ok(())
    }
    async fn reconcile(
        &self,
        state: &AppState,
        user: &User,
        operation: &actions::Operation,
    ) -> ApiResult<Option<Outcome>> {
        let Some(intent) = journal::load(state, user, operation.operation_id, P::KIND)
            .await
            .map_err(ApiError::from)?
        else {
            // Pre-journal operations have no exact body to replay safely.
            return Ok(None);
        };
        let record = self
            .project(state, user, intent.project)
            .await
            .map_err(ApiError::from)?;
        let rt = runtime(state).map_err(ApiError::from)?;
        if operation.scope
            != (Scope::Project {
                project_id: intent.project,
            })
            || record.revision != intent.revision
            || json!(record.binding) != intent.binding
            || super::super::settlement_service_identity(rt, intent.workspace)
                .ok()
                .as_ref()
                != Some(&intent.service)
        {
            return Ok(Some(Outcome::unknown()));
        }
        // Graph scopes receipts by actor/workspace/operation/key and checks the
        // payload hash before executing. Replay the retained bytes, including
        // the original reason and metadata; never rebuild from current state.
        let posted = tokio::time::timeout(
            Duration::from_millis(1500),
            super::super::settlement_post(rt, intent.workspace, intent.body),
        )
        .await;
        Ok(Some(match posted {
            Ok(Ok((status, raw))) if status.is_success() => confirmed(&raw, intent.operations),
            // A new authorization/conflict/transport failure cannot disprove
            // an earlier commit. Only its original receipt settles the effect.
            _ => Outcome::unknown(),
        }))
    }
    async fn execute(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        payload: P,
    ) -> Outcome {
        match self.run(state, user, request, &payload).await {
            Ok(outcome) => outcome,
            Err(error) => existing::failure(error),
        }
    }
}
impl<P: GraphPayload> GraphAction<P> {
    async fn run(
        &self,
        state: &AppState,
        user: &User,
        request: &ActionRequest,
        payload: &P,
    ) -> Result<Outcome> {
        let subject: Uuid = request.subject.id.parse().map_err(|_| invalid())?;
        let expected = version(payload.expected()).ok_or_else(invalid)?;
        let project = payload.project_id();
        let record = self.project(state, user, project).await?;
        let workspace = record.binding.graph_workspace_id;
        let rt = runtime(state)?;
        let (node, live) = if P::ENTITY == "project" {
            (Value::Null, record.revision)
        } else {
            let raw = current(rt, workspace, subject, P::ENTITY).await?;
            let v = raw["version"].as_u64().ok_or_else(unavailable)?;
            (raw, v)
        };
        if live != expected {
            return Ok(stale(P::ENTITY, live));
        }
        let operations = payload
            .operations(&Current {
                subject,
                actor: user.id,
                supervisor_actor: self.supervisor.as_ref().map(|p| p.actor_id),
                agent: self.agent.map(|(a, t, _)| (a, t)),
                project,
                version: live,
                node: &node,
            })
            .map_err(|_| invalid())?;
        if P::never_attempted() || P::not_held() {
            let attempted: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM execution_attempts WHERE task_id=$1 AND ($2 OR holds_task))",
            )
            .bind(subject)
            .bind(P::never_attempted())
            .fetch_one(&state.store.pool)
            .await
            .map_err(|_| AppError::Internal)?;
            if attempted {
                return Ok(existing::disposition(Disposition::Rejected));
            }
        }
        let id = payload.operation_id();
        let reason = request
            .reason
            .clone()
            .unwrap_or_else(|| format!("AQ dashboard action {} {id}", P::KIND));
        let body = json!({"idempotency_key":format!("aq-dashboard-action:{id}"),"reason":reason,"operations":operations});
        if !contract::upstream_valid("MutationRequest", &body) {
            return Err(invalid());
        }
        // This is native Graph JSON, which can include fractional metadata.
        // Integer-only canonical hashing belongs to the action request boundary.
        let bytes = serde_json::to_vec(&body).map_err(|_| invalid())?;
        let service = super::super::settlement_service_identity(rt, workspace)?;
        journal::retain(state, user, id, &record, &service, &bytes).await?;
        // From here an error is an unknown effect, never a rejection.
        let posted = tokio::time::timeout(
            Duration::from_secs(3),
            super::super::settlement_post(rt, workspace, bytes),
        )
        .await;
        let Ok(Ok((status, raw))) = posted else {
            return Ok(Outcome::unknown());
        };
        Ok(match status.as_u16() {
            200..=299 => confirmed(&raw, operations.len()),
            409 if code(&raw) == Some("conflict") => {
                if P::ENTITY == "project" {
                    existing::disposition(Disposition::Conflict)
                } else {
                    match current(rt, workspace, subject, P::ENTITY).await {
                        // Graph refused the change itself, not its version
                        // (an open child prevents a parent's completion).
                        Ok(n) if n["version"].as_u64() == Some(live) => {
                            existing::disposition(Disposition::Rejected)
                        }
                        Ok(n) => n["version"].as_u64().map_or_else(
                            || Outcome::conflict(BTreeMap::new()),
                            |v| stale(P::ENTITY, v),
                        ),
                        Err(_) => Outcome::conflict(BTreeMap::new()),
                    }
                }
            }
            400 | 401 | 403 | 404 | 422 if code(&raw).is_some() => {
                existing::disposition(Disposition::Rejected)
            }
            _ => Outcome::unknown(),
        })
    }
}
