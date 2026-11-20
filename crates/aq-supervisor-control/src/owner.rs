//! The owner bridge: a supervisor bearer with the `owner` scope reaches the
//! live owner app in process, as the owner who approved the supervisor.
use crate::read::{Clock, ReadOwner, bearer};
use axum::{
    extract::{ConnectInfo, Request},
    http::{HeaderValue, Uri},
    response::{IntoResponse, Response},
    routing::any,
};
use std::net::SocketAddr;
use tower::ServiceExt;

tokio::task_local! {
    /// The approving owner while a bridged request runs. Owner checks that
    /// see only headers (manual agents, supervisor controls) read it.
    pub static DELEGATED_OWNER: ReadOwner;
}

pub const PREFIX: &str = "/v1/supervisor/owner";

/// The owner who approved `p`, when `p` may act as that owner.
pub async fn approving_owner(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    p: &crate::authority::SupervisorPrincipal,
) -> Option<uuid::Uuid> {
    if !p.scopes.iter().any(|s| s == "owner") {
        return None;
    }
    crate::read::owner(tx, p.actor_id)
        .await
        .ok()
        .map(|owner| owner.id)
}

/// Mounts `{PREFIX}/{*rest}` for every method; each request is forwarded to
/// `owner_app` at `/{rest}` with the owner's identity and none of the caller's.
pub fn routes<S: Clone + Send + Sync + 'static>(
    owner_app: axum::Router,
    pool: sqlx::PgPool,
    clock: Clock,
) -> axum::Router<S> {
    let auth = bearer(pool, clock, "owner");
    let bridge = any(move |request: Request| {
        let (auth, owner_app) = (auth.clone(), owner_app.clone());
        async move {
            let permit = match auth(request.headers().clone()).await {
                Ok(p) => p,
                Err(e) => return e.into_response(),
            };
            // Release the credential locks: the owner handler may write the same rows.
            drop(permit.guard);
            let (parts, body) = request.into_parts();
            let tail = parts
                .uri
                .path_and_query()
                .map_or("", |p| p.as_str())
                .strip_prefix(PREFIX)
                .unwrap_or_default();
            let Ok(uri) = tail.parse::<Uri>() else {
                return axum::http::StatusCode::BAD_REQUEST.into_response();
            };
            // A fresh request: the outer router's path params and matched
            // path would otherwise leak into the owner app's routing.
            let mut forwarded = Request::new(body);
            *forwarded.method_mut() = parts.method;
            *forwarded.uri_mut() = uri;
            *forwarded.version_mut() = parts.version;
            *forwarded.headers_mut() = parts.headers;
            let headers = forwarded.headers_mut();
            for name in ["authorization", "cookie", "x-aq-dev-email"] {
                headers.remove(name);
            }
            headers.insert("x-aq-request", HeaderValue::from_static("1"));
            if let Some(peer) = parts.extensions.get::<ConnectInfo<SocketAddr>>() {
                forwarded.extensions_mut().insert(*peer);
            }
            forwarded.extensions_mut().insert(permit.owner.clone());
            let response: Response = DELEGATED_OWNER
                .scope(permit.owner, owner_app.oneshot(forwarded))
                .await
                .unwrap_or_else(|never| match never {});
            response
        }
    });
    // A literal path: the route review scans sources for `.route("…"`.
    axum::Router::new().route("/v1/supervisor/owner/{*rest}", bridge)
}
