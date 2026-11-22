//! API error type shared by all routes: a status code plus an `ApiError` body.

use aq_protocol::{
    ApiError, BAD_REQUEST, CONFLICT, CREDENTIAL_REVOKED, CSRF_REJECTED, CompatibilityError,
    ENROLLMENT_INVALID, FORBIDDEN, IDENTITY_UNAVAILABLE, INTERNAL, INVALID_CREDENTIAL,
    INVALID_VERSION, MAINTENANCE, NOT_FOUND, UNAUTHENTICATED, UPDATE_IN_PROGRESS,
    UPDATES_UNAVAILABLE,
};
use axum::Json;
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};

use crate::identity::IdentityError;
use crate::store::StoreError;

/// Classification only; never contains an internal message or request data.
#[derive(Clone, Debug)]
pub(crate) struct RefusalContext {
    pub status: u16,
    pub code: String,
}

/// Where a request is going, for the refusal log: its method and the matched
/// route template (`/api/human/conversations/{conversation}/messages`). Never
/// the raw path or the query string, which can carry ids and cursors.
#[derive(Clone, Debug)]
struct RequestSite {
    method: axum::http::Method,
    route: String,
}
tokio::task_local! {
    static REQUEST_SITE: RequestSite;
}
/// Middleware: make the request's method and matched route template known to
/// [`AppError`]'s refusal log for the rest of the request. A request that
/// matched no route is logged as `(unmatched)`.
pub(crate) async fn record_request_site(
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let route = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map_or_else(|| "(unmatched)".to_owned(), |p| p.as_str().to_owned());
    let site = RequestSite {
        method: req.method().clone(),
        route,
    };
    REQUEST_SITE.scope(site, next.run(req)).await
}

#[derive(Debug)]
pub enum AppError {
    Unauthenticated(String),
    Forbidden(String),
    CsrfRejected(String),
    IdentityUnavailable(String),
    NotFound(String),
    BadRequest(String),
    InputDeadline,
    Conflict(String),
    DefinitionIdentityConflict,
    RepositoryAdoptionUnavailable(&'static str),
    /// Fixed endpoint diagnostic; never includes database or terminal payloads.
    TerminalRelayFailure(&'static str),
    ExecutionCancelled,
    PayloadTooLarge(String),
    /// 426 with a `CompatibilityError` body.
    UnsupportedProtocol(CompatibilityError),
    InvalidCredential(String),
    CredentialRevoked,
    /// Unknown, expired or used code: deliberately indistinguishable.
    EnrollmentInvalid,
    UpdateInProgress(String),
    Maintenance(String),
    UpdatesUnavailable(String),
    InvalidVersion(String),
    /// A row lock timed out or deadlocked (55P03, 40P01, 40001): 503 with `Retry-After`.
    Busy,
    Internal,
}

impl AppError {
    pub fn status(&self) -> StatusCode {
        match self {
            AppError::Unauthenticated(_) => StatusCode::UNAUTHORIZED,
            AppError::Forbidden(_) | AppError::CsrfRejected(_) => StatusCode::FORBIDDEN,
            AppError::IdentityUnavailable(_) | AppError::RepositoryAdoptionUnavailable(_) => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::BadRequest(_) => StatusCode::BAD_REQUEST,
            AppError::InputDeadline => StatusCode::UNPROCESSABLE_ENTITY,
            AppError::Conflict(_)
            | AppError::ExecutionCancelled
            | AppError::DefinitionIdentityConflict => StatusCode::CONFLICT,
            AppError::PayloadTooLarge(_) => StatusCode::PAYLOAD_TOO_LARGE,
            AppError::UnsupportedProtocol(_) => StatusCode::UPGRADE_REQUIRED,
            AppError::InvalidCredential(_) | AppError::CredentialRevoked => {
                StatusCode::UNAUTHORIZED
            }
            AppError::EnrollmentInvalid => StatusCode::BAD_REQUEST,
            AppError::UpdateInProgress(_) => StatusCode::CONFLICT,
            AppError::Maintenance(_) | AppError::UpdatesUnavailable(_) | AppError::Busy => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            AppError::InvalidVersion(_) => StatusCode::BAD_REQUEST,
            AppError::Internal | AppError::TerminalRelayFailure(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }

    pub fn to_api_error(&self) -> ApiError {
        let (code, message) = match self {
            AppError::Unauthenticated(m) => (UNAUTHENTICATED, m.clone()),
            AppError::Forbidden(m) => (FORBIDDEN, m.clone()),
            AppError::CsrfRejected(m) => (CSRF_REJECTED, m.clone()),
            AppError::IdentityUnavailable(m) => (IDENTITY_UNAVAILABLE, m.clone()),
            AppError::NotFound(m) => (NOT_FOUND, m.clone()),
            AppError::BadRequest(m) => (BAD_REQUEST, m.clone()),
            AppError::InputDeadline => (
                "input_deadline",
                aq_terminal_relay::Error::InputDeadline.to_string(),
            ),
            AppError::Conflict(m) => (CONFLICT, m.clone()),
            AppError::DefinitionIdentityConflict => (
                "admission_definition_identity_conflict",
                "Definition identity/version already retains different bytes; publish a new definition version".into(),
            ),
            AppError::TerminalRelayFailure(code) => (
                *code,
                "Terminal relay failed; retry the request and inspect server diagnostics if it persists".into(),
            ),
            AppError::RepositoryAdoptionUnavailable(source) => (
                "repository_adoption_unavailable",
                format!("{source} is unavailable"),
            ),
            AppError::ExecutionCancelled => (CONFLICT, "execution cancelled".into()),
            AppError::PayloadTooLarge(m) => (BAD_REQUEST, m.clone()),
            AppError::UnsupportedProtocol(c) => (c.error.as_str(), c.message.clone()),
            AppError::InvalidCredential(m) => (INVALID_CREDENTIAL, m.clone()),
            AppError::CredentialRevoked => (
                CREDENTIAL_REVOKED,
                "this worker credential has been revoked; enroll the worker again".to_string(),
            ),
            AppError::EnrollmentInvalid => (
                ENROLLMENT_INVALID,
                "enrollment code is invalid, expired or already used".to_string(),
            ),
            AppError::UpdateInProgress(m) => (UPDATE_IN_PROGRESS, m.clone()),
            AppError::Maintenance(m) => (MAINTENANCE, m.clone()),
            AppError::UpdatesUnavailable(m) => (UPDATES_UNAVAILABLE, m.clone()),
            AppError::InvalidVersion(m) => (INVALID_VERSION, m.clone()),
            AppError::Busy => ("overloaded", "row lock contended; retry the same operation".into()),
            AppError::Internal => (INTERNAL, "internal server error".to_string()),
        };
        ApiError {
            error: code.to_string(),
            message,
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        if matches!(self, AppError::InputDeadline) {
            return crate::dashboard_api::errors::ApiError::from(self).into_response();
        }
        if let AppError::UnsupportedProtocol(c) = &self {
            return (self.status(), Json(c.clone())).into_response();
        }
        let body = self.to_api_error();
        let context = RefusalContext {
            status: self.status().as_u16(),
            code: body.error.clone(),
        };
        let (method, route) = REQUEST_SITE
            .try_with(|site| (site.method.to_string(), site.route.clone()))
            .unwrap_or_else(|_| ("(unknown)".into(), "(unknown)".into()));
        tracing::warn!(%method, %route, status=context.status, code=%context.code, message=[body.message.as_str(), ""][(context.status != 409) as usize], "API request refused"); // only a conflict's text: others may repeat the caller's input
        let mut response = (self.status(), Json(body)).into_response();
        if matches!(self, AppError::Busy) {
            (response.headers_mut()).insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        }
        response.extensions_mut().insert(context);
        response
    }
}

impl From<IdentityError> for AppError {
    fn from(e: IdentityError) -> Self {
        match e {
            IdentityError::Missing => {
                AppError::Unauthenticated("authentication required".to_string())
            }
            IdentityError::Invalid(reason) => {
                AppError::Unauthenticated(format!("invalid credential: {reason}"))
            }
            IdentityError::KeysUnavailable(reason) => {
                tracing::error!(%reason, "identity signing keys unavailable");
                AppError::IdentityUnavailable(
                    "identity provider signing keys are temporarily unavailable".to_string(),
                )
            }
        }
    }
}

/// A database failure: `Busy` when it is row-lock contention, else `Internal`.
pub fn db(e: sqlx::Error) -> AppError {
    if aq_machine_control::is_contention(&e) {
        return AppError::Busy;
    }
    tracing::warn!(error = %e, "database error");
    AppError::Internal
}
impl From<aq_login_store::Error> for AppError {
    fn from(error: aq_login_store::Error) -> Self {
        match error {
            aq_login_store::Error::Db(e) => db(e),
            aq_login_store::Error::Store => AppError::Internal,
        }
    }
}
impl From<aq_machine_control::Error> for AppError {
    fn from(error: aq_machine_control::Error) -> Self {
        if let aq_machine_control::Error::Db(e) = error {
            return db(e);
        }
        tracing::warn!(%error, "machine control store error");
        AppError::Internal
    }
}
impl From<StoreError> for AppError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::EmailTaken => {
                AppError::Conflict("a user with that email already exists".to_string())
            }
            StoreError::Database(e) => db(e),
            e => {
                tracing::error!(error = %e, "store error");
                AppError::Internal
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, body::Body, http::Request, middleware::from_fn, routing::get};
    use std::sync::{Arc, Mutex};
    use tower::ServiceExt;
    use tracing_subscriber::fmt::MakeWriter;

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl MakeWriter<'_> for Sink {
        type Writer = Sink;
        fn make_writer(&self) -> Sink {
            self.clone()
        }
    }

    async fn refused(app: Router, uri: &str) -> String {
        let sink = Sink::default();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(sink.clone())
            .with_ansi(false)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);
        let response = app
            .oneshot(Request::get(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        String::from_utf8(sink.0.lock().unwrap().clone()).unwrap()
    }
    async fn not_found() -> Result<(), AppError> {
        Err(AppError::NotFound("missing".into()))
    }

    #[tokio::test]
    async fn a_refusal_log_names_the_method_and_route_template_but_not_the_query() {
        let app = Router::new()
            .route("/things/{thing}/messages", get(not_found))
            .layer(from_fn(record_request_site));
        let id = "6f1c2b0e-7d58-4f0a-9a43-0c2f5a1e9b77";
        let log = refused(
            app,
            &format!("/things/{id}/messages?before=41.2&token=sekrit"),
        )
        .await;
        assert!(log.contains("API request refused"), "{log}");
        assert!(log.contains("method=GET"), "{log}");
        assert!(log.contains("route=/things/{thing}/messages"), "{log}");
        for leaked in [id, "before", "41.2", "token", "sekrit", "?"] {
            assert!(!log.contains(leaked), "{leaked} in {log}");
        }
    }

    #[tokio::test]
    async fn a_refusal_without_a_matched_route_says_so() {
        let app = Router::new()
            .fallback(not_found)
            .layer(from_fn(record_request_site));
        let log = refused(app, "/nowhere/1234?cursor=9").await;
        assert!(log.contains("route=(unmatched)"), "{log}");
        assert!(!log.contains("nowhere") && !log.contains("cursor"), "{log}");
    }
}
