//! Auth bridge for the separately owned manual-session lifecycle.
use crate::{AppState, authz::RequireOwner, worker_api::authenticate_context};
use aq_manual_sessions::{Auth, Error, Service};
use axum::{
    extract::FromRequestParts,
    http::{HeaderMap, Request, StatusCode},
    response::IntoResponse,
};
use std::{collections::BTreeMap, sync::Arc};
fn refused(error: crate::error::AppError) -> Error {
    Error(
        error.into_response().status(),
        "Manual session access refused",
    )
}
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
                .map_err(refused)
        })
    });
    let node_state = state.clone();
    let node: Auth = Arc::new(move |headers: HeaderMap| {
        let state = node_state.clone();
        Box::pin(async move {
            let auth = authenticate_context(&state, &headers)
                .await
                .map_err(refused)?;
            if auth.delegated().is_some() {
                return Err(Error(
                    StatusCode::FORBIDDEN,
                    "Manual sessions require an enrolled worker credential",
                ));
            }
            Ok(auth.id())
        })
    });
    let sources: BTreeMap<_, _> = state
        .config
        .profiled_execution
        .as_ref()
        .into_iter()
        .flat_map(|c| c.projects.iter())
        .filter(|p| p.enabled)
        .filter_map(|p| {
            serde_json::to_value(&p.source)
                .ok()
                .map(|v| (p.project_id, v))
        })
        .collect();
    Service::new(
        state.store.pool.clone(),
        owner,
        node,
        sources,
        state.store.manual_runtime.clone(),
    )
}
