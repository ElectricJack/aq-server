use crate::{
    AppState,
    error::AppError,
    worker_api::{authenticate_context, lock_current_node},
};
use aq_intelligence_config::LoadedRegistry;
use aq_manual_sessions::{Error, logins::Materials};
use aq_selection::ArtifactRef;
use axum::response::IntoResponse;
use serde_json::{Value, json};
use sqlx::{PgConnection, PgPool};
use std::sync::Arc;
use uuid::Uuid;
const UNAVAILABLE: &str = "No permitted server subscription is available for this provider";

pub(super) fn delivery(state: AppState) -> Materials {
    Arc::new(move |headers, providers| {
        let state = state.clone();
        Box::pin(async move {
            material(&state, headers, providers)
                .await
                .map_err(|e| match e {
                    AppError::Conflict(ref message) if message == UNAVAILABLE => Error(
                        axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                        "A selected server subscription is unavailable",
                    ),
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
    let (materials, versions) =
        read_materials(&state.store.pool, loaded, auth.id(), providers).await?;
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
    verify_versions(&mut tx, loaded, versions).await?;
    tx.commit().await.map_err(crate::error::db)?;
    Ok(Value::Array(materials))
}
struct SourceVersion {
    profile: ArtifactRef,
    revision: u64,
    head: Option<Uuid>,
}
async fn read_materials(
    pool: &PgPool,
    loaded: &LoadedRegistry,
    node: Uuid,
    providers: Vec<String>,
) -> Result<(Vec<Value>, Vec<SourceVersion>), AppError> {
    let mut materials = Vec::new();
    let mut versions = Vec::new();
    for provider in providers {
        let (index, c) = loaded
            .subscription_for(node, &provider)
            .ok_or_else(|| AppError::Conflict(UNAVAILABLE.into()))?;
        let source = crate::profiled_execution::login_refresh::newest_source(
            pool, loaded, &c.profile, c.revision,
        )
        .await?;
        let bytes = crate::profiled_execution::material::source_or_configured(
            loaded,
            index,
            source.as_ref(),
        )
        .map_err(|_| AppError::Conflict("Server subscription login needs attention".into()))?;
        let value = std::str::from_utf8(bytes.bytes()).map_err(|_| AppError::Internal)?;
        materials.push(json!({"provider":provider,"material":value}));
        versions.push(SourceVersion {
            profile: c.profile.clone(),
            revision: c.revision,
            head: source.map(|s| s.revision_id),
        });
    }
    Ok((materials, versions))
}
async fn verify_versions(
    conn: &mut PgConnection,
    loaded: &LoadedRegistry,
    versions: Vec<SourceVersion>,
) -> Result<(), AppError> {
    for version in versions {
        if crate::profiled_execution::login_refresh::newest_source(
            &mut *conn,
            loaded,
            &version.profile,
            version.revision,
        )
        .await?
        .map(|s| s.revision_id)
            != version.head
        {
            return Err(AppError::Conflict(
                "Subscription changed; retry delivery".into(),
            ));
        }
    }
    Ok(())
}
