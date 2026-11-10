//! Human global-conversation admission over AQ-11 mail, never runtime delivery.
mod attachments;
pub mod dispatch;
mod marker;
mod reads;
pub mod reply;
pub mod reservations;
mod writes;
use super::{Envelope as Mail, append, digest, receipt, replay, sender_key};
use crate::{AppState, authz::RequireOwner, error::AppError, store::User};
use axum::{
    Extension, Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Digest as _;
use sqlx::{PgPool, Postgres, Row, Transaction, postgres::PgPoolOptions};
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;
use uuid::Uuid;
pub(crate) use writes::action::SendMessage;
type Result<T> = std::result::Result<T, AppError>;
const CONTRACT: &str = "human-supervisor-conversation-1";
pub(super) const MAX_OPERATIONS: i64 = 4096;
pub(super) const MAX_BYTES: i64 = 64 * 1024 * 1024;
const PAGE_BYTES: usize = 262144;
fn invalid() -> AppError {
    AppError::BadRequest("invalid bounded supervisor conversation request".into())
}
fn unavailable() -> AppError {
    AppError::IdentityUnavailable("supervisor conversation unavailable".into())
}
fn conflict() -> AppError {
    AppError::Conflict("operation identity conflicts; inspect original operation".into())
}
fn missing() -> AppError {
    AppError::NotFound("conversation or operation not found".into())
}
fn db(_: sqlx::Error) -> AppError {
    unavailable()
}
fn hash(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(sha2::Sha256::digest(bytes)))
}
fn positive(s: &str) -> Result<i64> {
    if s.starts_with('0') || !s.bytes().all(|b| b.is_ascii_digit()) {
        return Err(invalid());
    }
    s.parse::<i64>().ok().filter(|n| *n > 0).ok_or_else(invalid)
}
fn cursor(s: &str) -> Result<i64> {
    if s == "0" { Ok(0) } else { positive(s) }
}
fn stamp(state: &AppState) -> Result<u64> {
    u64::try_from((state.clock)().timestamp_millis())
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(unavailable)
}
use aq_supervisor_control::wire::{Conversation, Runtime, author, recipient};
fn runtime() -> Runtime {
    Runtime::unavailable()
}
/// X4: owner reads report the active central session bound to this
/// conversation. Admission receipts keep the admission-time [`runtime`].
async fn live_runtime(pool: &PgPool, conversation: Uuid) -> Runtime {
    let binding = dispatch::binding(pool, Some(conversation))
        .await
        .map(|b| json!(b));
    let ready = binding.is_none() && aq_supervisor_control::connected(pool, conversation).await;
    Runtime::live(binding, ready)
}
pub(crate) struct Service {
    pool: PgPool,
    gate: Semaphore,
}
impl Service {
    pub(crate) fn new(state: &AppState) -> Self {
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
                        .application_name("aq-supervisor-conversation"),
                ),
            gate: Semaphore::new(2),
        }
    }
    async fn tx(&self) -> Result<Transaction<'_, Postgres>> {
        let mut tx = self.pool.begin().await.map_err(db)?;
        sqlx::query("SELECT set_config('statement_timeout','1000ms',true),set_config('lock_timeout','500ms',true)").execute(&mut *tx).await.map_err(db)?;
        Ok(tx)
    }
}
pub fn routes(state: &AppState) -> Router<AppState> {
    let service = Arc::new(Service::new(state));
    Router::new()
        .route("/api/human/conversations/global", get(reads::discovery))
        .route(
            "/api/human/conversations/projects/{project}",
            get(reads::project_discovery),
        )
        .route(
            "/api/human/conversations/{conversation}/messages",
            get(reads::history).post(writes::enqueue),
        )
        .route(
            "/api/human/conversations/{conversation}/operations/{operation}",
            get(writes::lookup),
        )
        .route(
            "/api/human/conversations/{conversation}/read-marker",
            get(marker::read).put(marker::write),
        )
        .route(
            "/api/human/conversations/{conversation}/attachments/{attachment}",
            get(attachments::download)
                .put(attachments::upload)
                .layer(DefaultBodyLimit::max(attachments::MAX_SIZE)),
        )
        .layer(DefaultBodyLimit::max(131072))
        .layer(Extension(service.clone()))
        .layer(middleware::from_fn_with_state(service, gate))
}
/// F1b: supervisor machine routes, authenticated by the generation-bound
/// supervisor credential only (never a worker or owner session).
pub fn supervisor_routes(state: &AppState) -> Router<AppState> {
    let service = Arc::new(Service::new(state));
    Router::new()
        .route("/v1/supervisor/conversation/replies", post(reply::reply))
        .route(
            "/v1/supervisor/conversation/attachments/{attachment}",
            get(attachments::supervisor_download),
        )
        .layer(DefaultBodyLimit::max(131072))
        .layer(Extension(service.clone()))
        .layer(middleware::from_fn_with_state(service, gate))
}
async fn gate(State(service): State<Arc<Service>>, request: Request, next: Next) -> Response {
    let Ok(_permit) = service.gate.try_acquire() else {
        return unavailable().into_response();
    };
    let mut response = match tokio::time::timeout(Duration::from_secs(4), next.run(request)).await {
        Ok(r) => r,
        Err(_) => unavailable().into_response(),
    };
    response
        .headers_mut()
        .insert("cache-control", "no-store".parse().unwrap());
    response
}
async fn owner(tx: &mut Transaction<'_, Postgres>, user: &User) -> Result<()> {
    let row = sqlx::query("SELECT role,email,issuer,subject FROM users WHERE id=$1 FOR SHARE")
        .bind(user.id)
        .fetch_optional(&mut **tx)
        .await
        .map_err(db)?
        .ok_or_else(missing)?;
    if row.get::<String, _>("role") != "owner"
        || row.get::<String, _>("email") != user.email
        || row.get::<Option<String>, _>("issuer") != user.issuer
        || row.get::<Option<String>, _>("subject") != user.subject
    {
        return Err(AppError::Forbidden(
            "current owner identity required".into(),
        ));
    }
    Ok(())
}
async fn definition(
    tx: &mut Transaction<'_, Postgres>,
    id: Option<Uuid>,
    write: bool,
) -> Result<Conversation> {
    aq_supervisor_control::definition(tx, id, write)
        .await
        .map_err(|error| match error {
            sqlx::Error::RowNotFound if id.is_some() => missing(),
            _ => unavailable(),
        })
}
fn key(conversation: Uuid) -> Result<String> {
    digest(&recipient(conversation)).map_err(|_| unavailable())
}
