//! Optional preconfigured policy runtime. Existing admin auth/CSRF/maintenance
//! layers remain authoritative. No artifact, activation, generic command or
//! synthetic host registration route is exposed by this first adapter slice.
use super::{domain::*, driver::Driver, service::PrepareRequest};
use crate::{AppState, authz::RequireOwner};
use axum::{
    Extension, Json, Router,
    extract::{DefaultBodyLimit, Path, Request, State},
    http::StatusCode,
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::Semaphore;
use uuid::Uuid;
struct Budget {
    work: Semaphore,
    control: Semaphore,
}
async fn gate(State(budget): State<Arc<Budget>>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    let control = path.ends_with("/revoke") || path.ends_with("/cancel") || path.ends_with("/stop");
    let budget = if control {
        &budget.control
    } else {
        &budget.work
    };
    let Ok(_permit) = budget.try_acquire() else {
        return Error::Overloaded.into_response();
    };
    next.run(request).await
}
pub fn routes(driver: Arc<Driver>) -> Router<AppState> {
    Router::new()
        .merge(super::inspection::routes(driver.clone()))
        .route("/api/playbooks/status", get(status))
        .route("/api/playbooks/bindings/{id}", get(binding))
        .route("/api/playbooks/grants", post(grant))
        .route("/api/playbooks/grants/{id}/revoke", post(revoke))
        .route("/api/playbooks/admissions", post(prepare))
        .route("/api/playbooks/admissions/{id}", get(admission))
        .route(
            "/api/playbooks/admissions/{id}/recover",
            post(recover_admission),
        )
        .route(
            "/api/playbooks/admissions/{id}/retry",
            post(retry_admission),
        )
        .route("/api/playbooks/drive", post(drive))
        .route("/api/playbooks/recover", post(recover))
        .route("/api/playbooks/runs/{id}/cancel", post(cancel))
        .route("/api/playbooks/runs/{id}", get(inspect_run))
        .route("/api/playbooks/stop", post(stop))
        .layer(DefaultBodyLimit::max(65536))
        .layer(Extension(driver))
        .layer(middleware::from_fn_with_state(
            Arc::new(Budget {
                work: Semaphore::new(2),
                control: Semaphore::new(2),
            }),
            gate,
        ))
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let code = self.to_string();
        let unknown = self == Error::Unknown;
        let status = match self {
            Error::Invalid => StatusCode::BAD_REQUEST,
            Error::Forbidden => StatusCode::NOT_FOUND,
            Error::Conflict | Error::Fenced => StatusCode::CONFLICT,
            Error::Overloaded => StatusCode::TOO_MANY_REQUESTS,
            Error::Unknown => StatusCode::SERVICE_UNAVAILABLE,
        };
        (status,Json(json!({"code":code,"disposition":if unknown{"unknown"}else{"refused"},"retryable":false}))).into_response()
    }
}
fn owner(driver: &Driver, id: Uuid) -> Result<()> {
    if driver.service.namespace.owner_user_id != id {
        return Err(Error::Forbidden);
    }
    Ok(())
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Empty {}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GrantRequest {
    grant_id: Uuid,
    scope: GrantScope,
    expires_at_ms: i64,
}
async fn status(
    RequireOwner(user): RequireOwner,
    Extension(d): Extension<Arc<Driver>>,
) -> Result<Json<Value>> {
    owner(&d, user.id)?;
    let registry = d.service.profile.registry();
    Ok(Json(
        json!({"namespace":d.service.namespace,"catalog_digest":digest(&registry)?,"registered_commands":registry.commands.keys().collect::<Vec<_>>(),"automatic_driver":d.automatic_state()!="manual","automatic_driver_state":d.automatic_state(),"synchronous_calls_interruptible":false}),
    ))
}
async fn binding(
    RequireOwner(user): RequireOwner,
    Extension(d): Extension<Arc<Driver>>,
    Path(id): Path<String>,
) -> Result<Json<Value>> {
    owner(&d, user.id)?;
    Ok(Json(d.inspect_binding(id).await?))
}
async fn grant(
    RequireOwner(user): RequireOwner,
    Extension(d): Extension<Arc<Driver>>,
    Json(r): Json<GrantRequest>,
) -> Result<Json<Value>> {
    owner(&d, user.id)?;
    d.service
        .grant(user.id, r.grant_id, r.scope, r.expires_at_ms)
        .await?;
    Ok(Json(
        json!({"grant_id":r.grant_id,"disposition":"retained"}),
    ))
}
async fn revoke(
    RequireOwner(user): RequireOwner,
    Extension(d): Extension<Arc<Driver>>,
    Path(id): Path<Uuid>,
    Json(_): Json<Empty>,
) -> Result<Json<Value>> {
    owner(&d, user.id)?;
    d.service.revoke(user.id, id).await?;
    Ok(Json(json!({"grant_id":id,"disposition":"revoked"})))
}
async fn prepare(
    RequireOwner(user): RequireOwner,
    Extension(d): Extension<Arc<Driver>>,
    Json(r): Json<PrepareRequest>,
) -> Result<impl IntoResponse> {
    owner(&d, user.id)?;
    Ok((
        StatusCode::ACCEPTED,
        Json(d.service.prepare(user.id, r).await?),
    ))
}
async fn admission(
    RequireOwner(user): RequireOwner,
    Extension(d): Extension<Arc<Driver>>,
    Path(id): Path<Uuid>,
) -> Result<impl IntoResponse> {
    owner(&d, user.id)?;
    Ok(Json(d.service.admission(user.id, id).await?.status))
}
async fn recover_admission(
    RequireOwner(user): RequireOwner,
    Extension(d): Extension<Arc<Driver>>,
    Path(id): Path<Uuid>,
    Json(_): Json<Empty>,
) -> Result<impl IntoResponse> {
    owner(&d, user.id)?;
    Ok(Json(d.admission(user.id, id, true).await?))
}
async fn retry_admission(
    RequireOwner(user): RequireOwner,
    Extension(d): Extension<Arc<Driver>>,
    Path(id): Path<Uuid>,
    Json(_): Json<Empty>,
) -> Result<impl IntoResponse> {
    owner(&d, user.id)?;
    Ok(Json(d.admission(user.id, id, false).await?))
}
async fn drive(
    RequireOwner(user): RequireOwner,
    Extension(d): Extension<Arc<Driver>>,
    Json(_): Json<Empty>,
) -> Result<impl IntoResponse> {
    owner(&d, user.id)?;
    Ok(Json(d.quantum().await?))
}
async fn recover(
    RequireOwner(user): RequireOwner,
    Extension(d): Extension<Arc<Driver>>,
    Json(_): Json<Empty>,
) -> Result<impl IntoResponse> {
    owner(&d, user.id)?;
    Ok(Json(d.recover().await?))
}
async fn cancel(
    RequireOwner(user): RequireOwner,
    Extension(d): Extension<Arc<Driver>>,
    Path(id): Path<String>,
    Json(_): Json<Empty>,
) -> Result<impl IntoResponse> {
    owner(&d, user.id)?;
    d.cancel_run(id).await?;
    Ok(Json(json!({"disposition":"requested"})))
}
async fn stop(
    RequireOwner(user): RequireOwner,
    Extension(d): Extension<Arc<Driver>>,
    Json(_): Json<Empty>,
) -> Result<impl IntoResponse> {
    owner(&d, user.id)?;
    d.stop().await?;
    Ok(Json(
        json!({"disposition":"driver_stopped","queued_messages":"retained"}),
    ))
}

/// Only finite typed stop/revoke/cancel routes survive maintenance or use the
/// execution control budget. No prefix grants general mutation authority.
pub(crate) fn cancellation_path(path: &str) -> bool {
    if path == "/api/playbooks/stop" {
        return true;
    }
    if let Some(id) = path
        .strip_prefix("/api/playbooks/grants/")
        .and_then(|s| s.strip_suffix("/revoke"))
    {
        return Uuid::parse_str(id).is_ok();
    }
    if let Some(id) = path
        .strip_prefix("/api/playbooks/runs/")
        .and_then(|s| s.strip_suffix("/cancel"))
    {
        return id
            .strip_prefix("run-")
            .is_some_and(|s| s.len() == 16 && s.bytes().all(|b| b.is_ascii_hexdigit()))
            || id
                .strip_prefix("event-sha256:")
                .is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()));
    }
    false
}

async fn inspect_run(
    RequireOwner(user): RequireOwner,
    Extension(d): Extension<Arc<Driver>>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse> {
    owner(&d, user.id)?;
    Ok(Json(d.inspect_run(id).await?))
}
