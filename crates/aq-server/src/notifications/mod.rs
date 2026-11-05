//! Central notification service; isolated transport never holds control capacity.
pub mod api;
pub mod config;
pub mod persistence;
pub mod policy;
pub mod runtime;
pub mod sources;
use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("invalid_notification_request")]
    Invalid,
    #[error("notification_not_found")]
    NotFound,
    #[error("notification_scope_refused")]
    Forbidden,
    #[error("notification_conflict")]
    Conflict,
    #[error("notification_dispatcher_fenced")]
    DispatcherFenced,
    #[error("notification_capacity_exhausted")]
    Overloaded,
    #[error("notification_storage_unavailable")]
    Unavailable,
}
pub type Result<T> = std::result::Result<T, Error>;
impl From<sqlx::Error> for Error {
    fn from(_: sqlx::Error) -> Self {
        Self::Unavailable
    }
}
impl From<serde_json::Error> for Error {
    fn from(_: serde_json::Error) -> Self {
        Self::Invalid
    }
}
impl From<aq_notifications::Error> for Error {
    fn from(error: aq_notifications::Error) -> Self {
        match error {
            aq_notifications::Error::Invalid => Self::Invalid,
            aq_notifications::Error::ScopeChanged => Self::Forbidden,
            aq_notifications::Error::Conflict
            | aq_notifications::Error::NotDue
            | aq_notifications::Error::Exhausted => Self::Conflict,
        }
    }
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        let code = match self {
            Self::Invalid => StatusCode::BAD_REQUEST,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::Conflict | Self::DispatcherFenced => StatusCode::CONFLICT,
            Self::Overloaded => StatusCode::TOO_MANY_REQUESTS,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        };
        (
            code,
            Json(serde_json::json!({"code":self.to_string(),"retryable":false})),
        )
            .into_response()
    }
}
