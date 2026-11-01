//! Owner-authorized read projection. Graph service credentials never leave AQ.
mod cursor;
use super::{Runtime, contract};
use crate::{
    AppState,
    authz::RequireOwner,
    error::AppError,
    project_bindings::{self, domain::ProjectRecord},
    store::User,
};
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
use tokio::sync::Semaphore;
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
pub struct ReadRuntime {
    pool: PgPool,
    gate: Semaphore,
    cursors: cursor::Codec,
}
impl ReadRuntime {
    fn new(state: &AppState) -> Self {
        Self {
            pool: PgPoolOptions::new()
                .max_connections(2)
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
            gate: Semaphore::new(2),
            cursors: cursor::Codec::new(),
        }
    }
}
pub fn routes(state: &AppState) -> Router<AppState> {
    let runtime = Arc::new(ReadRuntime::new(state));
    Router::new()
        .route("/api/human/projects/{project}/tasks", get(tasks))
        .route("/api/human/projects/{project}/tasks/{node}", get(task))
        .route(
            "/api/human/projects/{project}/tasks/{node}/graph",
            get(graph),
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
    let Ok(_permit) = r.gate.try_acquire() else {
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
async fn access(state: &AppState, r: &ReadRuntime, actor: User, project: Uuid) -> Result<Access> {
    if project.is_nil() {
        return Err(invalid());
    }
    owner(r, &actor).await?;
    let mut view = state.clone();
    view.store.pool = r.pool.clone();
    let record = project_bindings::current(&view, project).await?;
    if record.binding.graph_workspace_id.is_nil() {
        return Err(unavailable());
    }
    Ok(Access {
        actor,
        record,
        requested: stamp(state),
    })
}
async fn finish(
    state: &AppState,
    r: &ReadRuntime,
    a: Access,
    data: Value,
) -> Result<Json<Envelope>> {
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
        .bind(a.actor.id)
        .fetch_optional(&mut *tx)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(conflict)?;
    if user.get::<String, _>("role") != "owner"
        || user.get::<String, _>("email") != a.actor.email
        || user.get::<Option<String>, _>("issuer") != a.actor.issuer
        || user.get::<Option<String>, _>("subject") != a.actor.subject
    {
        return Err(conflict());
    }
    let latest = sqlx::query(
        "SELECT revision,binding FROM project_execution_bindings WHERE project_id=$1 FOR SHARE",
    )
    .bind(a.record.binding.project_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|_| unavailable())?
    .ok_or_else(conflict)?;
    if latest.get::<i64, _>("revision") as u64 != a.record.revision
        || latest.get::<Value, _>("binding")
            != serde_json::to_value(&a.record.binding).map_err(|_| unavailable())?
    {
        return Err(conflict());
    }
    tx.commit().await.map_err(|_| unavailable())?;
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
    finish(state, r, a, json!({"items":items,"next_cursor":next})).await
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
async fn graph(
    State(s): State<AppState>,
    Extension(r): Extension<Arc<ReadRuntime>>,
    RequireOwner(u): RequireOwner,
    Path((p, id)): Path<(Uuid, Uuid)>,
) -> Result<Json<Envelope>> {
    if id.is_nil() {
        return Err(invalid());
    }
    let a = access(&s, &r, u, p).await?;
    let w = a.record.binding.graph_workspace_id;
    let raw = runtime(&s)?
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
    let root = node(&raw["node"], w, "task")?;
    let mut nodes = vec![root];
    let mut ids = std::collections::BTreeSet::from([id.to_string()]);
    let mut truncated = raw["truncated"].as_bool().ok_or_else(unavailable)?;
    for value in raw["nodes"].as_array().ok_or_else(unavailable)? {
        if value["workspace_id"] != json!(w) {
            return Err(unavailable());
        }
        if value["kind"] != "task" || value["deleted"] != false || nodes.len() >= 64 {
            truncated = true;
            continue;
        }
        let n = node(value, w, "task")?;
        if ids.insert(n["id"].as_str().ok_or_else(unavailable)?.into()) {
            nodes.push(n)
        }
    }
    let mut edges = Vec::new();
    for edge in raw["edges"].as_array().ok_or_else(unavailable)? {
        if edge["workspace_id"] != json!(w) {
            return Err(unavailable());
        }
        if edges.len() >= 256
            || edge["deleted"] != false
            || !ids.contains(edge["source"].as_str().ok_or_else(unavailable)?)
            || !ids.contains(edge["target"].as_str().ok_or_else(unavailable)?)
        {
            truncated = true;
            continue;
        }
        let mut e = serde_json::Map::new();
        for k in ["id", "source", "target", "edge_type", "description"] {
            e.insert(k.into(), edge[k].clone());
        }
        for k in ["version", "source_revision", "target_revision"] {
            e.insert(
                k.into(),
                if edge[k].is_null() && k != "version" {
                    Value::Null
                } else {
                    json!(
                        edge[k]
                            .as_u64()
                            .filter(|n| *n > 0)
                            .ok_or_else(unavailable)?
                            .to_string()
                    )
                },
            );
        }
        edges.push(Value::Object(e));
    }
    finish(
        &s,
        &r,
        a,
        json!({"root_id":id,"nodes":nodes,"edges":edges,"truncated":truncated}),
    )
    .await
}
