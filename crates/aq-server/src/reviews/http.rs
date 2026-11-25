use super::*;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelRequest {
    operation_id: Uuid,
    expected_decision_revision: u64,
}
/// Optional injection seam only. The ordinary production admin builder does
/// not mount these routes until its configured owner/protection is supplied.
/// Caller mounts below the existing identity/CSRF/maintenance layers.
pub fn admin_routes(service: Arc<Service>) -> axum::Router<AppState> {
    use axum::{
        extract::DefaultBodyLimit,
        routing::{get, post},
    };
    axum::Router::new()
        .route("/api/reviews/decisions", get(inbox::inbox_http))
        .route("/api/reviews/decisions/{id}", get(inspect_http))
        .route("/api/reviews/decisions/{id}/resolve", post(resolve_http))
        .route("/api/reviews/decisions/{id}/cancel", post(cancel_http))
        .route("/api/reviews/decisions/{id}/defer", post(view::defer_http))
        .route(
            "/api/reviews/decisions/{id}/evidence/{evidence}/viewed",
            post(view::viewed_http),
        )
        .merge(combined_http::routes())
        .layer(DefaultBodyLimit::max(16384))
        .layer(axum::Extension(service))
}
async fn inspect_http(
    crate::authz::RequireOwner(actor): crate::authz::RequireOwner,
    axum::Extension(service): axum::Extension<Arc<Service>>,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
) -> Result<axum::Json<serde_json::Value>> {
    Ok(axum::Json(service.decision_view(&actor, id).await?))
}
async fn resolve_http(
    crate::authz::RequireOwner(actor): crate::authz::RequireOwner,
    axum::Extension(service): axum::Extension<Arc<Service>>,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    crate::admin_api::ApiJson(answer): crate::admin_api::ApiJson<ResolveRequest>,
) -> Result<axum::Json<DecisionReceipt>> {
    if id != answer.decision_id {
        return Err(invalid());
    }
    answer.validate().map_err(|_| invalid())?;
    let pin =
        crate::profiled_execution::settlement::actor::ActorPin::observe(&service.state, &actor)
            .await?;
    let view = service
        .inspect_subject(
            ReviewActor::new(actor.id, Some(&pin)),
            id,
            SubjectProfile::Code,
        )
        .await?;
    let subject = serde_json::from_value(view["request"]["subject"].clone())
        .map_err(|_| AppError::Internal)?;
    let payload = action::Resolve::from_answer(answer, subject);
    let mut registry = crate::dashboard_api::actions::Registry::default();
    registry
        .register(action::ResolveDecision(Some(service.clone())))
        .map_err(crate::dashboard_api::actions::existing::legacy_error)?;
    let operation = crate::dashboard_api::actions::submit_legacy(
        &service.state,
        &registry,
        &actor,
        payload.request(),
    )
    .await
    .map_err(crate::dashboard_api::actions::existing::legacy_error)?;
    if operation.disposition == crate::dashboard_api::actions::Disposition::Conflict {
        return Err(conflict());
    }
    let view = service
        .inspect_subject(
            ReviewActor::new(actor.id, Some(&pin)),
            id,
            SubjectProfile::Code,
        )
        .await?;
    let receipt: DecisionReceipt =
        serde_json::from_value(view["receipt"].clone()).map_err(|_| {
            if operation.disposition == crate::dashboard_api::actions::Disposition::Conflict {
                conflict()
            } else {
                AppError::IdentityUnavailable("decision operation is unresolved".into())
            }
        })?;
    if !matches!(receipt.finality,DecisionFinality::HumanAnswer{owner_user_id,operation_id,..} if owner_user_id==actor.id && operation_id==operation.operation_id)
    {
        return Err(conflict());
    }
    Ok(axum::Json(receipt))
}
async fn cancel_http(
    crate::authz::RequireOwner(actor): crate::authz::RequireOwner,
    axum::Extension(service): axum::Extension<Arc<Service>>,
    axum::extract::Path(id): axum::extract::Path<Uuid>,
    crate::admin_api::ApiJson(request): crate::admin_api::ApiJson<CancelRequest>,
) -> Result<axum::Json<DecisionReceipt>> {
    Ok(axum::Json(
        service
            .cancel(
                actor.id,
                request.operation_id,
                id,
                request.expected_decision_revision,
            )
            .await?,
    ))
}
