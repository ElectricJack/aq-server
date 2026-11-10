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
pub type Auth = Arc<
    dyn Fn(HeaderMap) -> Pin<Box<dyn Future<Output = Result<Permit, Error>> + Send>> + Send + Sync,
>;
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
pub async fn gate(State(auth): State<Auth>, mut request: Request, next: Next) -> Response {
    if request.method() != Method::GET && request.method() != Method::HEAD {
        return Error(
            StatusCode::FORBIDDEN,
            "Supervisor read endpoint accepts reads only",
        )
        .into_response();
    }
    let permit = match auth(request.headers().clone()).await {
        Ok(p) => p,
        Err(e) => return e.into_response(),
    };
    if let Some(project) = permit.project {
        let path = request.uri().path().split('/').collect::<Vec<_>>();
        let id = path
            .iter()
            .position(|p| *p == "projects")
            .and_then(|at| path.get(at + 1))
            .and_then(|s| s.parse::<Uuid>().ok());
        if id != Some(project) {
            return Error(
                StatusCode::FORBIDDEN,
                "Project supervisor cannot read another project",
            )
            .into_response();
        }
    }
    request.extensions_mut().insert(permit.owner);
    let response = next.run(request).await;
    // Dropping releases the same actor/credential fence held by authentication.
    drop(permit.guard);
    response
}

pub fn routes<S: Clone + Send + Sync + 'static>(
    routes: axum::Router<S>,
    pool: sqlx::PgPool,
    clock: Arc<dyn Fn() -> u64 + Send + Sync>,
) -> axum::Router<S> {
    let project_pool = pool.clone();
    let auth: Auth = Arc::new(move |headers| {
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
                "graph.read",
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
    });
    let project_auth = auth.clone();
    let projects = axum::routing::get(move |headers: HeaderMap| {
        let auth = project_auth.clone();
        let pool = project_pool.clone();
        async move {
            let permit = auth(headers).await?;
            let rows:Vec<serde_json::Value>=sqlx::query_scalar("SELECT jsonb_build_object('id',project_id,'revision',revision::text,'name',binding->>'name','enabled',binding->'enabled') FROM project_execution_bindings WHERE $1::uuid IS NULL OR project_id=$1 ORDER BY project_id LIMIT 256").bind(permit.project).fetch_all(&pool).await?;
            Ok::<_, Error>(axum::Json(serde_json::json!({"projects":rows})))
        }
    });
    axum::Router::new()
        .route("/v1/supervisor/projects", projects)
        .nest(
            "/v1/supervisor/read",
            routes.layer(axum::middleware::from_fn_with_state(auth, gate)),
        )
}
