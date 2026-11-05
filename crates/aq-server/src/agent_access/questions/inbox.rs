//! F5 human read: the owner lists and inspects agent question decisions
//! (`question-inbox-1`). Open lists pending questions oldest asked first;
//! closed lists answered ones most recently answered first. Cursors are the
//! shared opaque dashboard format, bound to the owner's current identity,
//! state, project and page size. Graph titles are best-effort and bounded:
//! an unavailable graph yields a null title, never a failed read.
use crate::{
    AppState,
    agent_access::human::{self as graph, ReadRuntime},
    authz::RequireOwner,
    dashboard_api::{
        contract::{
            QuestionDelivery, QuestionInboxDetail, QuestionInboxPage, QuestionInboxVersion,
            QuestionItem, QuestionKind, QuestionState,
        },
        cursor::{Binding, CursorCodec, CursorError},
        errors::ApiError,
    },
    store::User,
};
use axum::{
    Extension, Json, Router,
    extract::{Path, Query, State, rejection::QueryRejection},
    http::StatusCode,
    response::IntoResponse,
    routing::get,
};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::Row;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use uuid::Uuid;

type Result<T> = std::result::Result<T, ApiError>;
pub const LIST_ROUTE: &str = "/api/questions/decisions";
pub const DETAIL_ROUTE: &str = "/api/questions/decisions/{id}";
const DEFAULT_LIMIT: u16 = 25;
const MAX_LIMIT: u16 = 50;
const CURSOR_TTL_MS: u64 = 300_000;
const BRIEF_TIMEOUT: Duration = Duration::from_secs(1);
const COLUMNS: &str = "decision_id,decision_revision,state,project_id,workspace_id,task_id,question,created_at_ms,expires_at_ms,answer,answered_at_ms,delivery";

struct Runtime {
    codec: CursorCodec,
    permits: Semaphore,
}

pub fn admin_routes() -> Router<AppState> {
    Router::new()
        .route(LIST_ROUTE, get(list_http))
        .route(DETAIL_ROUTE, get(detail_http))
        .layer(Extension(Arc::new(Runtime {
            codec: CursorCodec::default(),
            permits: Semaphore::new(4),
        })))
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum InboxState {
    #[default]
    Open,
    Closed,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListQuery {
    #[serde(default)]
    state: InboxState,
    limit: Option<u16>,
    project_id: Option<Uuid>,
    cursor: Option<String>,
}

fn now(state: &AppState) -> Result<u64> {
    u64::try_from((state.clock)().timestamp_millis()).map_err(|_| ApiError::unavailable())
}
fn restart() -> ApiError {
    ApiError::new(
        StatusCode::GONE,
        "cursor_restart_required",
        "Restart this list; preserve your reading anchor",
    )
}
fn cursor_error(error: CursorError) -> ApiError {
    match error {
        CursorError::RestartRequired => restart(),
        CursorError::AuthorizationChanged => ApiError::new(
            StatusCode::FORBIDDEN,
            "authorization_changed",
            "Your access changed; reconnect to AQ",
        ),
        CursorError::Unavailable => ApiError::unavailable(),
    }
}
/// The owner's full current identity partitions cursors, not only the user ID.
fn binding(user: &User, query: &ListQuery, limit: u16) -> Result<Binding> {
    use sha2::Digest;
    let partition = hex::encode(sha2::Sha256::digest(
        serde_json::to_vec(&json!([user.role, user.email, user.issuer, user.subject]))
            .map_err(|_| ApiError::unavailable())?,
    ));
    let (state, sort) = match query.state {
        InboxState::Open => ("open", "asked_asc_id_asc"),
        InboxState::Closed => ("closed", "answered_desc_id_desc"),
    };
    Ok(Binding {
        principal: user.id,
        authorization_partition: partition,
        scope: json!({"project_id": query.project_id}),
        filters: json!({"state": state}),
        sort: sort.into(),
        page_size: limit,
        resource: "questions".into(),
    })
}
fn position(value: &str) -> Result<(i64, Uuid)> {
    let (at, id) = value.split_once(':').ok_or_else(restart)?;
    Ok((
        at.parse().map_err(|_| restart())?,
        id.parse().map_err(|_| restart())?,
    ))
}

/// Per-request graph title cache: one bounded read per distinct task; the
/// first timeout stops further reads for this request.
struct Titles {
    state: AppState,
    actor: User,
    runtime: Option<ReadRuntime>,
    cache: HashMap<Uuid, Option<String>>,
    down: bool,
}
impl Titles {
    fn new(state: &AppState, actor: &User) -> Self {
        Self {
            state: state.clone(),
            actor: actor.clone(),
            runtime: state
                .agent_access
                .is_some()
                .then(|| ReadRuntime::with_pool(state.store.pool.clone())),
            cache: HashMap::new(),
            down: false,
        }
    }
    async fn get(&mut self, project: Uuid, task: Uuid) -> Option<String> {
        let runtime = self.runtime.as_ref()?;
        if let Some(hit) = self.cache.get(&task) {
            return hit.clone();
        }
        if self.down {
            return None;
        }
        let title = match tokio::time::timeout(
            BRIEF_TIMEOUT,
            graph::task_brief(&self.state, runtime, &self.actor, project, task),
        )
        .await
        {
            Ok(Ok(brief)) => brief.title.filter(|t| t.len() <= 4096),
            Ok(Err(_)) => None,
            Err(_) => {
                self.down = true;
                None
            }
        };
        self.cache.insert(task, title.clone());
        title
    }
}

fn item(row: &sqlx::postgres::PgRow, task_title: Option<String>) -> Result<QuestionItem> {
    let ms = |v: i64| u64::try_from(v).map_err(|_| ApiError::unavailable());
    let state = match row.get::<String, _>("state").as_str() {
        "pending" => QuestionState::Pending,
        "answered" => QuestionState::Answered,
        _ => return Err(ApiError::unavailable()),
    };
    let delivery = row
        .get::<Option<Value>, _>("delivery")
        .map(serde_json::from_value::<QuestionDelivery>)
        .transpose()
        .map_err(|_| ApiError::unavailable())?;
    let answered_at_ms = row
        .get::<Option<i64>, _>("answered_at_ms")
        .map(ms)
        .transpose()?;
    Ok(QuestionItem {
        kind: QuestionKind::Question,
        decision_id: row.get("decision_id"),
        decision_revision: row.get::<i64, _>("decision_revision").to_string(),
        state,
        project_id: row.get("project_id"),
        workspace_id: row.get("workspace_id"),
        task_id: row.get("task_id"),
        task_title,
        question: row.get("question"),
        asked_at_ms: ms(row.get("created_at_ms"))?,
        expires_at_ms: ms(row.get("expires_at_ms"))?,
        answer: row.get("answer"),
        answered_at_ms,
        delivery,
    })
}

async fn list(
    state: &AppState,
    runtime: &Runtime,
    user: &User,
    query: &ListQuery,
) -> Result<QuestionInboxPage> {
    let limit = query.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) || query.project_id.is_some_and(|p| p.is_nil()) {
        return Err(ApiError::invalid());
    }
    let bind = binding(user, query, limit)?;
    let at = now(state)?;
    let (after, expires) = match &query.cursor {
        Some(token) => {
            let pos = runtime
                .codec
                .decode(token, &bind, at)
                .map_err(cursor_error)?;
            (Some(position(&pos.position)?), pos.expires)
        }
        None => (None, at + CURSOR_TTL_MS),
    };
    let (filter, order, key) = match query.state {
        InboxState::Open => (
            "state='pending' AND ($2::bigint IS NULL OR (created_at_ms,decision_id)>($2,$3))",
            "created_at_ms,decision_id",
            "created_at_ms",
        ),
        InboxState::Closed => (
            "state='answered' AND ($2::bigint IS NULL OR (answered_at_ms,decision_id)<($2,$3))",
            "answered_at_ms DESC,decision_id DESC",
            "answered_at_ms",
        ),
    };
    let sql = format!(
        "SELECT {COLUMNS} FROM question_decisions WHERE ($1::uuid IS NULL OR project_id=$1) AND {filter} ORDER BY {order} LIMIT $4"
    );
    let rows = sqlx::query(&sql)
        .bind(query.project_id)
        .bind(after.map(|(at, _)| at))
        .bind(after.map(|(_, id)| id))
        .bind(i64::from(limit) + 1)
        .fetch_all(&state.store.pool)
        .await
        .map_err(|_| ApiError::unavailable())?;
    let more = rows.len() > usize::from(limit);
    let page = &rows[..rows.len().min(usize::from(limit))];
    let mut titles = Titles::new(state, user);
    let mut items = Vec::with_capacity(page.len());
    for row in page {
        let title = titles.get(row.get("project_id"), row.get("task_id")).await;
        items.push(item(row, title)?);
    }
    let next_cursor = match page.last() {
        Some(last) if more => Some(
            runtime
                .codec
                .encode(
                    &bind,
                    "keyset",
                    &format!(
                        "{}:{}",
                        last.get::<i64, _>(key),
                        last.get::<Uuid, _>("decision_id")
                    ),
                    at,
                    expires,
                )
                .map_err(cursor_error)?,
        ),
        _ => None,
    };
    Ok(QuestionInboxPage {
        api_version: QuestionInboxVersion::V1,
        server_time_ms: now(state)?,
        items,
        next_cursor,
    })
}

async fn detail(state: &AppState, user: &User, id: Uuid) -> Result<QuestionInboxDetail> {
    let row = sqlx::query(&format!(
        "SELECT {COLUMNS} FROM question_decisions WHERE decision_id=$1"
    ))
    .bind(id)
    .fetch_optional(&state.store.pool)
    .await
    .map_err(|_| ApiError::unavailable())?
    .ok_or_else(|| ApiError::for_status(StatusCode::NOT_FOUND))?;
    let title = Titles::new(state, user)
        .get(row.get("project_id"), row.get("task_id"))
        .await;
    Ok(QuestionInboxDetail {
        api_version: QuestionInboxVersion::V1,
        server_time_ms: now(state)?,
        item: item(&row, title)?,
    })
}

fn bounded<T: serde::Serialize>(value: T) -> Result<impl IntoResponse> {
    let bytes = serde_json::to_vec(&value).map_err(|_| ApiError::unavailable())?;
    if bytes.len() > 1_048_576 {
        return Err(ApiError::unavailable());
    }
    Ok(([("cache-control", "no-store")], Json(value)))
}
async fn list_http(
    State(state): State<AppState>,
    RequireOwner(user): RequireOwner,
    Extension(runtime): Extension<Arc<Runtime>>,
    query: std::result::Result<Query<ListQuery>, QueryRejection>,
) -> Result<impl IntoResponse> {
    let Query(query) = query.map_err(|_| ApiError::invalid())?;
    let _permit = runtime
        .permits
        .try_acquire()
        .map_err(|_| ApiError::for_status(StatusCode::TOO_MANY_REQUESTS))?;
    let page = tokio::time::timeout(
        Duration::from_secs(5),
        list(&state, &runtime, &user, &query),
    )
    .await
    .map_err(|_| ApiError::unavailable())??;
    bounded(page)
}
async fn detail_http(
    State(state): State<AppState>,
    RequireOwner(user): RequireOwner,
    Extension(runtime): Extension<Arc<Runtime>>,
    id: std::result::Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<impl IntoResponse> {
    let Path(id) = id.map_err(|_| ApiError::invalid())?;
    let _permit = runtime
        .permits
        .try_acquire()
        .map_err(|_| ApiError::for_status(StatusCode::TOO_MANY_REQUESTS))?;
    let read = tokio::time::timeout(Duration::from_secs(3), detail(&state, &user, id))
        .await
        .map_err(|_| ApiError::unavailable())??;
    bounded(read)
}
