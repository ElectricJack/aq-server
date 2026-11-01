//! Native control on the machine listener, separate from legacy fixture wire.
use crate::{AppState, error::AppError};
use aq_admission::{
    control::{ControlReply, Failure, FailureCode},
    transport::{Accept, Prepare},
};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State, rejection::BytesRejection},
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use uuid::Uuid;
pub(crate) struct Error(AppError);
impl From<AppError> for Error {
    fn from(value: AppError) -> Self {
        Self(value)
    }
}
impl IntoResponse for Error {
    fn into_response(self) -> Response {
        #[cfg(feature = "test-support")]
        let classification = crate::test_support::control_diagnostics::classify(&self.0);
        let status = self.0.status();
        let code = match status {
            StatusCode::UNAUTHORIZED => FailureCode::AuthenticationRequired,
            StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => FailureCode::UnknownOrNotOwned,
            StatusCode::BAD_REQUEST => FailureCode::InvalidRequest,
            StatusCode::PAYLOAD_TOO_LARGE | StatusCode::TOO_MANY_REQUESTS => FailureCode::Capacity,
            StatusCode::CONFLICT => FailureCode::Conflict,
            _ => FailureCode::Unavailable,
        };
        let mut reply = (
            status,
            Json(ControlReply::Failure(Failure {
                schema_version: 1,
                code,
                operation_id: None,
            })),
        )
            .into_response();
        reply
            .headers_mut()
            .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
        // In-process test observation only. Extensions are never HTTP headers
        // or body fields; the public failure remains deliberately redacted.
        #[cfg(feature = "test-support")]
        if let Some(classification) = classification {
            reply.extensions_mut().insert(classification);
        }
        reply
    }
}
pub(crate) fn nonce(headers: &HeaderMap) -> Result<Uuid, AppError> {
    if headers.get_all("aq-delivery-nonce").iter().count() != 1 {
        return Err(AppError::BadRequest(
            "one physical delivery nonce required".into(),
        ));
    }
    headers
        .get("aq-delivery-nonce")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| Uuid::parse_str(v).ok())
        .filter(|v| !v.is_nil())
        .ok_or_else(|| AppError::BadRequest("invalid physical delivery nonce".into()))
}
pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route(
            "/v1/worker/profiled/accept",
            post(accept).layer(DefaultBodyLimit::max(65_536)),
        )
        .route(
            "/v1/worker/profiled/prepare",
            post(prepare).layer(DefaultBodyLimit::max(
                aq_admission::transport::MAX_FRAME_BYTES,
            )),
        )
        .with_state(state.control_view())
}
async fn accept(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Error> {
    let nonce = nonce(&headers)?;
    let auth = crate::worker_api::authenticate_context(&state, &headers).await?;
    let body = body.map_err(|_| AppError::PayloadTooLarge("native control body bound".into()))?;
    let request: Accept = serde_json::from_slice(&body)
        .map_err(|_| AppError::BadRequest("invalid native acceptance document".into()))?;
    let reply = super::preparation::accept(&state, &auth, &request, nonce).await?;
    let mut response = Json(ControlReply::Preparation(reply)).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    Ok(response)
}

async fn prepare(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, Error> {
    let nonce = nonce(&headers)?;
    let auth = crate::worker_api::authenticate_context(&state, &headers).await?;
    let body = body.map_err(|_| AppError::PayloadTooLarge("native control body bound".into()))?;
    let request: Prepare = serde_json::from_slice(&body)
        .map_err(|_| AppError::BadRequest("invalid native preparation document".into()))?;
    let reply = super::initial_grant::prepare(&state, &auth, &request, nonce).await?;
    let mut response = Json(ControlReply::InitialGrant(reply)).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    Ok(response)
}

#[cfg(all(test, feature = "test-support"))]
mod diagnostic_tests {
    use super::*;
    use crate::test_support::control_diagnostics::ConflictClass;

    #[tokio::test]
    async fn diagnostic_extension_preserves_the_exact_redacted_wire_failure() {
        let response = Error(AppError::Conflict(
            "selection expired during admission locks".into(),
        ))
        .into_response();
        assert_eq!(response.status(), StatusCode::CONFLICT);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            response.extensions().get::<ConflictClass>(),
            Some(&ConflictClass::SelectionExpired)
        );
        let bytes = axum::body::to_bytes(response.into_body(), 65536)
            .await
            .unwrap();
        let expected = serde_json::to_vec(&ControlReply::Failure(Failure {
            schema_version: 1,
            code: FailureCode::Conflict,
            operation_id: None,
        }))
        .unwrap();
        assert_eq!(&bytes[..], expected);
        assert!(!String::from_utf8_lossy(&bytes).contains("expired"));
    }
}
