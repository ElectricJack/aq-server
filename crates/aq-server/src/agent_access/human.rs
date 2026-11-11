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
use aq_human_work::{buckets, count};
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
/// Final current-role and binding fence, with one bounded project batch query.
async fn fence_records(r: &ReadRuntime, actor: &User, records: &[ProjectRecord]) -> Result<()> {
    if records.len() > 8 {
        return Err(invalid());
    }
    let mut tx = r.pool.begin().await.map_err(|_| unavailable())?;
    sqlx::query("SET LOCAL statement_timeout='1500ms'")
        .execute(&mut *tx)
        .await
        .map_err(|_| unavailable())?;
    sqlx::query("SET LOCAL lock_timeout='500ms'")
        .execute(&mut *tx)
        .await
        .map_err(|_| unavailable())?;
    let user = sqlx::query("SELECT role,email,issuer,subject FROM users WHERE id=$1 FOR SHARE")
        .bind(actor.id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(conflict)?;
    if user.get::<String, _>("role") != "owner"
        || user.get::<String, _>("email") != actor.email
        || user.get::<Option<String>, _>("issuer") != actor.issuer
        || user.get::<Option<String>, _>("subject") != actor.subject
    {
        return Err(conflict());
    }
    let ids: Vec<Uuid> = records
        .iter()
        .map(|record| record.binding.project_id)
        .collect();
    let rows=sqlx::query("SELECT project_id,revision,binding FROM project_execution_bindings WHERE project_id=ANY($1) ORDER BY project_id FOR SHARE").bind(&ids).fetch_all(&mut *tx).await.map_err(|_|unavailable())?;
    if rows.len() != records.len() {
        return Err(conflict());
    }
    for latest in rows {
        let expected = records
            .iter()
            .find(|record| record.binding.project_id == latest.get::<Uuid, _>("project_id"))
            .ok_or_else(conflict)?;
        if latest.get::<i64, _>("revision") as u64 != expected.revision
            || latest.get::<Value, _>("binding")
                != serde_json::to_value(&expected.binding).map_err(|_| unavailable())?
        {
            return Err(conflict());
        }
    }
    tx.commit().await.map_err(|_| unavailable())?;
    Ok(())
}
/// QA N-04: Graph attributes every AQ write to the one configured mutation
/// identity. Show the AQ account whose operation created (operation ID = node
/// ID) or last verified the record instead; other attributions are unchanged.
async fn attribute(state: &AppState, r: &ReadRuntime, data: &mut Value) -> Result<()> {
    fn records<'a>(v: &'a mut Value, out: &mut Vec<&'a mut serde_json::Map<String, Value>>) {
        match v {
            Value::Array(items) => items.iter_mut().for_each(|i| records(i, out)),
            Value::Object(o) => {
                if o.contains_key("created_by") && o.get("id").is_some_and(Value::is_string) {
                    out.push(o)
                } else {
                    o.values_mut().for_each(|i| records(i, out))
                }
            }
            _ => {}
        }
    }
    let Some(service) = state
        .agent_access
        .as_deref()
        .and_then(|rt| rt.mutation_actor.clone())
    else {
        return Ok(());
    };
    let mut found = Vec::new();
    records(data, &mut found);
    let by_service =
        |o: &serde_json::Map<String, Value>, k: &str| o.get(k) == Some(&json!(service));
    found.retain(|o| by_service(o, "created_by") || by_service(o, "last_verified_by"));
    let ids: Vec<Uuid> = found
        .iter()
        .filter_map(|o| o["id"].as_str()?.parse().ok())
        .collect();
    if ids.is_empty() {
        return Ok(());
    }
    let rows: Vec<(Uuid, Option<String>, Option<String>)> = sqlx::query_as("SELECT n.id,(SELECT u.email FROM users u WHERE u.id=COALESCE((SELECT d.owner_id FROM dashboard_operations d WHERE d.operation_id=n.id),(SELECT h.actor_id FROM human_graph_operations h WHERE h.operation_id=n.id))),(SELECT u.email FROM dashboard_operations d JOIN users u ON u.id=d.owner_id WHERE d.kind='knowledge.verify' AND d.disposition IN ('confirmed','unknown') AND d.response->'subject_refs'->0->>'id'=n.id::text ORDER BY d.updated_at DESC LIMIT 1) FROM unnest($1::uuid[]) AS n(id)")
        .bind(&ids).fetch_all(&r.pool).await.map_err(|_| unavailable())?;
    for o in found {
        let row = rows.iter().find(|row| o["id"] == json!(row.0));
        for (key, human) in [
            ("created_by", row.and_then(|r| r.1.as_ref())),
            ("last_verified_by", row.and_then(|r| r.2.as_ref())),
        ] {
            if let Some(email) = human.filter(|_| by_service(o, key)) {
                o.insert(key.into(), json!(email));
            }
        }
    }
    Ok(())
}
async fn finish(
    state: &AppState,
    r: &ReadRuntime,
    a: Access,
    mut data: Value,
) -> Result<Json<Envelope>> {
    attribute(state, r, &mut data).await?;
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
    async fn human_get(&self, path: &str, query: &[(String, String)], max: usize) -> Result<Value> {
        let mut response = self
            .client
            .get(self.url.join(path).map_err(|_| invalid())?)
            .bearer_auth(&self.token)
            .header("X-AQ-Graph-API-Version", "1")
            .query(query)
            .send()
            .await
            .map_err(|_| unavailable())?;
        if response
            .headers()
            .get("X-AQ-Graph-API-Version")
            .is_none_or(|h| h != "1")
        {
            return Err(unavailable());
        }
        match response.status().as_u16() {
            200 => {}
            404 => return Err(AppError::NotFound("Graph record not found".into())),
            _ => return Err(unavailable()),
        }
        if response.content_length().is_some_and(|n| n > max as u64) {
            return Err(unavailable());
        }
        let mut bytes = Vec::new();
        while let Some(c) = response.chunk().await.map_err(|_| unavailable())? {
            if bytes.len() + c.len() > max {
                return Err(unavailable());
            }
            bytes.extend_from_slice(&c)
        }
        // Same strict duplicate-key/depth parser as the existing scheduling reader.
        super::scheduling::parse::parse(&bytes).map_err(|_| unavailable())
    }
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
    let mut output = serde_json::Map::new();
    for key in [
        "id",
        "kind",
        "title",
        "body",
        "acceptance_criteria",
        "labels",
        "task_state",
        "verification",
        "content_type",
        "retired",
        "deleted",
        "created_at",
        "updated_at",
        "created_by",
        "provenance",
        "last_verified_by",
        "last_verified_at",
        "retirement_reason",
        "valid_from",
        "valid_until",
        "review_at",
        "successor_id",
        "summary",
    ] {
        output.insert(key.into(), raw[key].clone());
    }
    for key in ["version", "content_revision", "summary_of_revision"] {
        output.insert(
            key.into(),
            if raw[key].is_null() && key == "summary_of_revision" {
                Value::Null
            } else {
                json!(
                    raw[key]
                        .as_u64()
                        .filter(|n| *n > 0)
                        .ok_or_else(unavailable)?
                        .to_string()
                )
            },
        );
    }
    Ok(Value::Object(output))
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
        let value = node(value, binding.workspace, kind)?;
        if !seen.insert(value["id"].as_str().ok_or_else(unavailable)?.to_owned()) {
            return Err(unavailable());
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
    let projected = node(&raw, w, kind)?;
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
    let root = node(&raw["node"], w, "task")?;
    let upstream_truncated = raw["truncated"].as_bool().ok_or_else(unavailable)?;
    let mut truncated = upstream_truncated;
    let mut nodes = vec![root];
    let mut ids = std::collections::BTreeSet::from([id.to_string()]);
    let mut hidden = std::collections::BTreeSet::new();
    let mut hidden_tasks = buckets();
    let mut hidden_other = 0u64;
    for value in raw["nodes"].as_array().ok_or_else(unavailable)? {
        let vid = value["id"].as_str().ok_or_else(unavailable)?.to_owned();
        if ids.contains(&vid) || hidden.contains(&vid) {
            continue;
        }
        if value["kind"] != "task" || value["deleted"] != false {
            truncated = true;
            hidden.insert(vid);
            hidden_other += 1;
            continue;
        }
        if nodes.len() > usize::from(limit) {
            truncated = true;
            hidden.insert(vid);
            count(&mut hidden_tasks, &value["task_state"]);
            continue;
        }
        let n = node(value, w, "task")?;
        ids.insert(vid);
        nodes.push(n)
    }
    let mut edges = Vec::new();
    let mut hidden_edges = 0u64;
    for edge in raw["edges"].as_array().ok_or_else(unavailable)? {
        if edges.len() >= 256
            || edge["deleted"] != false
            || !ids.contains(edge["source"].as_str().ok_or_else(unavailable)?)
            || !ids.contains(edge["target"].as_str().ok_or_else(unavailable)?)
        {
            truncated = true;
            hidden_edges += 1;
            continue;
        }
        edges.push(aq_human_work::edge(edge).ok_or_else(unavailable)?);
    }
    let hidden_task_count: u64 = hidden_tasks.values().filter_map(Value::as_u64).sum();
    let shown = (nodes.len() - 1) as u64;
    let expansion = if hidden_task_count > 0 && u64::from(limit) < 63 {
        json!({"limit": (shown + hidden_task_count).min(63).to_string()})
    } else {
        Value::Null
    };
    let parent = parent(&raw, w, id)?;
    finish(
        &s,
        &r,
        a,
        json!({"root_id":id,"parent":parent,"nodes":nodes,"edges":edges,"truncated":truncated,
            "omitted":{"tasks":hidden_task_count.to_string(),
                "tasks_by_state":aq_human_work::decimal(hidden_tasks),
                "other_nodes":hidden_other.to_string(),"edges":hidden_edges.to_string(),
                "exact":!upstream_truncated},
            "expansion":expansion}),
    )
    .await
}
