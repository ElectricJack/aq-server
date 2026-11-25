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
    } else {
        routes = routes.merge(unavailable_routes(true));
    }
    routes.layer(Extension(runtime))
}
/// Keep capability absence observable through the actual protected write paths.
pub fn unavailable_routes(configured: bool) -> Router<AppState> {
    let refused = move |RequireOwner(_): RequireOwner| async move {
        crate::dashboard_api::errors::ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            if configured {
                "media_upload_disabled"
            } else {
                "media_store_unconfigured"
            },
            if configured {
                "Result media uploads are disabled"
            } else {
                "Result media storage is not configured"
            },
        )
    };
    Router::new()
        .route(
            "/api/human/results/{report}/revisions/{revision}/media-uploads",
            post(refused),
        )
        .route(
            "/api/human/result-media-uploads/{operation}/content",
            put(refused),
        )
        .route(
            "/api/human/result-media-uploads/{operation}/finalize",
            post(refused),
        )
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
    let kind = match request
        .headers()
        .get(header::CONTENT_TYPE)
        .map(|v| v.as_bytes())
    {
        Some(b"image/png") => media::MediaKind::Image,
        Some(b"video/mp4") => media::MediaKind::Video,
        _ => return Err(invalid()),
    };
    let operation = id(&operation)?;
    let raw = bytes(request, kind.max_input_bytes() as usize).await?;
    let original = runtime
        .owner()
        .status(&actor, operation)
        .await
        .map_err(mapped)?;
    if original.media != kind {
        return Err(invalid());
    }
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
    let range = request
        .headers()
        .get(header::RANGE)
        .map(|value| value.to_str().map(str::to_owned).unwrap_or_default());
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
        Ok(content) => Ok(content_response(content, permit, range.as_deref())),
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

fn content_response(
    content: super::Content,
    permit: OwnedSemaphorePermit,
    range: Option<&str>,
) -> Response {
    let total = content.bytes().len();
    let Some(range) = range else {
        return body::response(content, permit, None);
    };
    // Only a clip render is seekable; images keep the 416 contract.
    let clip = content.descriptor.mime == media::Mime::Mp4;
    match byte_range(range, total).filter(|_| clip) {
        Some(span) => body::response(content, permit, Some(span)),
        None => {
            let response = (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{total}"))],
            )
                .into_response();
            // Release the reader before waking the next admitted request.
            drop(content);
            drop(permit);
            response
        }
    }
}

/// One `bytes=` range (`a-b`, `a-` or `-n`) as a non-empty half-open span.
fn byte_range(value: &str, total: usize) -> Option<std::ops::Range<usize>> {
    let spec = value.strip_prefix("bytes=")?;
    let (first, last) = spec.split_once('-')?;
    let digits = |v: &str| {
        (!v.is_empty() && v.len() <= 19 && v.bytes().all(|b| b.is_ascii_digit()))
            .then(|| v.parse::<u64>().ok())
            .flatten()
    };
    let total64 = total as u64;
    let (start, end) = match (first.is_empty(), last.is_empty()) {
        (true, false) => {
            let n = digits(last)?.min(total64);
            (total64 - n, total64)
        }
        (false, true) => (digits(first)?, total64),
        (false, false) => {
            let (a, b) = (digits(first)?, digits(last)?);
            if b < a {
                return None;
            }
            (a, b.saturating_add(1).min(total64))
        }
        (true, true) => return None,
    };
    (start < end && start < total64).then_some(start as usize..end as usize)
}
#[cfg(test)]
mod range_tests {
    use super::*;

    /// `files::Root::open` requires an owner-only (0700) directory, which
    /// `tempfile::tempdir()` does not guarantee under a 022 umask.
    fn private_media_dir() -> tempfile::TempDir {
        use std::os::unix::fs::PermissionsExt;
        tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap()
    }

    fn content(
        root: &Arc<super::super::files::Root>,
        artifact: Uuid,
        mime: media::Mime,
    ) -> super::super::Content {
        let bytes = b"0123456789".to_vec();
        let digest = media::Digest::of(&bytes);
        super::super::Content {
            descriptor: media::Variant {
                digest: digest.clone(),
                bytes: bytes.len() as u64,
                mime,
                width: 1,
                height: 1,
            },
            receipt_digest: digest,
            bytes,
            _guard: root.active(artifact, false).unwrap(),
        }
    }

    #[test]
    fn single_byte_ranges_parse_and_clamp() {
        assert_eq!(byte_range("bytes=0-9", 100), Some(0..10));
        assert_eq!(byte_range("bytes=90-", 100), Some(90..100));
        assert_eq!(byte_range("bytes=-10", 100), Some(90..100));
        assert_eq!(byte_range("bytes=50-500", 100), Some(50..100));
        for bad in [
            "bytes=100-",
            "bytes=-0",
            "bytes=5-4",
            "bytes=0-1,3-4",
            "items=0-1",
            "bytes=-",
            "bytes= 0-1",
        ] {
            assert_eq!(byte_range(bad, 100), None, "{bad}");
        }
    }

    #[tokio::test]
    async fn content_ranges_keep_reader_and_admission_guards_until_frames_drop() {
        use http_body_util::BodyExt;
        let directory = private_media_dir();
        let root = super::super::files::Root::open(directory.path(), Uuid::new_v4()).unwrap();
        let artifact = Uuid::new_v4();
        for (mime, range, status, expected) in [
            (media::Mime::Png, None, StatusCode::OK, &b"0123456789"[..]),
            (
                media::Mime::Mp4,
                Some("bytes=2-4"),
                StatusCode::PARTIAL_CONTENT,
                &b"234"[..],
            ),
            (
                media::Mime::Png,
                Some("bytes=2-4"),
                StatusCode::RANGE_NOT_SATISFIABLE,
                &b""[..],
            ),
            (
                media::Mime::Mp4,
                Some("bytes=10-"),
                StatusCode::RANGE_NOT_SATISFIABLE,
                &b""[..],
            ),
        ] {
            let budget = Arc::new(Semaphore::new(1));
            let response = content_response(
                content(&root, artifact, mime),
                slot(&budget).unwrap(),
                range,
            );
            assert_eq!(response.status(), status);
            if status == StatusCode::RANGE_NOT_SATISFIABLE {
                assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes */10");
                assert_eq!(budget.available_permits(), 1);
                assert!(root.active(artifact, true).is_ok());
                continue;
            }
            if status == StatusCode::PARTIAL_CONTENT {
                assert_eq!(response.headers()[header::CONTENT_RANGE], "bytes 2-4/10");
            }
            let mut body = response.into_body();
            let frame = body.frame().await.unwrap().unwrap();
            assert_eq!(frame.data_ref().unwrap().as_ref(), expected);
            drop(body);
            assert_eq!(budget.available_permits(), 0);
            assert!(root.active(artifact, true).is_err());
            drop(frame);
            assert_eq!(budget.available_permits(), 1);
            assert!(root.active(artifact, true).is_ok());
        }
    }

    #[test]
    fn dropping_unread_content_releases_reader_and_admission_guards() {
        let directory = private_media_dir();
        let root = super::super::files::Root::open(directory.path(), Uuid::new_v4()).unwrap();
        let artifact = Uuid::new_v4();
        let budget = Arc::new(Semaphore::new(1));
        for range in [None, Some("bytes=2-4")] {
            let response = content_response(
                content(&root, artifact, media::Mime::Mp4),
                slot(&budget).unwrap(),
                range,
            );
            assert_eq!(budget.available_permits(), 0);
            assert!(root.active(artifact, true).is_err());
            drop(response);
            assert_eq!(budget.available_permits(), 1);
            assert!(root.active(artifact, true).is_ok());
        }
    }
}
