//! Explicit human-only adapter. The daemon merges this below its existing
//! identity, CSRF, maintenance and admission layers; no worker listener mount.
use super::{Error, body, media, now, runtime::Runtime};
use crate::{AppState, authz::RequireOwner, error::AppError, store::User};
use axum::{
    Extension, Router,
    body::to_bytes,
    extract::{Path, Query, Request},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde::Deserialize;
use std::{sync::Arc, time::Duration};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;
type ApiResult<T> = std::result::Result<T, AppError>;
fn invalid() -> AppError {
    AppError::BadRequest("invalid media request".into())
}
fn mapped(error: Error) -> AppError {
    match error {
        Error::Access => {
            AppError::Forbidden("media requires the same current Owner principal".into())
        }
        Error::NotFound => AppError::NotFound("media association not found".into()),
        Error::Invalid => invalid(),
        Error::Conflict => {
            AppError::Conflict("media operation conflicts with its immutable intent".into())
        }
        Error::Capacity => AppError::PayloadTooLarge("media retention capacity exhausted".into()),
        Error::Busy => {
            AppError::UpdatesUnavailable("media owner busy; inspect the original operation".into())
        }
        _ => AppError::UpdatesUnavailable(
            "media owner unavailable; inspect the original operation".into(),
        ),
    }
}
fn slot(budget: &Arc<Semaphore>) -> ApiResult<OwnedSemaphorePermit> {
    budget
        .clone()
        .try_acquire_owned()
        .map_err(|_| mapped(Error::Busy))
}
fn id(value: &str) -> ApiResult<Uuid> {
    media::Id::parse(value)
        .map(|v| v.as_uuid())
        .map_err(|_| invalid())
}
fn revision(value: &str) -> ApiResult<()> {
    media::ReportRevision::parse(value)
        .map(|_| ())
        .map_err(|_| invalid())
}
fn no_query(request: &Request) -> ApiResult<()> {
    if request.uri().query().is_some() {
        return Err(invalid());
    }
    Ok(())
}
fn json_response(status: StatusCode, value: &impl serde::Serialize) -> ApiResult<Response> {
    let bytes = media::canonical_bytes(value).map_err(|_| AppError::Internal)?;
    if bytes.len() > media::MAX_PAGE_BYTES {
        return Err(AppError::Internal);
    }
    let mut response = (
        status,
        [
            (header::CONTENT_TYPE, "application/json"),
            (header::CACHE_CONTROL, "private, no-store"),
        ],
        bytes,
    )
        .into_response();
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        "nosniff".parse().expect("staticheader"),
    );
    Ok(response)
}
async fn bytes(request: Request, limit: usize) -> ApiResult<Vec<u8>> {
    if request.headers().contains_key(header::CONTENT_ENCODING) {
        return Err(invalid());
    }
    let bytes = tokio::time::timeout(
        Duration::from_secs(10),
        to_bytes(request.into_body(), limit),
    )
    .await
    .map_err(|_| AppError::BadRequest("media request body timed out".into()))?
    .map_err(|_| AppError::PayloadTooLarge("media request body exceeds its bound".into()))?;
    Ok(bytes.to_vec())
}
async fn empty(request: Request) -> ApiResult<()> {
    bytes(request, 0).await.map(|_| ())
}
fn json_mime(headers: &HeaderMap) -> ApiResult<()> {
    if headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_none_or(|v| !matches!(v, "application/json" | "application/json; charset=utf-8"))
    {
        return Err(invalid());
    }
    Ok(())
}
pub fn routes(runtime: Arc<Runtime>) -> Router<AppState> {
    let mut routes = Router::new()
        .route(
            "/api/human/results/{report}/revisions/{revision}/media",
            get(list),
        )
        .route(
            "/api/human/results/{report}/revisions/{revision}/media/{artifact}/content",
            get(content),
        );
    routes = routes.route("/api/human/result-media-uploads/{operation}", get(status));
    if runtime.uploads_enabled() {
        routes = routes
            .route(
                "/api/human/results/{report}/revisions/{revision}/media-uploads",
                post(admit),
            )
            .route(
                "/api/human/result-media-uploads/{operation}/content",
                put(upload),
            )
            .route(
                "/api/human/result-media-uploads/{operation}/finalize",
                post(finalize),
            );
    }
    routes.layer(Extension(runtime))
}
async fn list(
    Extension(runtime): Extension<Arc<Runtime>>,
    RequireOwner(actor): RequireOwner,
    Path((report, rev)): Path<(String, String)>,
    request: Request,
) -> ApiResult<Response> {
    let _permit = slot(&runtime.metadata)?;
    no_query(&request)?;
    empty(request).await?;
    let report = id(&report)?;
    revision(&rev)?;
    let page = runtime
        .owner()
        .list(&actor, report, &rev)
        .await
        .map_err(mapped)?;
    json_response(StatusCode::OK, &page)
}
async fn admit(
    Extension(runtime): Extension<Arc<Runtime>>,
    RequireOwner(actor): RequireOwner,
    Path((report, rev)): Path<(String, String)>,
    request: Request,
) -> ApiResult<Response> {
    let _permit = slot(&runtime.uploads)?;
    no_query(&request)?;
    json_mime(request.headers())?;
    let report = id(&report)?;
    revision(&rev)?;
    let raw = bytes(request, media::MAX_JSON_BYTES).await?;
    let admitted = runtime
        .owner()
        .admit(&actor, report, &rev, &raw)
        .await
        .map_err(mapped)?;
    json_response(
        if admitted.created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        &admitted.status,
    )
}
async fn status(
    Extension(runtime): Extension<Arc<Runtime>>,
    RequireOwner(actor): RequireOwner,
    Path(operation): Path<String>,
    request: Request,
) -> ApiResult<Response> {
    let _permit = slot(&runtime.metadata)?;
    no_query(&request)?;
    empty(request).await?;
    let value = runtime
        .owner()
        .status(&actor, id(&operation)?)
        .await
        .map_err(mapped)?;
    json_response(StatusCode::OK, &value)
}
fn operation_code(runtime: &Runtime, status: &media::UploadStatus) -> ApiResult<StatusCode> {
    use media::UploadState as S;
    Ok(if status.state == S::Retained {
        StatusCode::OK
    } else if status.state == S::Rejected {
        StatusCode::UNPROCESSABLE_ENTITY
    } else if status.state == S::Expired
        || now(&runtime.owner.state).map_err(mapped)? >= status.upload_expires_at_ms
    {
        StatusCode::GONE
    } else if status.state == S::Finalizing
        || status.recovery.reason == Some(media::RecoveryReason::Busy)
    {
        StatusCode::ACCEPTED
    } else {
        StatusCode::OK
    })
}
async fn operation_result(
    runtime: &Runtime,
    actor: &User,
    operation: Uuid,
    result: super::Result<media::UploadStatus>,
) -> ApiResult<Response> {
    let status = match result {
        Ok(status) => status,
        Err(Error::Busy | Error::Expired) => runtime
            .owner()
            .status(actor, operation)
            .await
            .map_err(mapped)?,
        Err(error) => return Err(mapped(error)),
    };
    json_response(operation_code(runtime, &status)?, &status)
}
async fn upload(
    Extension(runtime): Extension<Arc<Runtime>>,
    RequireOwner(actor): RequireOwner,
    Path(operation): Path<String>,
    request: Request,
) -> ApiResult<Response> {
    let _permit = slot(&runtime.uploads)?;
    no_query(&request)?;
    if request
        .headers()
        .get(header::CONTENT_TYPE)
        .is_none_or(|v| v.as_bytes() != b"image/png")
    {
        return Err(invalid());
    }
    let operation = id(&operation)?;
    let raw = bytes(request, media::MAX_INPUT_BYTES).await?;
    let original = runtime
        .owner()
        .status(&actor, operation)
        .await
        .map_err(mapped)?;
    if raw.len() != original.input.bytes as usize
        || media::Digest::of(&raw) != original.input.digest
    {
        return Err(mapped(Error::Conflict));
    }
    if matches!(
        original.state,
        media::UploadState::Rejected | media::UploadState::Expired | media::UploadState::Finalizing
    ) {
        return json_response(operation_code(&runtime, &original)?, &original);
    }
    let result = runtime.owner().put(&actor, operation, raw).await;
    operation_result(&runtime, &actor, operation, result).await
}
async fn finalize(
    Extension(runtime): Extension<Arc<Runtime>>,
    RequireOwner(actor): RequireOwner,
    Path(operation): Path<String>,
    request: Request,
) -> ApiResult<Response> {
    let _permit = slot(&runtime.uploads)?;
    no_query(&request)?;
    json_mime(request.headers())?;
    let operation = id(&operation)?;
    media::Empty::decode(&bytes(request, media::MAX_JSON_BYTES).await?).map_err(|_| invalid())?;
    let original = runtime
        .owner()
        .status(&actor, operation)
        .await
        .map_err(mapped)?;
    if matches!(
        original.state,
        media::UploadState::Rejected | media::UploadState::Expired
    ) || (original.state != media::UploadState::Retained
        && now(&runtime.owner.state).map_err(mapped)? >= original.upload_expires_at_ms)
    {
        return json_response(operation_code(&runtime, &original)?, &original);
    }
    if original.state == media::UploadState::Reserved {
        return Err(mapped(Error::Conflict));
    }
    let result = runtime.owner().finalize(&actor, operation).await;
    operation_result(&runtime, &actor, operation, result).await
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct VariantQuery {
    variant: Variant,
}
#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Variant {
    Preview,
    Render,
}
fn typed_failure(code: StatusCode, error: media::ContentError, message: &str) -> Response {
    let failure = media::ContentFailure {
        contract: media::Contract::HumanResultMedia1,
        error,
        message: message.into(),
    };
    json_response(code, &failure).unwrap_or_else(IntoResponse::into_response)
}
async fn content(
    Extension(runtime): Extension<Arc<Runtime>>,
    RequireOwner(actor): RequireOwner,
    Path((report, rev, artifact)): Path<(String, String, String)>,
    request: Request,
) -> ApiResult<Response> {
    let permit = slot(&runtime.content)?;
    let Query(query) = Query::<VariantQuery>::try_from_uri(request.uri()).map_err(|_| invalid())?;
    // Authorization resolves before unsupported-header checks. No object data
    // is disclosed by these generic request-shape failures.
    if request.headers().contains_key(header::RANGE) {
        return Ok(StatusCode::RANGE_NOT_SATISFIABLE.into_response());
    }
    if [
        header::IF_MATCH,
        header::IF_NONE_MATCH,
        header::IF_MODIFIED_SINCE,
        header::IF_UNMODIFIED_SINCE,
        header::IF_RANGE,
    ]
    .iter()
    .any(|name| request.headers().contains_key(name))
    {
        return Err(invalid());
    }
    empty(request).await?;
    let report = id(&report)?;
    let artifact = id(&artifact)?;
    revision(&rev)?;
    let content = runtime
        .owner()
        .content(
            &actor,
            report,
            &rev,
            artifact,
            matches!(query.variant, Variant::Preview),
        )
        .await;
    match content {
        Ok(content) => Ok(body::response(content, permit)),
        Err(Error::Expired) => Ok(typed_failure(
            StatusCode::GONE,
            media::ContentError::MediaExpired,
            "Media retention expired",
        )),
        Err(Error::Withheld) => Ok(typed_failure(
            StatusCode::GONE,
            media::ContentError::MediaWithheld,
            "Media was withheld",
        )),
        Err(Error::Missing) => Ok(typed_failure(
            StatusCode::NOT_FOUND,
            media::ContentError::MediaMissing,
            "Retained media file missing",
        )),
        Err(Error::Integrity | Error::Storage) => Ok(typed_failure(
            StatusCode::CONFLICT,
            media::ContentError::MediaFailed,
            "Retained media unavailable",
        )),
        Err(error) => Err(mapped(error)),
    }
}
