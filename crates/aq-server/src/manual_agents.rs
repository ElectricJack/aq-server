use crate::{AppState, authz::RequireOwner, worker_api::authenticate_context};
use aq_manual_sessions::{Auth, Error, Service};
use axum::{
    extract::FromRequestParts,
    http::{HeaderMap, Request},
    response::IntoResponse,
};
use std::sync::Arc;
pub(super) fn service(state: &AppState) -> Service {
    let user_state = state.clone();
    let owner: Auth = Arc::new(move |headers: HeaderMap| {
        let state = user_state.clone();
        Box::pin(async move {
            let (mut parts, _) = Request::new(()).into_parts();
            parts.headers = headers;
            RequireOwner::from_request_parts(&mut parts, &state)
                .await
                .map(|user| user.0.id)
                .map_err(|error| Error::from_response(error.into_response()))
        })
    });
    let node_state = state.clone();
    let node: Auth = Arc::new(move |headers: HeaderMap| {
        let state = node_state.clone();
        Box::pin(async move {
            let auth = authenticate_context(&state, &headers)
                .await
                .map_err(|error| Error::from_response(error.into_response()))?;
            if auth.delegated().is_some() {
                return Err(Error::enrolled_worker_required());
            }
            Ok(auth.id())
        })
    });
    let config = &state.config;
    let sources = aq_manual_sessions::configured_sources(config.profiled_execution.as_ref());
    Service::new(
        state.store.pool.clone(),
        owner,
        node,
        sources,
        state.store.manual_runtime.clone(),
    )
    .intelligence_choices(aq_profile_settings::model_choices(
        config
            .intelligence
            .iter()
            .flat_map(|r| &r.config.qualified_harnesses),
        config.intelligence_preferences_file.as_deref(),
    ))
    .browser_origin(
        config.server.public_origin.clone(),
        matches!(config.auth, crate::config::AuthConfig::Dev { .. }),
    )
}
