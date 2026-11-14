//! Supervisor authority over Intelligence settings and internal runs (spec 0012
//! §8). A bearer with the `settings` scope reaches the existing owner handlers
//! as its approving owner, as the read nest does. The bearer is the authority,
//! so the nest has no CSRF header; it is mounted only on the machine listener.
use crate::read::{Auth, Clock, bearer};
use aq_manual_sessions::Error;
use axum::{
    extract::{Request, State},
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};

/// Profiles and the routing policy, and the internal work default and
/// overrides: each document at its own path and below it.
const DOCUMENTS: [&str; 2] = ["/api/settings/intelligence", "/api/settings/internal-work"];
/// Internal runs: only below it.
const RUNS: &str = "/api/internal-work/";
/// Playbook sources (spec 0013 §4.4): the list and each source below it. A
/// supervisor may also replace a source's document (PUT) and delete one.
const SOURCES: &str = "/api/playbooks/sources";

fn under(path: &str, document: &str) -> bool {
    path.strip_prefix(document)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

fn forwarded(path: &str) -> bool {
    path.starts_with(RUNS)
        || under(path, SOURCES)
        || DOCUMENTS.iter().any(|document| under(path, document))
}

/// Any other path is 404 before authentication or the inner handler.
/// A project supervisor is not narrowed: these documents and runs are global.
pub async fn gate(State(auth): State<Auth>, mut request: Request, next: Next) -> Response {
    if !forwarded(request.uri().path()) {
        return Error(StatusCode::NOT_FOUND, "no such route").into_response();
    }
    let allowed = match *request.method() {
        Method::GET | Method::HEAD | Method::POST => true,
        Method::PUT | Method::DELETE => under(request.uri().path(), SOURCES),
        _ => false,
    };
    if !allowed {
        return Error(
            StatusCode::FORBIDDEN,
            "Supervisor admin endpoint accepts GET, HEAD and POST only",
        )
        .into_response();
    }
    let permit = match auth(request.headers().clone()).await {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    request.extensions_mut().insert(permit.owner);
    let response = next.run(request).await;
    // Held through the handler, as for reads: the credential stays live for the write.
    drop(permit.guard);
    response
}

/// Mounts `routes` at `/v1/supervisor/admin` behind [`gate`].
pub fn routes<S: Clone + Send + Sync + 'static>(
    routes: axum::Router<S>,
    pool: sqlx::PgPool,
    clock: Clock,
) -> axum::Router<S> {
    let auth = bearer(pool, clock, "settings");
    axum::Router::new().nest(
        "/v1/supervisor/admin",
        routes.layer(axum::middleware::from_fn_with_state(auth, gate)),
    )
}
