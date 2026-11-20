//! The owner bridge: a supervisor bearer with the `owner` scope reaches the
//! live owner app in process, as the owner who approved the supervisor.
use crate::read::{Clock, ReadOwner, bearer};
use axum::{
    body::Body,
    extract::{ConnectInfo, Request},
    http::{HeaderValue, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::any,
};
use serde_json::Value;
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
    // The owner journal fallback is for the global supervisor only.
    if p.project_id.is_some() || !p.scopes.iter().any(|s| s == "owner") {
        return None;
    }
    crate::read::owner(tx, p.actor_id)
        .await
        .ok()
        .map(|owner| owner.id)
}

/// Whether the request names at least one project and every project it names
/// is `project`: each segment after `projects` in `path` (no query), and for
/// `POST /api/v1/actions` the body's `payload.project_id` and project subject.
pub fn names_only(project: uuid::Uuid, method: &Method, path: &str, body: Option<&Value>) -> bool {
    let segments: Vec<&str> = path.split('/').collect();
    // An unparsable id still counts as named, so it can only refuse.
    let mut named: Vec<Option<uuid::Uuid>> = segments
        .windows(2)
        .filter(|w| w[0] == "projects")
        .map(|w| w[1].parse().ok())
        .collect();
    let id = |v: Option<&Value>| v.and_then(Value::as_str).and_then(|s| s.parse().ok());
    if let (&Method::POST, "/api/v1/actions", Some(body)) = (method, path, body) {
        if let Some(p) = body.pointer("/payload/project_id") {
            named.push(id(Some(p)));
        }
        if body.pointer("/subject/type").and_then(Value::as_str) == Some("project") {
            named.push(id(body.pointer("/subject/id")));
        }
    }
    !named.is_empty() && named.iter().all(|n| *n == Some(project))
}

/// Whether project `$1` has a non-nil Graph workspace no other project shares.
const OWN_WORKSPACE: &str = "SELECT EXISTS(SELECT 1 FROM project_execution_bindings b WHERE b.project_id=$1 AND b.binding->>'graph_workspace_id'<>'00000000-0000-0000-0000-000000000000' AND NOT EXISTS(SELECT 1 FROM project_execution_bindings o WHERE o.project_id<>b.project_id AND o.binding->>'graph_workspace_id'=b.binding->>'graph_workspace_id'))";

/// Mounts `{PREFIX}/{*rest}` for every method; each request is forwarded to
/// `owner_app` at `/{rest}` with the owner's identity and none of the caller's.
pub fn routes<S: Clone + Send + Sync + 'static>(
    owner_app: axum::Router,
    pool: sqlx::PgPool,
    clock: Clock,
) -> axum::Router<S> {
    let auth = bearer(pool.clone(), clock, "owner");
    let bridge = any(move |request: Request| {
        let (auth, owner_app, pool) = (auth.clone(), owner_app.clone(), pool.clone());
        async move {
            let permit = match auth(request.headers().clone()).await {
                Ok(p) => p,
                Err(e) => return e.into_response(),
            };
            // Release the credential locks: the owner handler may write the same rows.
            drop(permit.guard);
            let (parts, mut body) = request.into_parts();
            let tail = parts
                .uri
                .path_and_query()
                .map_or("", |p| p.as_str())
                .strip_prefix(PREFIX)
                .unwrap_or_default();
            let Ok(uri) = tail.parse::<Uri>() else {
                return StatusCode::BAD_REQUEST.into_response();
            };
            if let Some(project) = permit.project {
                let mut json = None;
                if parts.method == Method::POST && uri.path() == "/api/v1/actions" {
                    // Any read failure, the limit included, refuses as too large.
                    let Ok(bytes) = axum::body::to_bytes(body, 65536).await else {
                        return StatusCode::PAYLOAD_TOO_LARGE.into_response();
                    };
                    let Ok(value) = serde_json::from_slice(&bytes) else {
                        return StatusCode::BAD_REQUEST.into_response();
                    };
                    (json, body) = (Some(value), Body::from(bytes));
                }
                let refusal = aq_manual_sessions::Error(
                    StatusCode::FORBIDDEN,
                    "Project supervisor can act only on its own project",
                );
                if !names_only(project, &parts.method, uri.path(), json.as_ref()) {
                    return refusal.into_response();
                }
                // Owner handlers bind a task only to its project's Graph workspace,
                // so a workspace shared with another project would let one in.
                match sqlx::query_scalar::<_, bool>(OWN_WORKSPACE)
                    .bind(project)
                    .fetch_one(&pool)
                    .await
                {
                    Ok(true) => {}
                    Ok(false) => return refusal.into_response(),
                    Err(e) => return aq_manual_sessions::Error::from(e).into_response(),
                }
            }
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

#[cfg(test)]
mod tests {
    use super::names_only;
    use axum::http::Method;
    use serde_json::json;
    use uuid::Uuid;

    const P: Uuid = Uuid::from_u128(1);
    const Q: Uuid = Uuid::from_u128(2);

    #[test]
    fn names_only_accepts_the_own_project_path() {
        let path = format!("/api/human/projects/{P}/tasks");
        assert!(names_only(P, &Method::GET, &path, None));
    }

    #[test]
    fn names_only_refuses_another_or_no_project() {
        for path in [
            format!("/api/human/projects/{Q}/tasks"),
            "/api/v1/swarm".into(),
            format!("/api/x/projects/{P}/y/projects/{Q}"),
            "/api/human/projects/not-a-uuid/tasks".into(),
        ] {
            assert!(!names_only(P, &Method::GET, &path, None), "{path}");
        }
    }

    #[test]
    fn names_only_matches_case_insensitively() {
        let path = format!("/api/human/projects/{}/tasks", P.to_string().to_uppercase());
        assert!(names_only(P, &Method::GET, &path, None));
    }

    #[test]
    fn names_only_reads_action_bodies() {
        let task = Uuid::from_u128(9);
        for (body, expected) in [
            (
                json!({"subject":{"type":"task","id":task},"payload":{"project_id":P}}),
                true,
            ),
            (
                json!({"subject":{"type":"task","id":task},"payload":{"project_id":Q}}),
                false,
            ),
            (
                json!({"subject":{"type":"project","id":P},"payload":{}}),
                true,
            ),
            (
                json!({"subject":{"type":"project","id":P},"payload":{"project_id":Q}}),
                false,
            ),
            (
                json!({"subject":{"type":"task","id":task},"payload":{}}),
                false,
            ),
        ] {
            let named = names_only(P, &Method::POST, "/api/v1/actions", Some(&body));
            assert_eq!(named, expected, "{body}");
        }
    }

    #[test]
    fn names_only_ignores_queries() {
        assert!(!names_only(P, &Method::GET, "/api/v1/tasks/overview", None));
    }
}
