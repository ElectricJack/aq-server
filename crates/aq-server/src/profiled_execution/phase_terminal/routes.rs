//! Bounded worker transport. Routes convey reports/history, never launch authority.
use crate::{AppState, error::AppError, worker_api};
use aq_phase_terminal_protocol::{self as wire, refresh_v1 as refresh_wire};
use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Request, State},
    http::HeaderMap,
    response::IntoResponse,
    routing::post,
};
use tokio::sync::Semaphore;
static WORK: Semaphore = Semaphore::const_new(2);
static CLEANUP: Semaphore = Semaphore::const_new(2);
const VERSION: &str = "aq-phase-terminal-api-version";

pub fn routes(state: AppState) -> Router<AppState> {
    Router::new()
        .route("/v1/worker/profiled-v2/started", post(started))
        .route("/v1/worker/profiled-v2/history", post(history))
        .with_state(state.clone())
        .merge(
            Router::new()
                .route("/v1/worker/profiled-v2/stopped", post(stopped))
                .route("/v1/worker/profiled-v2/launch-closure", post(closure))
                .with_state(state.control_view()),
        )
        .layer(axum::middleware::map_response(
            |mut r: axum::response::Response| async move {
                r.headers_mut().insert(VERSION, "1".parse().unwrap());
                r.headers_mut().insert(
                    axum::http::header::CACHE_CONTROL,
                    "no-store".parse().unwrap(),
                );
                r
            },
        ))
}
fn check_headers(headers: &HeaderMap) -> Result<(), AppError> {
    for (name, expected) in [(VERSION, "1"), ("content-type", "application/json")] {
        let mut values = headers.get_all(name).iter();
        if values.next().is_none_or(|v| v != expected) || values.next().is_some() {
            return Err(AppError::BadRequest(
                "exact phase terminal version/type required".into(),
            ));
        }
    }
    Ok(())
}
fn profile(bytes: &[u8]) -> Result<wire::Profile, AppError> {
    if bytes.len() > wire::MAX_BYTES {
        return Err(AppError::PayloadTooLarge("phase terminal body".into()));
    }
    // The probe chooses a nominal decoder; every authoritative decoder below
    // still receives the original bytes, preserving duplicate/unknown refusals.
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|_| AppError::BadRequest("invalid phase terminal request".into()))?;
    let contract: aq_admission::ArtifactRef = serde_json::from_value(
        value
            .get("negotiation")
            .and_then(|v| v.get("contract"))
            .cloned()
            .ok_or_else(|| AppError::BadRequest("invalid phase terminal request".into()))?,
    )
    .map_err(|_| AppError::BadRequest("invalid phase terminal request".into()))?;
    wire::Profile::for_contract(&contract)
        .map_err(|_| AppError::BadRequest("invalid phase terminal request".into()))
}
macro_rules! endpoint {
    ($name:ident, $lane:ident, $decode:ident, $receipt:ident, $owner:ident, $method:ident, $($binding:ident).+) => {
        async fn $name(
            State(state): State<AppState>,
            request: Request,
        ) -> Result<axum::response::Response, AppError> {
            let _permit = $lane
                .try_acquire()
                .map_err(|_| AppError::Maintenance("phase terminal ingress full".into()))?;
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                check_headers(request.headers())?;
                let node = match stringify!($name) {
                    "started" => {
                        crate::delegated_permits::runtime::authenticate_runner(
                            &state,
                            request.headers(),
                            aq_admission_sandbox::sandbox::RunnerOperation::Started,
                            None,
                        )
                        .await?
                    }
                    "stopped" => {
                        crate::delegated_permits::runtime::authenticate_runner(
                            &state,
                            request.headers(),
                            aq_admission_sandbox::sandbox::RunnerOperation::ReportResult,
                            None,
                        )
                        .await?
                    }
                    _ => worker_api::authenticate_context(&state, request.headers()).await?,
                };
                let bytes = to_bytes(request.into_body(), wire::MAX_BYTES)
                    .await
                    .map_err(|_| AppError::PayloadTooLarge("phase terminal body".into()))?;
                match profile(&bytes)? {
                    wire::Profile::FrozenV1 => {
                        let q = wire::$decode(&bytes).map_err(|_| {
                            AppError::BadRequest("invalid phase terminal request".into())
                        })?;
                        crate::delegated_permits::runtime::require_binding(&node, &q.$($binding).+)?;
                        super::$owner::$method(&state, &node, &q)
                            .await
                            .map(|r| Json(r).into_response())
                    }
                    wire::Profile::RegistryRefreshV1 => {
                        let q = refresh_wire::$decode(&bytes).map_err(|_| {
                            AppError::BadRequest("invalid phase terminal request".into())
                        })?;
                        crate::delegated_permits::runtime::require_binding(&node, &q.$($binding).+)?;
                        super::super::phase_refresh_terminal::$owner::$method(&state, &node, &q)
                            .await
                            .map(|r| Json(r).into_response())
                    }
                }
            })
            .await
            .map_err(|_| {
                AppError::Maintenance("phase terminal deadline exceeded; outcome unknown".into())
            })?
        }
    };
}
endpoint!(
    started,
    WORK,
    decode_started_request,
    StartedReceipt,
    started,
    record,
    original.binding
);
endpoint!(
    history,
    WORK,
    decode_history_request,
    HistoryReceipt,
    history,
    read,
    binding
);
endpoint!(
    stopped,
    CLEANUP,
    decode_stopped_request,
    StoppedReceipt,
    stopped,
    record,
    original.evidence.binding
);
endpoint!(
    closure,
    CLEANUP,
    decode_closure_request,
    ClosureReceipt,
    closure,
    record,
    original.binding
);

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn version_and_content_type_must_be_single_and_exact() {
        let mut headers = HeaderMap::new();
        assert!(check_headers(&headers).is_err());
        headers.insert(VERSION, "1".parse().unwrap());
        headers.insert("content-type", "application/json".parse().unwrap());
        check_headers(&headers).unwrap();
        headers.append(VERSION, "1".parse().unwrap());
        assert!(check_headers(&headers).is_err());
        headers.remove(VERSION);
        headers.insert(VERSION, "2".parse().unwrap());
        assert!(check_headers(&headers).is_err());
        headers.insert(VERSION, "1".parse().unwrap());
        headers.append("content-type", "application/json".parse().unwrap());
        assert!(check_headers(&headers).is_err());
    }
}
