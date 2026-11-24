//! Trusted in-process bridge to existing owner read projections. The extension
//! cannot be supplied over HTTP, and the central fence is held through the read.
use aq_manual_sessions::Error;
use axum::{
    extract::{Request, State},
    http::{HeaderMap, Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use sqlx::{Postgres, Row, Transaction};
use std::{future::Future, pin::Pin, sync::Arc};
use uuid::Uuid;
#[derive(Clone)]
pub struct ReadOwner {
    pub id: Uuid,
    pub email: String,
    pub issuer: Option<String>,
    pub subject: Option<String>,
}
pub struct Permit {
    pub owner: ReadOwner,
    pub project: Option<Uuid>,
    pub guard: Transaction<'static, Postgres>,
}
/// Passed only to handlers that have cleared the project ownership gate.
#[derive(Clone)]
pub struct ProjectScope;
pub type Auth = Arc<
    dyn Fn(HeaderMap) -> Pin<Box<dyn Future<Output = Result<Permit, Error>> + Send>> + Send + Sync,
>;
/// The owner a trusted in-process caller vouches for: the request extension,
/// or a bridged request's [`crate::owner::DELEGATED_OWNER`].
pub fn delegated(extensions: &axum::http::Extensions) -> Option<ReadOwner> {
    let owner = extensions.get::<ReadOwner>().cloned();
    #[cfg(feature = "supervisor_full_access")]
    {
        owner.or_else(|| crate::owner::DELEGATED_OWNER.try_with(Clone::clone).ok())
    }
    #[cfg(not(feature = "supervisor_full_access"))]
    {
        owner
    }
}
pub async fn owner(tx: &mut Transaction<'_, Postgres>, actor: Uuid) -> Result<ReadOwner, Error> {
    let row=sqlx::query("SELECT u.id,u.email,u.issuer,u.subject,a.approved_identity FROM central_supervisor_actors a JOIN users u ON u.id=a.approved_by WHERE a.actor_id=$1 AND u.role='owner' FOR SHARE OF u").bind(actor).fetch_optional(&mut **tx).await?.ok_or(Error(StatusCode::FORBIDDEN,"Supervisor owner authorization is unavailable"))?;
    let owner = ReadOwner {
        id: row.get("id"),
        email: row.get("email"),
        issuer: row.get("issuer"),
        subject: row.get("subject"),
    };
    let expected = serde_json::json!({"id":owner.id,"email":owner.email,"issuer":owner.issuer,"subject":owner.subject,"role":"owner"});
    if row.get::<serde_json::Value, _>("approved_identity") != expected {
        return Err(Error(
            StatusCode::FORBIDDEN,
            "Supervisor owner identity changed",
        ));
    }
    Ok(owner)
}
/// A refusal of the supervisor read API. Its own error code, so a supervisor
/// can tell it from a manual-session refusal.
pub struct Refused(pub StatusCode, pub &'static str);
impl IntoResponse for Refused {
    fn into_response(self) -> Response {
        (
            self.0,
            axum::Json(serde_json::json!({"error":"supervisor_refused","message":self.1})),
        )
            .into_response()
    }
}
impl From<Error> for Refused {
    fn from(e: Error) -> Self {
        Self(e.0, e.1)
    }
}
const OTHER_PROJECT: Refused = Refused(
    StatusCode::FORBIDDEN,
    "Project supervisor cannot read another project",
);
/// What a read path names, which decides how a project supervisor is scoped.
#[derive(Debug, PartialEq)]
enum Reads {
    /// `.../projects/{id}/...`; `None` when the id is not a UUID.
    Project(Option<Uuid>),
    /// `/api/human/results`: a page the gate narrows to the project.
    Results,
    /// One result revision, by the result's or report's ID.
    Result(Uuid),
    /// One profiled attempt, by its immutable launch's ID.
    Attempt(Uuid),
    /// Nothing a project supervisor can read.
    Unserved,
}
fn reads(path: &str) -> Reads {
    let path = path.strip_prefix("/v1/supervisor/read").unwrap_or(path);
    let parts = path.trim_start_matches('/').split('/').collect::<Vec<_>>();
    match parts.as_slice() {
        ["api", "human", "results"] => return Reads::Results,
        ["api", "human" | "v1", "results", id, "revisions", _] => {
            return id.parse().map_or(Reads::Unserved, Reads::Result);
        }
        ["api", "execution", "attempts", id] => {
            return id.parse().map_or(Reads::Unserved, Reads::Attempt);
        }
        _ => {}
    }
    match parts.iter().position(|p| *p == "projects") {
        Some(at) => Reads::Project(parts.get(at + 1).and_then(|s| s.parse().ok())),
        None => Reads::Unserved,
    }
}
/// `query` with `project_id` set to `project`; `None` when it names another
/// project (or hides the key behind an escape).
fn narrowed(query: Option<&str>, project: Uuid) -> Option<String> {
    let mut pairs = Vec::new();
    let mut named = false;
    for pair in query.unwrap_or("").split('&').filter(|p| !p.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        if key.contains(['%', '+']) {
            return None;
        }
        if key == "project_id" && value.parse::<Uuid>().ok() != Some(project) {
            return None;
        }
        named |= key == "project_id";
        pairs.push(pair.to_string());
    }
    if !named {
        pairs.push(format!("project_id={project}"));
    }
    Some(pairs.join("&"))
}
/// Whether the result (or report) `id` belongs to `project`.
async fn in_project(
    tx: &mut Transaction<'static, Postgres>,
    id: Uuid,
    project: Uuid,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM profiled_result_reports r JOIN profiled_launches l USING(attempt_id) WHERE r.report_id=$1 AND (l.execution_spec#>>'{lifecycle,project_id}')=$2::text) OR EXISTS(SELECT 1 FROM result_revision_pins WHERE (report_id=$1 OR result_id=$1) AND project_id=$2)")
        .bind(id).bind(project).fetch_one(&mut **tx).await
}
/// What no supervisor read route serves.
pub async fn unserved() -> Refused {
    Refused(StatusCode::NOT_FOUND, "No such supervisor route")
}
fn narrow_uri(uri: &axum::http::Uri, project: Uuid) -> Option<axum::http::Uri> {
    let query = narrowed(uri.query(), project)?;
    let mut parts = uri.clone().into_parts();
    parts.path_and_query = format!("{}?{query}", uri.path()).parse().ok();
    axum::http::Uri::from_parts(parts).ok()
}

async fn scoped_read(
    target: Reads,
    request: &mut Request,
    tx: &mut Transaction<'static, Postgres>,
    project: Uuid,
) -> Result<(), Refused> {
    match target {
        Reads::Project(Some(id)) if id == project => Ok(()),
        Reads::Project(_) => Err(OTHER_PROJECT),
        Reads::Unserved => Err(unserved().await),
        Reads::Result(id) => match in_project(tx, id, project).await {
            Ok(true) => Ok(()),
            Ok(false) => Err(OTHER_PROJECT),
            Err(e) => Err(Refused::from(Error::from(e))),
        },
        Reads::Attempt(id) => {
            // The execution spec is immutable after the server seals the launch.
            let own: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM profiled_launches WHERE attempt_id=$1 AND execution_spec#>>'{lifecycle,project_id}'=$2::text)",
            )
            .bind(id)
            .bind(project)
            .fetch_one(&mut **tx)
            .await
            .map_err(|err| Refused::from(Error::from(err)))?;
            own.then_some(()).ok_or(OTHER_PROJECT)
        }
        Reads::Results => {
            *request.uri_mut() = narrow_uri(request.uri(), project).ok_or(OTHER_PROJECT)?;
            Ok(())
        }
    }
}

pub async fn gate(State(auth): State<Auth>, mut request: Request, next: Next) -> Response {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return Refused(
            StatusCode::FORBIDDEN,
            "Supervisor read endpoint accepts reads only",
        )
        .into_response();
    }
    let mut permit = match auth(request.headers().clone()).await {
        Ok(p) => p,
        Err(e) => return Refused::from(e).into_response(),
    };
    let target = reads(request.uri().path());
    if cfg!(feature = "supervisor_full_access")
        && !matches!(
            target,
            Reads::Results | Reads::Result(_) | Reads::Attempt(_)
        )
    {
        return unserved().await.into_response();
    }
    if let Some(project) = permit.project {
        if let Err(refusal) = scoped_read(target, &mut request, &mut permit.guard, project).await {
            return refusal.into_response();
        }
        request.extensions_mut().insert(ProjectScope);
    }
    request.extensions_mut().insert(permit.owner);
    let response = next.run(request).await;
    // Dropping releases the same actor/credential fence held by authentication.
    drop(permit.guard);
    response
}

pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;
/// Authenticates the supervisor bearer for `scope` and resolves its approving
/// owner, holding the actor/credential fence in the returned permit.
pub(crate) fn bearer(pool: sqlx::PgPool, clock: Clock, scope: &'static str) -> Auth {
    Arc::new(move |headers| {
        let pool = pool.clone();
        let clock = clock.clone();
        Box::pin(async move {
            let token = headers
                .get("authorization")
                .and_then(|h| h.to_str().ok())
                .and_then(|h| h.strip_prefix("Bearer "))
                .ok_or(Error(
                    StatusCode::UNAUTHORIZED,
                    "Supervisor credential required",
                ))?;
            let mut tx = pool.begin().await?;
            let principal = crate::authority::authenticate_in_transaction(
                &pool,
                &mut tx,
                token,
                scope,
                clock(),
            )
            .await?;
            let owner = owner(&mut tx, principal.actor_id).await?;
            Ok(Permit {
                owner,
                project: principal.project_id,
                guard: tx,
            })
        })
    })
}

/// `/v1/supervisor/projects`: the projects the bearer's scope covers.
pub fn projects<S: Clone + Send + Sync + 'static>(
    pool: sqlx::PgPool,
    clock: Clock,
) -> axum::Router<S> {
    let auth = bearer(pool.clone(), clock, "graph.read");
    let projects = axum::routing::get(move |headers: HeaderMap| {
        let auth = auth.clone();
        let pool = pool.clone();
        async move {
            let permit = auth(headers).await?;
            let project = permit.project;
            let rows:Vec<serde_json::Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',project_id,'revision',revision::text,'name',binding->>'name','enabled',binding->'enabled') FROM project_execution_bindings WHERE $1::uuid IS NULL OR project_id=$1 ORDER BY project_id LIMIT 256").bind(project).fetch_all(&pool).await?;
            Ok::<_, Error>(axum::Json(serde_json::json!({"projects":rows})))
        }
    });
    axum::Router::new().route("/v1/supervisor/projects", projects)
}

/// Mounts `routes` at `/v1/supervisor/read` behind [`gate`].
pub fn routes<S: Clone + Send + Sync + 'static>(
    routes: axum::Router<S>,
    pool: sqlx::PgPool,
    clock: Clock,
) -> axum::Router<S> {
    nest(routes, bearer(pool, clock, "graph.read"))
}

/// [`routes`] behind a given `auth`. The gate also covers the fallback: a
/// bearer that passes it and asks for a path no route serves is refused as a
/// supervisor request ([`unserved`]).
pub fn nest<S: Clone + Send + Sync + 'static>(
    routes: axum::Router<S>,
    auth: Auth,
) -> axum::Router<S> {
    let routes = routes
        .fallback(unserved)
        .layer(axum::middleware::from_fn_with_state(auth, gate));
    axum::Router::new().nest("/v1/supervisor/read", routes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    #[test]
    fn paths_say_what_a_project_supervisor_reads() {
        let (a, rev) = (id(1), "0".repeat(64));
        let project = format!("/api/human/projects/{a}/tasks/{}/work", id(2));
        assert_eq!(reads(&project), Reads::Project(Some(a)));
        assert_eq!(
            reads(&format!("/v1/supervisor/read{project}")),
            Reads::Project(Some(a))
        );
        assert_eq!(reads("/api/human/projects/x/tasks"), Reads::Project(None));
        assert_eq!(reads("/api/human/results"), Reads::Results);
        for family in ["human", "v1"] {
            let path = format!("/api/{family}/results/{a}/revisions/{rev}");
            assert_eq!(reads(&path), Reads::Result(a), "{path}");
        }
        // Anything else is not a supervisor route, however it is spelled.
        for path in [
            "/api/human/conversations/global",
            "/api/human/results/not-a-uuid/revisions/r",
            "/api/v1/results",
            "/nope",
            "/",
        ] {
            assert_eq!(reads(path), Reads::Unserved, "{path}");
        }
        let path = format!("/api/v1/results/{a}/revisions/{rev}/comments");
        assert_eq!(reads(&path), Reads::Unserved, "{path}");
    }

    #[test]
    fn a_result_page_is_narrowed_to_the_project() {
        let (p, other) = (id(1), id(2));
        assert_eq!(narrowed(None, p), Some(format!("project_id={p}")));
        assert_eq!(
            narrowed(Some("limit=5"), p),
            Some(format!("limit=5&project_id={p}"))
        );
        assert_eq!(
            narrowed(Some(&format!("project_id={p}&limit=5")), p),
            Some(format!("project_id={p}&limit=5"))
        );
        for query in [
            format!("project_id={other}"),
            "project_id".to_string(),
            "project_id=".to_string(),
            format!("project%5Fid={other}"),
            format!("project+id={other}"),
        ] {
            assert_eq!(narrowed(Some(&query), p), None, "{query}");
        }
    }
}
