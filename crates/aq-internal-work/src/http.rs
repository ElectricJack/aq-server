//! Routes and handlers, generic over the host server so the server adapter
//! only says how to find the pool, the owner and how to render an [`Error`].
use crate::{Error, settings};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, FromRequestParts, Path, State},
    response::IntoResponse,
    routing::get,
};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

/// `S` holds the pool, `O` is the authenticated owner extractor and `E` the
/// server's error type. Both routes are owner-only.
pub fn routes<S, O, E>() -> Router<S>
where
    S: Clone + Send + Sync + AsRef<PgPool> + 'static,
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
