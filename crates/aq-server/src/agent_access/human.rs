//! Owner-authorized read projection. Graph service credentials never leave AQ.
mod commands;
pub(crate) mod comments;
pub(crate) use commands::action::TaskCreate;
mod cursor;
pub(crate) mod enqueue;
pub(crate) mod graph_actions;
mod overview;
pub(crate) mod task_execution;
pub mod task_termination;
mod work;
use super::{Runtime, contract};
use crate::{
    AppState,
    authz::RequireOwner,
    error::AppError,
    project_bindings::{self, domain::ProjectRecord},
    store::User,
};
use aq_human_work::task_list;
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, Request, State},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{PgPool, Row, postgres::PgPoolOptions};
use std::{sync::Arc, time::Duration};
use uuid::Uuid;
type Result<T> = std::result::Result<T, AppError>;
fn invalid() -> AppError {
    AppError::BadRequest("invalid bounded human Graph read".into())
}
fn unavailable() -> AppError {
    AppError::IdentityUnavailable("Graph read unavailable".into())
}
fn conflict() -> AppError {
    AppError::Conflict("project, authorization or cursor changed; restart this read".into())
}
fn stamp(state: &AppState) -> i64 {
    (state.clock)().timestamp_millis()
}
/// Human Graph reads: 4 run (one per pooled connection), 12 more wait up to
/// [`crate::read_gate::QUEUE_WAIT`]; see docs/development.md "Read capacity".
const RUNNING: usize = 4;
pub struct ReadRuntime {
    pool: PgPool,
    gate: crate::read_gate::ReadGate,
    cursors: cursor::Codec,
}
impl ReadRuntime {
    /// Internal aggregate readers share their existing bounded SQL pool.
    pub(crate) fn with_pool(pool: PgPool) -> Self {
        Self {
            pool,
            gate: crate::read_gate::ReadGate::new(RUNNING, 12),
            cursors: cursor::Codec::new(),
        }
    }
    pub(crate) fn new(state: &AppState) -> Self {
        Self {
            pool: PgPoolOptions::new()
                .max_connections(RUNNING as u32)
                .acquire_timeout(Duration::from_millis(500))
                .connect_lazy_with(
                    state
                        .store
                        .pool
                        .connect_options()
                        .as_ref()
                        .clone()
                        .application_name("aq-human-graph-read"),
                ),
            gate: crate::read_gate::ReadGate::new(RUNNING, 12),
            cursors: cursor::Codec::new(),
        }
    }
}
pub fn task_create_available(state: &AppState) -> bool {
    state
        .agent_access
        .as_deref()
        .is_some_and(|rt| rt.mutation_actor.is_some())
}
pub fn routes(state: &AppState) -> Router<AppState> {
    let runtime = Arc::new(ReadRuntime::new(state));
    Router::new()
        .route("/api/human/overview/tasks", get(overview::read))
        .route("/api/human/work/tasks", get(all_tasks))
        .route(
            "/api/human/projects/{project}/tasks",
            get(tasks).post(commands::create),
        )
        .route(
            "/api/human/projects/{project}/workers/{node}/launch",
            axum::routing::post(commands::launch::create),
        )
        .route(
            "/api/human/workers/{node}/launch-projects",
            get(commands::launch::projects),
        )
        .route("/api/human/operations/{operation}", get(commands::lookup))
        .route(
            "/api/human/operations/{operation}/recover",
            axum::routing::post(commands::recover),
        )
        .layer(axum::extract::DefaultBodyLimit::max(65536))
        .route("/api/human/projects/{project}/tasks/{node}", get(task))
        .route(
            "/api/human/projects/{project}/tasks/{node}/graph",
            get(graph),
        )
        .route(
            "/api/human/projects/{project}/tasks/{node}/work",
            get(work::work),
        )
        .route(
            "/api/human/projects/{project}/tasks/{node}/history",
            get(work::history),
        )
        .route(
            "/api/human/projects/{project}/tasks/{node}/summary",
            get(work::summary),
        )
        .route(
            "/api/human/projects/{project}/tasks/{node}/comments",
            get(comments::read),
        )
        .route("/api/human/projects/{project}/knowledge", get(knowledge))
        .route(
            "/api/human/projects/{project}/knowledge/{node}",
            get(document),
        )
        .layer(Extension(runtime.clone()))
        .layer(middleware::from_fn_with_state(runtime, gate))
}
async fn gate(State(r): State<Arc<ReadRuntime>>, request: Request, next: Next) -> Response {
    let Some(_permits) = r.gate.enter().await else {
        return unavailable().into_response();
    };
    match tokio::time::timeout(Duration::from_secs(5), next.run(request)).await {
        Ok(mut response) => {
            response
                .headers_mut()
                .insert("cache-control", "no-store".parse().unwrap());
            response
        }
        Err(_) => unavailable().into_response(),
    }
}
#[derive(Clone, Serialize)]
struct Project {
    id: Uuid,
    name: String,
    revision: String,
    workspace_id: Uuid,
}
#[derive(Serialize)]
struct Envelope {
    contract: &'static str,
    project: Project,
    observed_at: String,
    data: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Page {
    limit: Option<u16>,
    cursor: Option<String>,
    q: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Revision {
    revision: Option<u64>,
}
struct Access {
    actor: User,
    record: ProjectRecord,
    requested: i64,
}
async fn owner(r: &ReadRuntime, actor: &User) -> Result<()> {
    let row = sqlx::query("SELECT role,email,issuer,subject FROM users WHERE id=$1")
        .bind(actor.id)
        .fetch_optional(&r.pool)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(conflict)?;
    if row.get::<String, _>("role") != "owner"
        || row.get::<String, _>("email") != actor.email
        || row.get::<Option<String>, _>("issuer") != actor.issuer
        || row.get::<Option<String>, _>("subject") != actor.subject
    {
        return Err(conflict());
    }
    Ok(())
}
pub(crate) async fn authorize_project(state: &AppState, actor: &User, project: Uuid) -> Result<()> {
    let runtime = ReadRuntime::new(state);
    access(state, &runtime, actor.clone(), project)
        .await
        .map(|_| ())
}
/// Reuse the bounded current-project and Graph task read for feedback references.
pub(crate) async fn authorize_task(
    state: &AppState,
    actor: &User,
    project: Uuid,
    task: Uuid,
) -> Result<()> {
    let runtime = ReadRuntime::new(state);
    detail(state, &runtime, actor.clone(), project, task, "task", None)
        .await
        .map(|_| ())
}
/// Bounded task title and direct downstream reach for decision views.
#[derive(Clone, Debug)]
pub(crate) struct TaskBrief {
    pub title: Option<String>,
    pub blocked_tasks: u32,
    pub truncated: bool,
}
/// One authorized depth-1 outgoing context read of a task node. `blocks`
/// edges run from the blocker (source) to the blocked task (target).
pub(crate) async fn task_brief(
    state: &AppState,
    r: &ReadRuntime,
    actor: &User,
    project: Uuid,
    task: Uuid,
) -> Result<TaskBrief> {
    if task.is_nil() {
        return Err(invalid());
    }
    let a = access(state, r, actor.clone(), project).await?;
    let w = a.record.binding.graph_workspace_id;
    let raw = runtime(state)?
        .human_get(
            &format!("/v1/workspaces/{w}/nodes/{task}/context"),
            &[
                ("depth".into(), "1".into()),
                ("limit".into(), "63".into()),
                ("max_bytes".into(), "262144".into()),
                ("direction".into(), "outgoing".into()),
            ],
            262144,
        )
        .await?;
    if !contract::upstream_valid("Context", &raw) || raw["node"]["id"] != json!(task) {
        return Err(unavailable());
    }
    let root = node(&raw["node"], w, "task")?;
    let blocked_tasks = aq_human_work::blocked_by(&raw, w, task).ok_or_else(unavailable)?;
    fence_records(r, &a.actor, std::slice::from_ref(&a.record)).await?;
    Ok(TaskBrief {
        title: root["title"].as_str().map(str::to_owned),
        blocked_tasks,
        truncated: raw["truncated"].as_bool().ok_or_else(unavailable)?,
    })
}
async fn access(state: &AppState, r: &ReadRuntime, actor: User, project: Uuid) -> Result<Access> {
    if project.is_nil() {
        return Err(invalid());
    }
    owner(r, &actor).await?;
    let mut view = state.clone();
    view.store.pool = r.pool.clone();
    let record = project_bindings::current(&view, project).await?;
    if record.binding.graph_workspace_id.is_nil()
        || record.binding.graph_adapter_reference != "aq.graph.configured"
    {
        return Err(unavailable());
    }
    Ok(Access {
        actor,
        record,
        requested: stamp(state),
    })
}
async fn fence_records(r: &ReadRuntime, actor: &User, records: &[ProjectRecord]) -> Result<()> {
    task_list::fence_records(&r.pool, &work_owner(actor), records)
        .await
        .map_err(work_error)
}
fn work_owner(actor: &User) -> task_list::Owner<'_> {
    task_list::Owner {
        id: actor.id,
        email: &actor.email,
        issuer: actor.issuer.as_deref(),
        subject: actor.subject.as_deref(),
    }
}
fn work_error(error: task_list::Error) -> AppError {
    match error {
        task_list::Error::Invalid => invalid(),
        task_list::Error::Changed => conflict(),
        task_list::Error::Unavailable => unavailable(),
    }
}
async fn finish(
    state: &AppState,
    r: &ReadRuntime,
    a: Access,
    mut data: Value,
) -> Result<Json<Envelope>> {
    aq_human_work::attribution::records(
        &r.pool,
        state
            .agent_access
            .as_ref()
            .and_then(|rt| rt.mutation_actor.as_deref()),
        &mut data,
    )
    .await
    .map_err(|_| unavailable())?;
    fence_records(r, &a.actor, std::slice::from_ref(&a.record)).await?;
    let now = stamp(state);
    if now < a.requested || now - a.requested >= 5000 {
        return Err(unavailable());
    }
    let reply = Envelope {
        contract: "human-graph-1",
        project: Project {
            id: a.record.binding.project_id,
            name: a.record.binding.name,
            revision: a.record.revision.to_string(),
            workspace_id: a.record.binding.graph_workspace_id,
        },
        observed_at: chrono::DateTime::from_timestamp_millis(now)
            .ok_or_else(unavailable)?
            .to_rfc3339(),
        data,
    };
    if serde_json::to_vec(&reply).map_err(|_| unavailable())?.len() > 1_048_576 {
        return Err(unavailable());
    }
    Ok(Json(reply))
}
impl Runtime {
    pub(super) async fn human_get(
        &self,
        path: &str,
        query: &[(String, String)],
        max: usize,
    ) -> Result<Value> {
        let request = self
            .client
            .get(self.url.join(path).map_err(|_| invalid())?)
            .query(query);
        self.human_response(request, max).await
    }
    async fn human_response(&self, request: reqwest::RequestBuilder, max: usize) -> Result<Value> {
        let bytes = aq_human_http::graph::read(request.bearer_auth(&self.token), max)
            .await
            .map_err(|e| match e {
                aq_human_http::Error::Status(404) => {
                    AppError::NotFound("Graph record not found".into())
                }
                _ => unavailable(),
            })?;
        // Same strict duplicate-key/depth parser as the existing scheduling reader.
        super::scheduling::parse::parse(&bytes).map_err(|_| unavailable())
    }
}
async fn all_tasks(
    State(s): State<AppState>,
    Extension(r): Extension<Arc<ReadRuntime>>,
    RequireOwner(u): RequireOwner,
    q: std::result::Result<
        Query<aq_human_work::task_list::Query>,
        axum::extract::rejection::QueryRejection,
    >,
) -> Result<Json<aq_human_work::task_list::Reply>> {
    let Query(q) = q.map_err(|_| invalid())?;
    let graph = runtime(&s)?;
    let url = graph.url.join("/v1/task-list").map_err(|_| unavailable())?;
    task_list::read(
        &r.pool,
        &r.cursors.shared,
        work_owner(&u),
        q,
        stamp(&s),
        async |body| {
            graph
                .human_response(graph.client.post(url).json(&body), 524288)
                .await
                .map_err(|_| task_list::Error::Unavailable)
        },
    )
    .await
    .map(Json)
    .map_err(work_error)
}
fn runtime(state: &AppState) -> Result<&Runtime> {
    state.agent_access.as_deref().ok_or_else(unavailable)
}
fn node(raw: &Value, workspace: Uuid, kind: &str) -> Result<Value> {
    if !contract::upstream_valid("Node", raw)
        || raw["workspace_id"] != json!(workspace)
        || raw["kind"] != kind
        || raw["deleted"] != false
    {
        return Err(unavailable());
    }
    aq_human_work::projection::node(raw, kind).ok_or_else(unavailable)
}
async fn list(
    state: &AppState,
    r: &ReadRuntime,
    actor: User,
    project: Uuid,
    page: Page,
    kind: &str,
) -> Result<Json<Envelope>> {
    let limit = page.limit.unwrap_or(25);
    if !(1..=64).contains(&limit)
        || page.q.as_ref().is_some_and(|q| {
            q.trim().is_empty() || q.len() > 256 || q.chars().any(char::is_control)
        })
    {
        return Err(invalid());
    }
    let a = access(state, r, actor, project).await?;
    let binding = cursor::Binding {
        actor: a.actor.id,
        project,
        revision: a.record.revision,
        workspace: a.record.binding.graph_workspace_id,
        kind: kind.into(),
        limit,
        q: page.q.clone(),
    };
    let mut query = vec![
        ("kind".into(), kind.into()),
        ("limit".into(), limit.to_string()),
    ];
    if let Some(token) = page.cursor {
        query.push((
            "cursor".into(),
            r.cursors.decode(&token, &binding, stamp(state))?,
        ))
    }
    if let Some(q) = page.q {
        query.push(("q".into(), q))
    }
    let endpoint = if binding.q.is_some() {
        "search"
    } else {
        "nodes"
    };
    let raw = runtime(state)?
        .human_get(
            &format!("/v1/workspaces/{}/{endpoint}", binding.workspace),
            &query,
            1_000_000,
        )
        .await?;
    if !contract::upstream_valid("NodePage", &raw) {
        return Err(unavailable());
    }
    let rows = raw["items"].as_array().ok_or_else(unavailable)?;
    if rows.len() > usize::from(limit) {
        return Err(unavailable());
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut items = Vec::new();
    for value in rows {
        let raw = value;
        let mut value = node(raw, binding.workspace, kind)?;
        if !seen.insert(value["id"].as_str().ok_or_else(unavailable)?.to_owned()) {
            return Err(unavailable());
        }
        if kind == "task" {
            crate::role_settings::show(&mut value, raw, &a.record.binding.default_role);
        }
        items.push(value)
    }
    let next = match raw["next_cursor"].as_str() {
        Some(c) => Some(r.cursors.encode(&binding, c, stamp(state))?),
        None => None,
    };
    let mut data = json!({"items":items,"next_cursor":next});
    if kind == "task" {
        let ids: Vec<Uuid> = items
            .iter()
            .map(|i| {
                i["id"]
                    .as_str()
                    .and_then(|v| v.parse().ok())
                    .ok_or_else(unavailable)
            })
            .collect::<Result<_>>()?;
        data["executions"] = json!(work::executions(&r.pool, project, &ids).await?);
    }
    finish(state, r, a, data).await
}
/// Internal sanitized search adapter; retains the same owner and binding fences.
pub(crate) async fn search_display(
    state: &AppState,
    r: &ReadRuntime,
    actor: &User,
    project: Uuid,
    q: &str,
    kind: &str,
) -> Result<Value> {
    if !matches!(kind, "task" | "knowledge") {
        return Err(invalid());
    }
    let Json(envelope) = list(
        state,
        r,
        actor.clone(),
        project,
        Page {
            limit: Some(32),
            cursor: None,
            q: Some(q.into()),
        },
        kind,
    )
    .await?;
    Ok(envelope.data)
}
async fn tasks(
    State(s): State<AppState>,
    Extension(r): Extension<Arc<ReadRuntime>>,
    RequireOwner(u): RequireOwner,
    Path(p): Path<Uuid>,
    q: std::result::Result<Query<Page>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Envelope>> {
    let Query(q) = q.map_err(|_| invalid())?;
    list(&s, &r, u, p, q, "task").await
}
async fn knowledge(
    State(s): State<AppState>,
    Extension(r): Extension<Arc<ReadRuntime>>,
    RequireOwner(u): RequireOwner,
    Path(p): Path<Uuid>,
    q: std::result::Result<Query<Page>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Envelope>> {
    let Query(q) = q.map_err(|_| invalid())?;
    list(&s, &r, u, p, q, "knowledge").await
}
async fn detail(
    s: &AppState,
    r: &ReadRuntime,
    u: User,
    p: Uuid,
    id: Uuid,
    kind: &str,
    revision: Option<u64>,
) -> Result<Json<Envelope>> {
    if id.is_nil() || revision.is_some_and(|n| n == 0 || n > i64::MAX as u64) {
        return Err(invalid());
    }
    let a = access(s, r, u, p).await?;
    let w = a.record.binding.graph_workspace_id;
    let suffix = revision.map_or_else(String::new, |n| format!("/revisions/{n}"));
    let raw = runtime(s)?
        .human_get(
            &format!("/v1/workspaces/{w}/nodes/{id}{suffix}"),
            &[],
            1_000_000,
        )
        .await?;
    let raw = if revision.is_some() {
        if !contract::upstream_valid("RevisionSnapshot", &raw) {
            return Err(unavailable());
        }
        raw["node"].clone()
    } else {
        raw
    };
    if raw["id"] != json!(id) || revision.is_some_and(|n| raw["content_revision"] != n) {
        return Err(unavailable());
    }
    let mut projected = node(&raw, w, kind)?;
    if kind == "task" {
        crate::role_settings::show(&mut projected, &raw, &a.record.binding.default_role);
    }
    finish(s, r, a, projected).await
}
async fn task(
    State(s): State<AppState>,
    Extension(r): Extension<Arc<ReadRuntime>>,
    RequireOwner(u): RequireOwner,
    Path((p, id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Envelope>> {
    detail(&s, &r, u, p, id, "task", None).await
}
async fn document(
    State(s): State<AppState>,
    Extension(r): Extension<Arc<ReadRuntime>>,
    RequireOwner(u): RequireOwner,
    Path((p, id)): Path<(Uuid, Uuid)>,
    q: std::result::Result<Query<Revision>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Envelope>> {
    let Query(q) = q.map_err(|_| invalid())?;
    detail(&s, &r, u, p, id, "knowledge", q.revision).await
}
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct GraphPage {
    limit: Option<u16>,
}
/// Bounded depth-1 Context read shared by the graph and Work projections.
async fn context(s: &AppState, w: Uuid, id: Uuid) -> Result<Value> {
    let raw = runtime(s)?
        .human_get(
            &format!("/v1/workspaces/{w}/nodes/{id}/context"),
            &[
                ("depth".into(), "1".into()),
                ("limit".into(), "63".into()),
                ("max_bytes".into(), "262144".into()),
                ("direction".into(), "both".into()),
            ],
            262144,
        )
        .await?;
    if !contract::upstream_valid("Context", &raw) || raw["node"]["id"] != json!(id) {
        return Err(unavailable());
    }
    for v in raw["nodes"].as_array().into_iter().flatten() {
        if v["workspace_id"] != json!(w) {
            return Err(unavailable());
        }
    }
    for e in raw["edges"].as_array().into_iter().flatten() {
        if e["workspace_id"] != json!(w) {
            return Err(unavailable());
        }
    }
    Ok(raw)
}
/// Parent of `id` from a present `parent-child` edge (source is the parent).
fn parent(raw: &Value, w: Uuid, id: Uuid) -> Result<Value> {
    let Some(edge) = raw["edges"].as_array().into_iter().flatten().find(|e| {
        e["edge_type"] == "parent-child" && e["deleted"] == false && e["target"] == json!(id)
    }) else {
        return Ok(Value::Null);
    };
    let pid = &edge["source"];
    let found = raw["nodes"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|n| &n["id"] == pid && n["deleted"] == false);
    Ok(match found {
        Some(n) => {
            let n = node(n, w, n["kind"].as_str().ok_or_else(unavailable)?)?;
            json!({"id":pid,"kind":n["kind"],"title":n["title"],"task_state":n["task_state"],"edge_id":edge["id"]})
        }
        None => json!({"id":pid,"kind":null,"title":null,"task_state":null,"edge_id":edge["id"]}),
    })
}
async fn graph(
    State(s): State<AppState>,
    Extension(r): Extension<Arc<ReadRuntime>>,
    RequireOwner(u): RequireOwner,
    Path((p, id)): Path<(Uuid, Uuid)>,
    q: std::result::Result<Query<GraphPage>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Envelope>> {
    let Query(q) = q.map_err(|_| invalid())?;
    let limit = q.limit.unwrap_or(63);
    if id.is_nil() || !(1..=63).contains(&limit) {
        return Err(invalid());
    }
    let a = access(&s, &r, u, p).await?;
    let w = a.record.binding.graph_workspace_id;
    let raw = context(&s, w, id).await?;
    let role =
        |p: &mut Value, n: &Value| crate::role_settings::show(p, n, &a.record.binding.default_role);
    let parent = parent(&raw, w, id)?;
    let data = aq_human_work::projection::neighborhood(&raw, id, parent, limit, |raw| {
        let mut value = node(raw, w, "task").ok()?;
        role(&mut value, raw);
        Some(value)
    })
    .ok_or_else(unavailable)?;
    finish(&s, &r, a, data).await
}
