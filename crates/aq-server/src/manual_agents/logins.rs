use crate::{
    AppState,
    error::AppError,
    worker_api::{authenticate_context, lock_current_node},
};
use aq_manual_sessions::{Error, logins::Materials};
use axum::response::IntoResponse;
use serde_json::{Value, json};
use std::sync::Arc;
pub(super) fn delivery(state: AppState) -> Materials {
    Arc::new(move |headers, providers| {
        let state = state.clone();
        Box::pin(async move {
            material(&state, headers, providers)
                .await
                .map_err(|e| match e {
                    AppError::Conflict(ref message)
                        if message
                            == "No permitted server subscription is available for this provider" =>
                    {
                        Error(
                            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                            "A selected server subscription is unavailable",
                        )
                    }
                    error => Error::from_response(error.into_response()),
                })
        })
    })
}
async fn material(
    state: &AppState,
    headers: axum::http::HeaderMap,
    providers: Vec<String>,
) -> Result<Value, AppError> {
    let auth = authenticate_context(state, &headers).await?;
    if auth.delegated().is_some() {
        return Err(AppError::Forbidden("An enrolled worker is required".into()));
    }
    let loaded = state
        .config
        .intelligence
        .as_deref()
        .ok_or(AppError::Internal)?;
    let mut result = Vec::new();
    let mut versions = Vec::new();
    for provider in providers {
        let (index, c) = loaded
            .subscription_for(auth.id(), &provider)
            .ok_or_else(|| {
                AppError::Conflict(
                    "No permitted server subscription is available for this provider".into(),
                )
            })?;
        let source = crate::profiled_execution::login_refresh::newest_source(
            &state.store.pool,
            loaded,
            &c.profile,
            c.revision,
        )
        .await?;
        let bytes = crate::profiled_execution::material::source_or_configured(
            loaded,
            index,
            source.as_ref(),
        )
        .map_err(|_| AppError::Conflict("Server subscription login needs attention".into()))?;
        let value = std::str::from_utf8(bytes.bytes()).map_err(|_| AppError::Internal)?;
        result.push(json!({"provider":provider,"material":value}));
        versions.push((c.profile.clone(), c.revision, source.map(|s| s.revision_id)));
    }
    let mut tx = state.store.pool.begin().await.map_err(crate::error::db)?;
    aq_machine_control::management::lock(&mut tx)
        .await
        .map_err(|_| AppError::Internal)?;
    lock_current_node(&mut tx, &auth).await?;
    crate::intelligence::fence_current_registry(state, &mut tx).await?;
    if !aq_machine_control::enrolled(&mut tx, auth.id())
        .await
        .map_err(|_| AppError::Internal)?
        || aq_machine_control::management::capacity(&mut tx, auth.id())
            .await
            .map_err(|_| AppError::Internal)?
            .removed
    {
        return Err(AppError::Forbidden("An enrolled worker is required".into()));
    }
    if aq_machine_control::management::capacity(&mut tx, auth.id())
        .await
        .map_err(|_| AppError::Internal)?
        .machine_used
        > 0
    {
        return Err(AppError::Conflict(
            "Worker will install logins when its agents are stopped".into(),
        ));
    }
    for (profile, revision, version) in versions {
        if crate::profiled_execution::login_refresh::newest_source(
            &mut *tx, loaded, &profile, revision,
        )
        .await?
        .map(|s| s.revision_id)
            != version
        {
            return Err(AppError::Conflict(
                "Subscription changed; retry delivery".into(),
            ));
        }
    }
    tx.commit().await.map_err(crate::error::db)?;
    Ok(Value::Array(result))
}
