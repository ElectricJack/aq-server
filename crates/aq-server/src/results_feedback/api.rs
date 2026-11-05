//! Bounded, owner-authorized Results routes and G1 cursor bindings.
use super::{actions as adapters, contract::*, store};
use crate::{
    AppState,
    authz::RequireOwner,
    dashboard_api::{
        actions,
        contract::*,
        cursor::{Binding, CursorCodec},
        errors::ApiError,
    },
    store::User,
};
use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::StatusCode,
    routing::{get, post, put},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
};
use tokio::sync::Semaphore;
use uuid::Uuid;
type Result<T> = std::result::Result<T, ApiError>;
#[derive(Clone)]
struct Snapshot {
    expires: u64,
    pins: Vec<(Uuid, String)>,
}
struct Runtime {
    codec: CursorCodec,
    snapshots: Mutex<HashMap<Uuid, Snapshot>>,
    permits: Semaphore,
}
impl Runtime {
    fn new() -> Self {
        Self {
            permits: Semaphore::new(8),
            codec: CursorCodec::default(),
            snapshots: Mutex::new(HashMap::new()),
        }
    }
}
pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/v1/results", get(feed))
        .route("/api/v1/results/{id}/revisions/{revision}", get(detail))
        .route(
            "/api/v1/results/{id}/revisions/{revision}/comments",
            get(comments).post(comment),
        )
        .route("/api/v1/results/{id}/follow", put(follow))
        .route("/api/v1/comments/{id}/revisions", post(correction))
        .route("/api/v1/threads/{id}/resolve", post(resolve))
        .route("/api/v1/feedback-tasks", post(draft))
        .route("/api/v1/feedback-links", post(link))
        .route("/api/v1/feedback-tasks/{id}", get(draft_status))
        .route("/api/v1/feedback-tasks/{id}/recover", post(recover))
        .route("/api/v1/results/{id}/evidence-waivers", post(waiver))
        .layer(DefaultBodyLimit::max(65536))
        .layer(Extension(Arc::new(Runtime::new())))
}
fn envelope<T>(state: &AppState, scope: Scope, data: T) -> Envelope<T> {
    let now = Timestamp((state.clock)().to_rfc3339_opts(chrono::SecondsFormat::Millis, true));
    Envelope {
        api_version: ApiVersion::Read1,
        request_id: Uuid::new_v4(),
        server_time: now.clone(),
        scope,
        data,
        observation: Observation {
            observed_at: now.clone(),
            fresh_until: now,
            source: "retained_result_feedback".into(),
            quality: Quality::Reported,
            coverage: Completeness::Complete,
        },
        pending_operations: vec![],
        coverage: Coverage {
            state: Completeness::Complete,
            reason: None,
        },
    }
}
#[derive(Deserialize, Serialize, Default)]
#[serde(deny_unknown_fields)]
struct Page {
    mode: Option<String>,
    scope: Option<String>,
    project_id: Option<Uuid>,
    task_id: Option<Uuid>,
    media: Option<String>,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
    limit: Option<u16>,
    #[serde(skip_serializing)]
    cursor: Option<String>,
}
fn limit(page: &Page) -> Result<u16> {
    let n = page.limit.unwrap_or(20);
    if n == 0 || n > 100 {
        return Err(store::invalid());
    }
    Ok(n)
}
fn binding(user: &User, p: &Page, resource: String) -> Result<Binding> {
    // Full current identity partitions cursors, not only the stable user UUID.
    use sha2::Digest;
    let partition = hex::encode(sha2::Sha256::digest(
        serde_json::to_vec(&json!([user.role, user.email, user.issuer, user.subject]))
            .map_err(|_| store::invalid())?,
    ));
    Ok(Binding {
        principal: user.id,
        authorization_partition: partition,
        scope: json!({"scope":p.scope,"project_id":p.project_id}),
        filters: json!(p),
        sort: "submitted_desc_id_desc".into(),
        page_size: limit(p)?,
        resource,
    })
}
fn cursor_error(_: crate::dashboard_api::cursor::CursorError) -> ApiError {
    ApiError::new(
        StatusCode::CONFLICT,
        "cursor_restart_required",
        "Restart this Results query",
    )
}
async fn feed(
    State(state): State<AppState>,
    RequireOwner(user): RequireOwner,
    Extension(runtime): Extension<Arc<Runtime>>,
    query: std::result::Result<Query<Page>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Envelope<Value>>> {
    let Query(page) = query.map_err(|_| store::invalid())?;
    let _permit = runtime
        .permits
        .try_acquire()
        .map_err(|_| ApiError::for_status(StatusCode::TOO_MANY_REQUESTS))?;
    store::principal(&state, &user).await?;
    let mode = page.mode.as_deref().unwrap_or("latest");
    if !matches!(mode, "latest" | "needs_review" | "following")
        || !matches!(
            page.scope.as_deref(),
            None | Some("all_authorized") | Some("project")
        )
        || page.scope.as_deref() == Some("project") && page.project_id.is_none()
        || !matches!(
            page.media.as_deref(),
            None | Some("any") | Some("image") | Some("video") | Some("none")
        )
        || page.since_ms.is_some_and(|n| n < 0)
        || page.until_ms.is_some_and(|n| n < 0)
        || matches!((page.since_ms,page.until_ms),(Some(a),Some(b)) if a>b)
    {
        return Err(store::invalid());
    }
    let bind = binding(&user, &page, "results".into())?;
    let now = (state.clock)().timestamp_millis() as u64;
    let (snapshot, id, start) = if let Some(token) = &page.cursor {
        let pos = runtime
            .codec
            .decode(token, &bind, now)
            .map_err(cursor_error)?;
        let id = pos.snapshot.parse::<Uuid>().map_err(|_| store::invalid())?;
        let snapshots = runtime
            .snapshots
            .lock()
            .map_err(|_| ApiError::unavailable())?;
        let snapshot = snapshots
            .get(&id)
            .filter(|s| s.expires > now)
            .cloned()
            .ok_or_else(|| {
                cursor_error(crate::dashboard_api::cursor::CursorError::RestartRequired)
            })?;
        (
            snapshot,
            id,
            pos.position
                .parse::<usize>()
                .map_err(|_| store::invalid())?,
        )
    } else {
        let pins = store::feed(
            &state,
            &user,
            mode,
            page.project_id,
            page.task_id,
            page.media.as_deref().unwrap_or("any"),
            page.since_ms,
            page.until_ms,
        )
        .await?;
        (
            Snapshot {
                expires: now + 300000,
                pins,
            },
            Uuid::new_v4(),
            0,
        )
    };
    if start > snapshot.pins.len() {
        return Err(store::invalid());
    }
    let end = (start + usize::from(bind.page_size)).min(snapshot.pins.len());
    let mut items = Vec::new();
    for (id, rev) in &snapshot.pins[start..end] {
        let p = store::pin(&state, *id, Some(rev)).await?;
        items.push(store::detail(&state, &user, &p).await?);
    }
    let next = if end < snapshot.pins.len() {
        let token = runtime
            .codec
            .encode(
                &bind,
                &id.to_string(),
                &end.to_string(),
                now,
                snapshot.expires,
            )
            .map_err(cursor_error)?;
        let mut snapshots = runtime
            .snapshots
            .lock()
            .map_err(|_| ApiError::unavailable())?;
        snapshots.retain(|_, s| s.expires > now);
        if !snapshots.contains_key(&id) && snapshots.len() >= 128 {
            return Err(ApiError::for_status(StatusCode::TOO_MANY_REQUESTS));
        }
        snapshots.insert(id, snapshot);
        Some(token)
    } else {
        None
    };
    store::principal(&state, &user).await?;
    let scope = page
        .project_id
        .map(|project_id| Scope::Project { project_id })
        .unwrap_or(Scope::AllAuthorized);
    Ok(Json(envelope(
        &state,
        scope,
        json!({"items":items,"next_cursor":next}),
    )))
}
async fn detail(
    State(state): State<AppState>,
    RequireOwner(user): RequireOwner,
    path: std::result::Result<Path<(Uuid, String)>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Envelope<Detail>>> {
    let Path((id, revision)) = path.map_err(|_| store::invalid())?;
    let p = store::pin(&state, id, Some(&revision)).await?;
    let data = store::detail(&state, &user, &p).await?;
    store::principal(&state, &user).await?;
    Ok(Json(envelope(
        &state,
        Scope::Project {
            project_id: p.project,
        },
        data,
    )))
}
async fn comments(
    State(state): State<AppState>,
    RequireOwner(user): RequireOwner,
    Extension(runtime): Extension<Arc<Runtime>>,
    path: std::result::Result<Path<(Uuid, String)>, axum::extract::rejection::PathRejection>,
    query: std::result::Result<Query<Page>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Envelope<Value>>> {
    let Path((id, rev)) = path.map_err(|_| store::invalid())?;
    let Query(page) = query.map_err(|_| store::invalid())?;
    if page.mode.is_some()
        || page.scope.is_some()
        || page.project_id.is_some()
        || page.task_id.is_some()
        || page.media.is_some()
        || page.since_ms.is_some()
        || page.until_ms.is_some()
    {
        return Err(store::invalid());
    }
    let _permit = runtime
        .permits
        .try_acquire()
        .map_err(|_| ApiError::for_status(StatusCode::TOO_MANY_REQUESTS))?;
    let p = store::pin(&state, id, Some(&rev)).await?;
    store::detail(&state, &user, &p).await?;
    let bind = binding(
        &user,
        &page,
        format!("comments:{}:{}", p.report, p.revision),
    )?;
    let now = (state.clock)().timestamp_millis() as u64;
    let (snapshot, start, issued, expires) = if let Some(token) = &page.cursor {
        let pos = runtime
            .codec
            .decode(token, &bind, now)
            .map_err(cursor_error)?;
        (
            pos.snapshot.parse::<i64>().map_err(|_| store::invalid())?,
            pos.position.parse::<i64>().map_err(|_| store::invalid())?,
            pos.issued,
            pos.expires,
        )
    } else {
        let upper: i64 =
            sqlx::query_scalar("SELECT COALESCE(max(position),0) FROM result_comments")
                .fetch_one(&state.store.pool)
                .await?;
        (upper, 0, now, now + 300000)
    };
    if start < 0 || snapshot < start {
        return Err(store::invalid());
    }
    let mut items = store::comments(
        &state,
        &user,
        &p,
        start,
        snapshot,
        i64::from(bind.page_size) + 1,
    )
    .await?;
    let next = if items.len() > usize::from(bind.page_size) {
        items.pop();
        let position = items
            .last()
            .and_then(|v| v["position"].as_str())
            .ok_or_else(store::invalid)?;
        Some(
            runtime
                .codec
                .encode(&bind, &snapshot.to_string(), position, issued, expires)
                .map_err(cursor_error)?,
        )
    } else {
        None
    };
    Ok(Json(envelope(
        &state,
        Scope::Project {
            project_id: p.project,
        },
        json!({"feedback_read_version":FeedbackReadVersion::V1_1,"follow_up_submission":FollowUpSubmission::Unsupported,"items":items,"next_cursor":next}),
    )))
}
type Body =
    std::result::Result<Json<actions::json::UniqueJson>, axum::extract::rejection::JsonRejection>;
fn parse<T: serde::de::DeserializeOwned>(body: Body) -> Result<T> {
    let Json(actions::json::UniqueJson(raw)) =
        body.map_err(|e| ApiError::for_status(e.status()))?;
    serde_json::from_value(raw).map_err(|_| store::invalid())
}
async fn submit<P: adapters::Payload + Serialize>(
    state: &AppState,
    user: &User,
    id: Uuid,
    p: P,
) -> Result<(StatusCode, Json<actions::OperationResponse>)> {
    let registry = actions::existing::registry(state, None, None);
    let op = actions::submit_legacy(state, &registry, user, adapters::request(id, p)).await?;
    Ok((
        actions::operation_status(&op),
        Json(actions::response(state, op)),
    ))
}
macro_rules! mutation {
    ($name:ident,$p:ty) => {
        async fn $name(
            State(state): State<AppState>,
            RequireOwner(user): RequireOwner,
            path: std::result::Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
            body: Body,
        ) -> Result<(StatusCode, Json<actions::OperationResponse>)> {
            let Path(id) = path.map_err(|_| store::invalid())?;
            submit(&state, &user, id, parse::<$p>(body)?).await
        }
    };
}
mutation!(follow, Follow);
mutation!(correction, Correction);
mutation!(resolve, Resolve);
mutation!(waiver, Waiver);
async fn comment(
    State(state): State<AppState>,
    RequireOwner(user): RequireOwner,
    path: std::result::Result<Path<(Uuid, String)>, axum::extract::rejection::PathRejection>,
    body: Body,
) -> Result<(StatusCode, Json<actions::OperationResponse>)> {
    let Path((id, rev)) = path.map_err(|_| store::invalid())?;
    let Json(actions::json::UniqueJson(mut raw)) =
        body.map_err(|e| ApiError::for_status(e.status()))?;
    let object = raw.as_object_mut().ok_or_else(store::invalid)?;
    if object.contains_key("result_id") || object.contains_key("revision") {
        return Err(store::invalid());
    }
    object.insert("result_id".into(), json!(id));
    object.insert("revision".into(), json!(rev));
    let p: Comment = serde_json::from_value(raw).map_err(|_| store::invalid())?;
    submit(&state, &user, id, p).await
}
async fn draft(
    State(state): State<AppState>,
    RequireOwner(user): RequireOwner,
    body: Body,
) -> Result<(StatusCode, Json<actions::OperationResponse>)> {
    let p: FeedbackDraft = parse(body)?;
    submit(&state, &user, p.result_id, p).await
}
async fn link(
    State(state): State<AppState>,
    RequireOwner(user): RequireOwner,
    body: Body,
) -> Result<(StatusCode, Json<actions::OperationResponse>)> {
    let p: FeedbackLink = parse(body)?;
    submit(&state, &user, p.result_id, p).await
}
// The candidate is deterministic even while upstream confirmation or local linking
// is uncertain. This endpoint reports identity without claiming task existence.
async fn draft_status(
    State(state): State<AppState>,
    RequireOwner(user): RequireOwner,
    path: std::result::Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
) -> Result<Json<Envelope<Value>>> {
    let Path(id) = path.map_err(|_| store::invalid())?;
    let (project_id, status) = adapters::draft_status(&state, &user, id).await?;
    Ok(Json(envelope(
        &state,
        Scope::Project { project_id },
        status,
    )))
}

async fn recover(
    State(state): State<AppState>,
    RequireOwner(user): RequireOwner,
    path: std::result::Result<Path<Uuid>, axum::extract::rejection::PathRejection>,
    body: Body,
) -> Result<(StatusCode, Json<actions::OperationResponse>)> {
    let Path(id) = path.map_err(|_| store::invalid())?;
    let value: Value = parse(body)?;
    if value != json!({}) {
        return Err(store::invalid());
    }
    let op = adapters::recover(&state, &user, id).await?;
    Ok((
        actions::operation_status(&op),
        Json(actions::response(&state, op)),
    ))
}
