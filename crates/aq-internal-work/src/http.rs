//! Routes and handlers, generic over the host server so the server adapter
//! only says how to find the pool, the owner and how to render an [`Error`].
use crate::{Error, runs, service::Service, settings, store};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, FromRequestParts, Path, Query, State},
    http::Extensions,
    response::IntoResponse,
    routing::{get, post},
};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

/// What the routes need from the host server: its pool, the service that
/// retries a run (an error when internal work is not running), and whether a
/// request came from a supervisor rather than the owner.
pub trait Host: AsRef<PgPool> {
    fn service(&self) -> Result<&Service, Error>;
    /// From what the host's authentication left on the request.
    fn by_supervisor(extensions: &Extensions) -> bool;
}

/// `S` is the host, `O` the authenticated owner extractor and `E` the
/// server's error type. Every route is owner-only; a supervisor reaches them
/// as its approving owner.
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
        .route(
            "/api/internal-work/runs/{id}/escalate",
            post(escalate::<S, O, E>),
        )
        .route(
            "/api/internal-work/runs/{id}/resolve",
            post(resolve::<S, O, E>),
        )
        .route(
            "/api/projects/{id}/escalations",
            post(escalate_decision::<S, O, E>),
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
    extensions: Extensions,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, E> {
    let new = s
        .service()?
        .retry(id, S::by_supervisor(&extensions))
        .await?;
    Ok(Json(serde_json::json!({"id": new})))
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Escalation {
    diagnosis: String,
}
async fn escalate<S: Host, O, E: From<Error>>(
    State(s): State<S>,
    _: O,
    Path(id): Path<Uuid>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, E> {
    let Escalation { diagnosis } = serde_json::from_value(body)
        .map_err(|_| Error::Invalid("Invalid internal run escalation".into()))?;
    s.service()?.escalate_to_owner(id, &diagnosis).await?;
    Ok(Json(runs::read(s.as_ref(), id).await?))
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    task_id: Option<Uuid>,
    decision_needed: String,
}
/// `supervisor.escalate`: the project is the path's, so a project supervisor
/// reaches only its own through the owner bridge.
async fn escalate_decision<S: Host, O, E: From<Error>>(
    State(s): State<S>,
    _: O,
    Path(project): Path<Uuid>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, E> {
    let invalid = || {
        Error::Invalid("Give decision_needed (1 to 4096 characters) and an optional task_id".into())
    };
    let Decision {
        task_id,
        decision_needed,
    } = serde_json::from_value(body).map_err(|_| invalid())?;
    let decision = decision_needed.trim();
    if decision.is_empty() || decision.chars().count() > 4096 || decision.contains('\0') {
        return Err(invalid().into());
    }
    let message = s
        .service()?
        .escalate_decision(project, task_id, decision)
        .await?;
    Ok(Json(
        serde_json::json!({"disposition":"confirmed","message_id":message}),
    ))
}
async fn resolve<S: Host, O, E: From<Error>>(
    State(s): State<S>,
    _: O,
    Path(id): Path<Uuid>,
) -> Result<Json<Value>, E> {
    store::resolve(s.as_ref(), id).await?;
    Ok(Json(runs::read(s.as_ref(), id).await?))
}
