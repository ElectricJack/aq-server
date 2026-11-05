//! API error type shared by all routes: a status code plus an `ApiError` body.

use aq_protocol::{
    ApiError, BAD_REQUEST, CONFLICT, CREDENTIAL_REVOKED, CSRF_REJECTED, CompatibilityError,
    ENROLLMENT_INVALID, FORBIDDEN, IDENTITY_UNAVAILABLE, INTERNAL, INVALID_CREDENTIAL,
    INVALID_VERSION, MAINTENANCE, NOT_FOUND, UNAUTHENTICATED, UPDATE_IN_PROGRESS,
    UPDATES_UNAVAILABLE,
};
use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};

use crate::identity::IdentityError;
use crate::store::StoreError;

/// Classification only; never contains an internal message or request data.
#[derive(Clone, Debug)]
pub(crate) struct RefusalContext {
    pub status: u16,
    pub code: String,
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
    Internal,
}

impl AppError {
    pub fn status(&self) -> StatusCode {
        match self {
            AppError::Unauthenticated(_) => StatusCode::UNAUTHORIZED,
            AppError::Forbidden(_) | AppError::CsrfRejected(_) => StatusCode::FORBIDDEN,
            AppError::IdentityUnavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
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
            AppError::Maintenance(_) | AppError::UpdatesUnavailable(_) => {
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
        tracing::warn!(status=context.status, code=%context.code, "API request refused");
        let mut response = (self.status(), Json(body)).into_response();
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

impl From<StoreError> for AppError {
    fn from(e: StoreError) -> Self {
        if matches!(e, StoreError::EmailTaken) {
            return AppError::Conflict("a user with that email already exists".to_string());
        }
        tracing::error!(error = %e, "store error");
        AppError::Internal
    }
}
