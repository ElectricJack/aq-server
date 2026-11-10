//! Authorization: current-user resolution, owner bootstrap, CSRF guard.

use axum::extract::{FromRequestParts, Request, State};
use axum::http::Method;
use axum::http::request::Parts;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::AppState;
use crate::error::AppError;
use crate::store::{Role, User};

/// The authenticated, authorized user (Owner or Viewer).
pub struct CurrentUser(pub User);

/// An authenticated Owner; Viewers get 403.
pub struct RequireOwner(pub User);

async fn resolve(parts: &Parts, state: &AppState) -> Result<User, AppError> {
    let id = state.identity.verify(&parts.headers).await?;
    if let Some(user) = state
        .store
        .find_user_by_binding(&id.issuer, &id.subject)
        .await?
    {
        return Ok(user);
    }
    let now = (state.clock)();
    if let Some(user) = state
        .store
        .bind_invited_user(&id.email, &id.issuer, &id.subject, now)
        .await?
    {
        return Ok(user);
    }
    let b = &state.config.bootstrap;
    if b.enabled
        && b.owner_email
            .as_deref()
            .is_some_and(|e| e.eq_ignore_ascii_case(&id.email))
        && let Some(user) = state
            .store
            .bootstrap_owner(&id.email, &id.issuer, &id.subject, now)
            .await?
    {
        return Ok(user);
    }
    // Concurrent first logins with the same token: a sibling request may have
    // just bound or bootstrapped this identity and committed, making our
    // conditional write a no-op. Look the binding up once more before denying.
    if let Some(user) = state
        .store
        .find_user_by_binding(&id.issuer, &id.subject)
        .await?
    {
        return Ok(user);
    }
    Err(AppError::Forbidden(format!(
        "{} is not authorized for this Agent Q installation",
        id.email
    )))
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        resolve(parts, state).await.map(CurrentUser)
    }
}

impl FromRequestParts<AppState> for RequireOwner {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let user = resolve(parts, state).await?;
        if user.role != Role::Owner {
            return Err(AppError::Forbidden(
                "this action requires the owner role".to_string(),
            ));
        }
        Ok(RequireOwner(user))
    }
}

/// Applies the CSRF rules to a request: mutations need `X-AQ-Request: 1` (a
/// custom header cannot be sent cross-origin without a CORS preflight, which
/// this server never grants) and, if an `Origin` is sent, it must be our own.
/// GET and HEAD are exempt, so their handlers must never change state.
fn csrf_check(
    state: &AppState,
    method: &Method,
    headers: &axum::http::HeaderMap,
) -> Result<(), AppError> {
    aq_human_http::csrf_rejection(
        method,
        headers,
        state.config.server.public_origin.trim_end_matches('/'),
        matches!(state.config.auth, crate::config::AuthConfig::Dev { .. }),
    )
    .map_or(Ok(()), |reason| Err(AppError::CsrfRejected(reason.into())))
}

pub use aq_human_http::origin_allowed;

/// Middleware applying [`csrf_check`]; layer it with `from_fn_with_state`.
///
/// GET and HEAD bypass this guard, so their handlers must never change state.
pub async fn csrf_guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    match csrf_check(&state, req.method(), req.headers()) {
        Ok(()) => next.run(req).await,
        Err(e) => e.into_response(),
    }
}
