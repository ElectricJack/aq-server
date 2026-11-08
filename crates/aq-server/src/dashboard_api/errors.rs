//! The human v1 boundary deliberately never reflects internal error messages.
use axum::{
    Json,
    extract::Request,
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ErrorEnvelope {
    pub request_id: Uuid,
    #[schemars(length(max = 64))]
    pub code: String,
    #[schemars(length(max = 256))]
    pub message: String,
    pub retryable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field_errors: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_versions: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u32>,
}
#[derive(Debug)]
pub struct ApiError {
    status: StatusCode,
    body: Box<ErrorEnvelope>,
    source: Option<crate::error::RefusalContext>,
}
#[derive(Clone)]
struct Normalized;
impl ApiError {
    pub fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            source: None,
            body: Box::new(ErrorEnvelope {
                request_id: Uuid::new_v4(),
                code: code.chars().filter(|c| !c.is_control()).take(64).collect(),
                message: message
                    .chars()
                    .filter(|c| !c.is_control())
                    .take(256)
                    .collect(),
                retryable: matches!(status.as_u16(), 429 | 503),
                operation_id: None,
                field_errors: None,
                current_versions: None,
                retry_after_ms: None,
            }),
        }
    }
    pub fn for_status(status: StatusCode) -> Self {
        let (status, code, message) = match status.as_u16() {
            401 => (status, "unauthenticated", "Sign in to AQ"),
            403 => (
                status,
                "forbidden",
                "This request is not available to your account",
            ),
            404 => (status, "not_found", "Resource not found"),
            409 => (
                status,
                "operation_conflict",
                "Operation identity or versions conflict",
            ),
            410 => (status, "expired", "This resource expired"),
            413 => (
                status,
                "request_too_large",
                "Request exceeds the size limit",
            ),
            426 => (
                status,
                "unsupported_contract",
                "This required contract is unsupported",
            ),
            429 => (
                status,
                "capacity_exceeded",
                "Request capacity is temporarily exhausted",
            ),
            500..=599 => (
                StatusCode::SERVICE_UNAVAILABLE,
                "unavailable",
                "This request is temporarily unavailable",
            ),
            _ => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "invalid_request",
                "Invalid request",
            ),
        };
        Self::new(status, code, message)
    }
    pub fn operation(mut self, id: Uuid) -> Self {
        self.body.operation_id = Some(id);
        self
    }
    pub fn status(&self) -> StatusCode {
        self.status
    }
    pub fn unavailable() -> Self {
        Self::for_status(StatusCode::SERVICE_UNAVAILABLE)
    }
    pub fn invalid() -> Self {
        Self::for_status(StatusCode::UNPROCESSABLE_ENTITY)
    }
}
impl From<crate::error::AppError> for ApiError {
    fn from(value: crate::error::AppError) -> Self {
        if matches!(value, crate::error::AppError::InputDeadline) {
            return Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "input_deadline",
                "Input deadline must be 1 to 2000 ms after server issue time; set expires_at_ms using aq-terminal-server-time-ms",
            );
        }
        Self::for_status(value.status())
    }
}
impl From<aq_swarm::TimeError> for super::Error {
    fn from(_: aq_swarm::TimeError) -> Self {
        Self::Unavailable
    }
}
impl From<sqlx::Error> for ApiError {
    fn from(_: sqlx::Error) -> Self {
        Self::unavailable()
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        // Only fixed public diagnostics: never log bodies, serde errors or
        // internal error messages. The ID is the one returned to the caller.
        tracing::warn!(request_id=%self.body.request_id, status=self.status.as_u16(), code=%self.body.code, source_status=self.source.as_ref().map(|s|s.status), source_code=self.source.as_ref().map(|s|s.code.as_str()), "human API request refused");
        let mut response = (self.status, Json(self.body)).into_response();
        response.extensions_mut().insert(Normalized);
        response
    }
}
/// Must wrap the *outer* human middleware so CSRF, admission and framework
/// refusals obey the v1 contract too. Other API families retain their contract.
pub async fn normalize_errors(mut req: Request, next: Next) -> Response {
    let v1 = req.uri().path().starts_with("/api/v1/");
    let receipt = super::actions::AdmissionReceipt::default();
    if req.uri().path() == "/api/v1/actions" && req.method() == axum::http::Method::POST {
        req.extensions_mut().insert(receipt.clone());
    }
    let response = next.run(req).await;
    if response.status().is_server_error()
        && let Some(retained) = receipt.retained_response().await
    {
        return preserve_protocol_headers(&response, retained);
    }
    if v1
        && (response.status().is_client_error() || response.status().is_server_error())
        && response.extensions().get::<Normalized>().is_none()
    {
        let mut error = ApiError::for_status(response.status());
        error.source = response
            .extensions()
            .get::<crate::error::RefusalContext>()
            .cloned();
        preserve_protocol_headers(&response, error.into_response())
    } else {
        response
    }
}
fn preserve_protocol_headers(original: &Response, mut normalized: Response) -> Response {
    for name in ["retry-after", "aq-terminal-server-time-ms", "cache-control"] {
        if let Some(value) = original.headers().get(name) {
            normalized.headers_mut().insert(name, value.clone());
        }
    }
    normalized
}
