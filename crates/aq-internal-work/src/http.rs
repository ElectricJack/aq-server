//! Routes and handlers, generic over the host server so the server adapter
//! only says how to find the pool, the owner and how to render an [`Error`].
use crate::{Error, runs, service::Service, settings};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, FromRequestParts, Path, Query, State},
    response::IntoResponse,
    routing::{get, post},
};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

/// What the routes need from the host server: its pool, and the service
/// that retries a run (an error when internal work is not running).
pub trait Host: AsRef<PgPool> {
    fn service(&self) -> Result<&Service, Error>;
}

/// `S` is the host, `O` the authenticated owner extractor and `E` the
/// server's error type. Every route is owner-only.
pub fn routes<S, O, E>() -> Router<S>
where
    S: Clone + Send + Sync + Host + 'static,
    O: FromRequestParts<S> + Into<Uuid> + Send + 'static,
    E: From<Error> + IntoResponse + 'static,
{
    Router::new()
        .route(
            "/api/settings/internal-work",
            get(read::<S, O, E>).post(save::<S, O, E>),
        )
        .route(
            "/api/settings/internal-work/operations/{id}",
            get(operation::<S, O, E>),
        )
        .route("/api/internal-work/runs", get(runs_list::<S, O, E>))
        .route("/api/internal-work/runs/{id}", get(run::<S, O, E>))
        .route("/api/internal-work/runs/{id}/retry", post(retry::<S, O, E>))
        .layer(DefaultBodyLimit::max(64 * 1024))
}
async fn read<S: AsRef<PgPool>, O, E: From<Error>>(
    State(s): State<S>,
    _: O,
) -> Result<Json<Value>, E> {
    Ok(Json(settings::read(s.as_ref()).await?))
}
async fn save<S: AsRef<PgPool>, O: Into<Uuid>, E: From<Error>>(
    State(s): State<S>,
    owner: O,
    Json(body): Json<Value>,
) -> Result<Json<Value>, E> {
    let request = serde_json::from_value(body)
        .map_err(|_| Error::Invalid("Invalid internal work settings request".into()))?;
    Ok(Json(
        settings::save(s.as_ref(), owner.into(), request).await?,
    ))
}
async fn operation<S: AsRef<PgPool>, O: Into<Uuid>, E: From<Error>>(
    State(s): State<S>,
    owner: O,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, E> {
    Ok(Json(
        settings::operation(s.as_ref(), owner.into(), id).await?,
    ))
}
async fn runs_list<S: AsRef<PgPool>, O, E: From<Error>>(
    State(s): State<S>,
    _: O,
    Query(q): Query<runs::Query>,
) -> Result<Json<Value>, E> {
    Ok(Json(runs::list(s.as_ref(), q).await?))
}
async fn run<S: AsRef<PgPool>, O, E: From<Error>>(
    State(s): State<S>,
    _: O,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, E> {
    Ok(Json(runs::read(s.as_ref(), id).await?))
}
async fn retry<S: Host, O, E: From<Error>>(
    State(s): State<S>,
    _: O,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, E> {
    let new = s.service()?.retry(id).await?;
    Ok(Json(serde_json::json!({"id": new})))
}
