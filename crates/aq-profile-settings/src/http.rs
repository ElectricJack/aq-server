//! Route assembly and typed dispatch shared by the thin server adapter.
use crate::{ModelChoice, Result};
use axum::{Router, extract::DefaultBodyLimit, routing::MethodRouter};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

pub fn routes<S: Clone + Send + Sync + 'static>(
    settings: MethodRouter<S>,
    operation: MethodRouter<S>,
) -> Router<S> {
    // Keep the established row-save boundary; JSON escaping needs extra room only for source.
    let structure = settings.clone().layer(DefaultBodyLimit::max(
        6 * super::structure::MAX_ROUTING_BYTES + 64 * 1024,
    ));
    Router::new()
        .route("/api/settings/intelligence", settings)
        .route("/api/settings/intelligence/structure", structure)
        .route(
            "/api/settings/intelligence/operations/{id}",
            operation.clone(),
        )
        .route(
            "/api/settings/intelligence/structure/operations/{id}",
            operation,
        )
        .layer(DefaultBodyLimit::max(128 * 1024))
}
pub async fn read(pool: &PgPool, choices: &[ModelChoice], path: &str) -> Result<Value> {
    if path.contains("/structure") {
        super::structure::read(pool).await
    } else {
        super::read(pool, choices).await
    }
}
pub async fn save(
    pool: &PgPool,
    actor: Uuid,
    body: Value,
    choices: &[ModelChoice],
    now_ms: i64,
    path: &str,
) -> Result<Value> {
    let invalid = |_| super::invalid("Invalid Intelligence settings request");
    if path.contains("/structure") {
        super::structure::save(pool, actor, serde_json::from_value(body).map_err(invalid)?).await
    } else {
        super::save(
            pool,
            actor,
            serde_json::from_value(body).map_err(invalid)?,
            choices,
            now_ms,
        )
        .await
    }
}
pub async fn operation(pool: &PgPool, actor: Uuid, id: Uuid, path: &str) -> Result<Value> {
    if path.contains("/structure") {
        super::structure::operation(pool, actor, id).await
    } else {
        super::operation(pool, actor, id).await
    }
}
