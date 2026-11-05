//! Exact nominal phase Prepare transport. No capability or provider is enabled by default.
use crate::{AppState, error::AppError};
use aq_admission::preparation_v2 as wire;
use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Request, State},
    response::{IntoResponse, Response},
    routing::post,
};
use tokio::sync::Semaphore;
static INGRESS: Semaphore = Semaphore::const_new(2);
/// Root worker router supplies the General view. Cold admission must not consume
/// renewal or cleanup connections; the issuer separately bounds actual owners.
pub fn routes(state: AppState) -> Router<AppState> {
    if !super::policy::transport_enabled(&state)
        && !super::super::phase_refresh_execution::policy::transport_enabled(&state)
    {
        return Router::new();
    }
    Router::new()
        .route("/v1/worker/profiled-v2/prepare", post(prepare))
        .with_state(state)
        .layer(axum::middleware::map_response(
            |mut r: axum::response::Response| async move {
                r.headers_mut().insert(
                    axum::http::header::CACHE_CONTROL,
                    "no-store".parse().unwrap(),
                );
                r
            },
        ))
}
async fn prepare(
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, crate::profiled_execution::control_api::Error> {
    let _permit = INGRESS
        .try_acquire()
        .map_err(|_| AppError::Maintenance("phase Prepare ingress full".into()))?;
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        prepare_inner(&state, request),
    )
    .await
    .map_err(|_| {
        AppError::IdentityUnavailable("phase Prepare outcome unknown after ingress deadline".into())
    })?
    .map_err(Into::into)
}
async fn prepare_inner(state: &AppState, request: Request) -> Result<Response, AppError> {
    let mut types = request.headers().get_all("content-type").iter();
    if types.next().is_none_or(|v| v != "application/json") || types.next().is_some() {
        return Err(AppError::BadRequest(
            "one exact JSON content type required".into(),
        ));
    }
    let nonce = crate::profiled_execution::control_api::nonce(request.headers())?;
    let mut auth = crate::delegated_permits::runtime::authenticate_runner(
        state,
        request.headers(),
        aq_admission_sandbox::sandbox::RunnerOperation::Prepared,
        None,
    )
    .await?;
    auth.upgrade =
        crate::delegated_permits::handoff::header(request.headers(), "aq-permit-upgrade")?;
    let bytes = to_bytes(request.into_body(), wire::MAX_BYTES)
        .await
        .map_err(|_| AppError::PayloadTooLarge("phase Prepare body".into()))?;
    let q = super::super::phase_profile::decode_prepare(&bytes)?;
    // An already-open historical profile cannot issue production grants without
    // the separately reserved terminal capacity installed by migration 33.
    let ready: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM meta WHERE key='state_format_version' AND value=$1)
        AND to_regclass('profiled_phase_terminal_capacity') IS NOT NULL
        AND EXISTS(SELECT 1 FROM pg_trigger
            WHERE tgrelid=to_regclass('profiled_phase_execution_grants')
            AND tgname='profiled_phase_terminal_reserve' AND NOT tgisinternal
            AND tgenabled IN ('O','A')
            AND tgfoid=to_regprocedure('profiled_phase_terminal_reserve()'))",
    )
    .bind(crate::STATE_FORMAT_VERSION.to_string())
    .fetch_one(&state.store.pool)
    .await
    .map_err(|_| AppError::Internal)?;
    if crate::STATE_FORMAT_VERSION < 33 || !ready {
        return Err(AppError::Maintenance(
            "phase terminal storage is not ready".into(),
        ));
    }
    macro_rules! respond {
        ($family:path, $delivery:expr) => {{
            use $family as family;
            let document = family::Document::ExecutionDelivery(Box::new($delivery));
            // An already-committed receipt remains uncertain if its response
            // cannot be encoded. No new operation/expiry may replace it.
            document.validate_shape().map_err(|_| {
                AppError::IdentityUnavailable(
                    "phase Prepare outcome unknown: response envelope unavailable".into(),
                )
            })?;
            let mut response = Json(document).into_response();
            crate::delegated_permits::handoff::response_header(state, &auth, &mut response).await?;
            Ok(response)
        }};
    }
    match q {
        super::super::phase_profile::PrepareRequest::Frozen(q) => {
            crate::delegated_permits::runtime::require_binding(&auth, &q.original.binding)?;
            // A shared URI becoming available for refresh must not activate the
            // frozen issuer without its own paired terminal qualification.
            if !super::policy::transport_enabled(state) {
                return Err(AppError::Maintenance(
                    "phase terminal owner is not installed".into(),
                ));
            }
            respond!(
                aq_admission::preparation_v2,
                super::prepare(state, &auth, &q, nonce).await?
            )
        }
        super::super::phase_profile::PrepareRequest::Refresh(q) => {
            crate::delegated_permits::runtime::require_binding(&auth, &q.original.binding)?;
            if !super::super::phase_refresh_execution::policy::transport_enabled(state) {
                return Err(AppError::Maintenance(
                    "refresh phase terminal owner is not installed".into(),
                ));
            }
            respond!(
                aq_admission::preparation_refresh_v1,
                super::super::phase_refresh_execution::prepare(state, &auth, &q, nonce).await?
            )
        }
    }
}
