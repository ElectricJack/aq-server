//! Explicitly instrumented disposable fixture. Not compiled into release builds.
//! Runtime controls are also unavailable under production authentication.
use crate::{AppState, authz::RequireOwner, config::AuthConfig};
use aq_worker_protocol::MAX_GRANT_MS;
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use tokio::sync::watch;

pub struct Controls {
    offset: AtomicU64,
    ttl: AtomicU64,
    bulk_blocked: watch::Sender<bool>,
    bulk_active: AtomicUsize,
}
impl Default for Controls {
    fn default() -> Self {
        Self {
            offset: AtomicU64::new(0),
            ttl: AtomicU64::new(MAX_GRANT_MS),
            bulk_blocked: watch::channel(false).0,
            bulk_active: AtomicUsize::new(0),
        }
    }
}
impl Controls {
    pub fn offset_ms(&self) -> u64 {
        self.offset.load(Ordering::SeqCst)
    }
    pub fn grant_ttl_ms(&self) -> u64 {
        self.ttl.load(Ordering::SeqCst)
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Change {
    advance_ms: Option<u64>,
    set_offset_ms: Option<u64>,
    grant_ttl_ms: Option<u64>,
    bulk_blocked: Option<bool>,
}
#[derive(Serialize)]
struct Snapshot {
    offset_ms: u64,
    now_ms: u64,
    grant_ttl_ms: u64,
    bulk_active: usize,
    bulk_limit: usize,
    control_limit: usize,
    general_limit: usize,
    bulk_blocked: bool,
}
fn require_dev(state: &AppState) -> Result<(), StatusCode> {
    if matches!(state.config.auth, AuthConfig::Dev { .. }) {
        Ok(())
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}
fn snapshot(state: &AppState) -> Snapshot {
    let controls = &state.store.recovery_test;
    Snapshot {
        offset_ms: controls.offset_ms(),
        now_ms: super::api::now(state),
        grant_ttl_ms: controls.grant_ttl_ms(),
        bulk_active: controls.bulk_active.load(Ordering::SeqCst),
        bulk_limit: crate::admission::BULK_LIMIT,
        control_limit: crate::admission::CONTROL_LIMIT,
        general_limit: crate::admission::GENERAL_LIMIT,
        bulk_blocked: *controls.bulk_blocked.borrow(),
    }
}
pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/api/execution/test-bulk", post(bulk))
        .with_state(state.clone())
        .merge(
            Router::new()
                .route("/api/execution/test-control", get(inspect).post(change))
                .with_state(state.control_view()),
        )
}
async fn inspect(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
) -> Result<Json<Snapshot>, StatusCode> {
    require_dev(&state)?;
    Ok(Json(snapshot(&state)))
}
async fn change(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
    Json(change): Json<Change>,
) -> Result<Json<Snapshot>, StatusCode> {
    require_dev(&state)?;
    let controls = &state.store.recovery_test;
    if change.advance_ms.is_some() && change.set_offset_ms.is_some()
        || change
            .grant_ttl_ms
            .is_some_and(|n| !(10..=MAX_GRANT_MS).contains(&n))
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    if let Some(offset) = change.set_offset_ms {
        if offset > 86_400_000 {
            return Err(StatusCode::BAD_REQUEST);
        }
        controls.offset.store(offset, Ordering::SeqCst);
    }
    if let Some(delta) = change.advance_ms {
        controls
            .offset
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                n.checked_add(delta).filter(|v| *v <= 86_400_000)
            })
            .map_err(|_| StatusCode::BAD_REQUEST)?;
    }
    if let Some(ttl) = change.grant_ttl_ms {
        controls.ttl.store(ttl, Ordering::SeqCst);
    }
    if let Some(blocked) = change.bulk_blocked {
        controls.bulk_blocked.send_replace(blocked);
    }
    Ok(Json(snapshot(&state)))
}
struct Active<'a>(&'a AtomicUsize);
impl Drop for Active<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}
async fn bulk(
    State(state): State<AppState>,
    RequireOwner(_): RequireOwner,
) -> Result<Json<serde_json::Value>, StatusCode> {
    require_dev(&state)?;
    // Deliberately hold a general database connection as well as the HTTP
    // permit. This measures real resource isolation, not merely route timing.
    let _connection = state
        .store
        .pool
        .acquire()
        .await
        .map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    let controls = &state.store.recovery_test;
    let mut blocked = controls.bulk_blocked.subscribe();
    controls.bulk_active.fetch_add(1, Ordering::SeqCst);
    let _active = Active(&controls.bulk_active);
    while *blocked.borrow_and_update() {
        if blocked.changed().await.is_err() {
            break;
        }
    }
    Ok(Json(
        serde_json::json!({"fixture":"synthetic_bulk_admission", "released":true}),
    ))
}
