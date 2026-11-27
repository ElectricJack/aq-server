//! Owner/project-supervisor publication recovery. The only remote operation is a read.
use super::*;
use crate::playbooks::host_adapters::{DeliveryOps, ServerDelivery};
use aq_delivery::retained_publication::{self as publication, Identity};
use axum::routing::post;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Inspection {
    repository_id: Uuid,
    after: Option<Uuid>,
    publication_operation_id: Option<Uuid>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Recovery {
    repository_id: Uuid,
    operation_id: Uuid,
    publication_operation_id: Uuid,
}
type Key = (Uuid, Uuid, String);
fn error(e: aq_delivery::DeliveryError) -> AppError {
    match e {
        aq_delivery::DeliveryError::InvalidInput(s) => AppError::BadRequest(s.into()),
        aq_delivery::DeliveryError::IdempotencyConflict => {
            AppError::Conflict("recovery request differs".into())
        }
        _ => AppError::Busy,
    }
}
async fn identity(s: &AppState, user: &User, key: &Key, repo: Uuid) -> Result<Identity> {
    let page = Page {
        profile: Some(ReadProfile::RecordedNativeV2),
        project_id: Some(key.0),
        after: None,
        limit: Some(1),
    };
    read_as(s, user, &page, Some((key.1, &key.2))).await?;
    publication::identity(&s.store.pool, key.0, key.1, &key.2, repo)
        .await
        .map_err(error)
}
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/api/projects/{project_id}/results/{report_id}/revisions/{revision}/publication",
            get(inspect),
        )
        .route(
            "/api/projects/{project_id}/results/{report_id}/revisions/{revision}/publication/reconcile",
            post(reconcile),
        )
}
async fn inspect(
    State(s): State<AppState>,
    RequireOwner(user): RequireOwner,
    Path(key): Path<Key>,
    Query(q): Query<Inspection>,
    Extension(budget): Extension<Arc<Semaphore>>,
) -> Result<Json<Value>> {
    let _permit = budget.try_acquire().map_err(|_| AppError::Busy)?;
    let id = identity(&s, &user, &key, q.repository_id).await?;
    let mut evidence = publication::inspect(&s.store.pool, &id, q.after)
        .await
        .map_err(error)?;
    if let Some(original) = q.publication_operation_id {
        let operations = ServerDelivery::new(s.clone());
        let target = operations
            .target(key.0)
            .await
            .map_err(|_| AppError::Busy)?
            .ok_or(AppError::Busy)?;
        let delivery = operations.commands().await.ok_or(AppError::Busy)?;
        evidence["selected"] =
            publication::inspect_operation(&s.store.pool, &delivery, &target, &id, original)
                .await
                .map_err(error)?;
    }
    Ok(Json(evidence))
}
async fn reconcile(
    State(s): State<AppState>,
    RequireOwner(user): RequireOwner,
    Path(key): Path<Key>,
    Extension(budget): Extension<Arc<Semaphore>>,
    Json(q): Json<Recovery>,
) -> Result<Json<Value>> {
    let _permit = budget.try_acquire().map_err(|_| AppError::Busy)?;
    let id = identity(&s, &user, &key, q.repository_id).await?;
    let operations = ServerDelivery::new(s.clone());
    let target = operations
        .target(key.0)
        .await
        .map_err(|_| AppError::Busy)?
        .ok_or(AppError::Busy)?;
    let delivery = operations.commands().await.ok_or(AppError::Busy)?;
    let receipt = publication::reconcile(
        &s.store.pool,
        &delivery,
        &target,
        &id,
        q.operation_id,
        q.publication_operation_id,
        user.id,
    )
    .await
    .map_err(error)?;
    Ok(Json(receipt))
}
