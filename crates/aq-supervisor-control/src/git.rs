//! Read-only code access: a supervisor bearer with the `owner` scope fetches a
//! project-bound repository from central aq-git with git smart-HTTP at
//! `/v1/supervisor/git/<repository>/…`. The global supervisor reads any bound
//! repository, a project supervisor only its own project's.
use crate::read::{Clock, bearer};
use aq_delivery::git_grant::{Refusal, serve_read};
use aq_manual_sessions::Error;
use axum::{
    extract::{Path, Request},
    http::StatusCode,
    response::IntoResponse,
    routing::any,
};
use sqlx::PgPool;
use std::path::PathBuf;
use uuid::Uuid;

/// Whether a supervisor of `project` (`None`: the global supervisor) may read
/// `repository`: some binding names it, and for a project supervisor that
/// binding is its project's.
pub async fn may_read(
    pool: &PgPool,
    project: Option<Uuid>,
    repository: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM project_execution_bindings WHERE binding->>'repository_id'=$1 AND ($2::uuid IS NULL OR project_id=$2))")
        .bind(repository.to_string())
        .bind(project)
        .fetch_one(pool)
        .await
}

/// Mounts `/v1/supervisor/git/{repository}/{*path}` for every method; only a
/// fetch is forwarded, to aq-git's git socket at `socket`.
pub fn routes<S: Clone + Send + Sync + 'static>(
    pool: PgPool,
    clock: Clock,
    socket: PathBuf,
) -> axum::Router<S> {
    let auth = bearer(pool.clone(), clock, "owner");
    let fetch = any(
        move |Path((repository, path)): Path<(String, String)>, request: Request| {
            let (auth, pool, socket) = (auth.clone(), pool.clone(), socket.clone());
            async move {
                let permit = match auth(request.headers().clone()).await {
                    Ok(p) => p,
                    Err(e) => return e.into_response(),
                };
                // A transfer can run for minutes; it holds no credential locks.
                drop(permit.guard);
                let refused = Error(
                    StatusCode::FORBIDDEN,
                    "Supervisor can read only its projects' repositories",
                );
                let Ok(repository) = Uuid::try_parse(&repository) else {
                    return refused.into_response();
                };
                match may_read(&pool, permit.project, repository).await {
                    Ok(true) => {}
                    Ok(false) => return refused.into_response(),
                    Err(e) => return Error::from(e).into_response(),
                }
                let (status, why) =
                    match serve_read(&pool, &socket, repository, &path, request).await {
                        Ok(response) => return response.map(axum::body::Body::new).into_response(),
                        Err(Refusal::Forbidden(why)) => (StatusCode::FORBIDDEN, why),
                        Err(Refusal::BadRequest(why)) => (StatusCode::BAD_REQUEST, why),
                        Err(Refusal::Unavailable) => {
                            (StatusCode::SERVICE_UNAVAILABLE, "git endpoint unavailable")
                        }
                        // serve_read refuses nothing else: it never forwards a push.
                        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "unexpected git refusal"),
                    };
                Error(status, why).into_response()
            }
        },
    );
    // A literal path: the route review scans sources for `.route("…"`.
    axum::Router::new().route("/v1/supervisor/git/{repository}/{*path}", fetch)
}
