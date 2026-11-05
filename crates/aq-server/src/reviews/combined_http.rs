//! Protected adapters only. Domain owners retain current actor, exact subject,
//! source/receipt and original-effect identity checks. Only explicit owner POSTs
//! admit decisions; a GET never dispatches a completion.
use super::*;
use crate::{admin_api::ApiJson, authz::RequireOwner, profiled_execution::settlement};
use axum::{
    Extension, Json, Router,
    extract::Path,
    routing::{get, post},
};
use serde::Deserialize;

/// New HTTP envelopes are objects; serde's derived struct decoder otherwise
/// also accepts positional sequences. Stream the map into T so duplicate and
/// unknown-field checks remain the original typed decoder's responsibility.
struct ObjectBody<T>(T);
impl<'de, T: Deserialize<'de>> Deserialize<'de> for ObjectBody<T> {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct MapOnly<T>(std::marker::PhantomData<T>);
        impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for MapOnly<T> {
            type Value = ObjectBody<T>;
            fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("a JSON object")
            }
            fn visit_map<A: serde::de::MapAccess<'de>>(
                self,
                map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                T::deserialize(serde::de::value::MapAccessDeserializer::new(map)).map(ObjectBody)
            }
        }
        deserializer.deserialize_map(MapOnly(std::marker::PhantomData))
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResolveCombined {
    expected_subject: ObjectBody<ArtifactRef>,
    answer: ObjectBody<ResolveRequest>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PrepareCombined {
    report_id: Uuid,
    source_operation_id: Uuid,
    external: ObjectBody<combined::ExternalDeclaration>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AdmitCombined {
    report_id: Uuid,
    source_operation_id: Uuid,
    external: ObjectBody<combined::ExternalDeclaration>,
    decision: ObjectBody<DecisionRequest>,
}
/// Same existing evaluation fields; only the HTTP object shape is narrower.
/// Every caller-supplied value passes unchanged to the actual owner.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EvaluateBody {
    schema_version: u32,
    operation_id: Uuid,
    report_id: Uuid,
    source_operation_id: Uuid,
    decision_id: Uuid,
    subject: ObjectBody<ArtifactRef>,
}
impl EvaluateBody {
    fn into_request(self) -> settlement::EvaluateRequest {
        settlement::EvaluateRequest {
            schema_version: self.schema_version,
            operation_id: self.operation_id,
            report_id: self.report_id,
            source_operation_id: self.source_operation_id,
            decision_id: self.decision_id,
            subject: self.subject.0,
        }
    }
}
/// Merged only inside the existing configured reviews router, beneath its
/// Service extension and body limit and the daemon's human protection layers.
pub(super) fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/reviews/combined/prepare", post(prepare))
        .route("/api/reviews/combined", post(admit))
        .route("/api/reviews/combined/{id}", get(inspect))
        .route("/api/reviews/combined/{id}/resolve", post(resolve))
        .route("/api/settlement/evaluations", post(evaluate))
        .route("/api/settlement/evaluations/{id}", get(lookup))
        .route(
            "/api/settlement/completions/{id}/reconcile",
            post(reconcile),
        )
}
async fn prepare(
    RequireOwner(actor): RequireOwner,
    Extension(service): Extension<Arc<Service>>,
    ApiJson(ObjectBody(request)): ApiJson<ObjectBody<PrepareCombined>>,
) -> Result<Json<combined::CombinedSubject>> {
    let prepared = service
        .prepare_combined_as(
            &actor,
            request.report_id,
            request.source_operation_id,
            request.external.0,
        )
        .await?;
    Ok(Json(prepared.subject().clone()))
}
async fn admit(
    RequireOwner(actor): RequireOwner,
    Extension(service): Extension<Arc<Service>>,
    ApiJson(ObjectBody(request)): ApiJson<ObjectBody<AdmitCombined>>,
) -> Result<Json<DecisionRequest>> {
    // Rebuild actual source/effect reads; caller-supplied subject bytes alone
    // never constitute a PreparedCombined or a completion permit.
    let prepared = service
        .prepare_combined_as(
            &actor,
            request.report_id,
            request.source_operation_id,
            request.external.0,
        )
        .await?;
    service
        .admit_combined(actor.id, &prepared, &request.decision.0)
        .await?;
    Ok(Json(request.decision.0))
}
fn owner(service: &Service) -> settlement::Service {
    settlement::Service::new(service.state.clone(), service.clone())
}
async fn inspect(
    RequireOwner(actor): RequireOwner,
    Extension(service): Extension<Arc<Service>>,
    Path(id): Path<Uuid>,
) -> Result<Json<serde_json::Value>> {
    Ok(Json(service.inspect_combined_as(&actor, id).await?))
}
async fn resolve(
    RequireOwner(actor): RequireOwner,
    Extension(service): Extension<Arc<Service>>,
    Path(id): Path<Uuid>,
    ApiJson(ObjectBody(request)): ApiJson<ObjectBody<ResolveCombined>>,
) -> Result<Json<DecisionReceipt>> {
    if id != request.answer.0.decision_id {
        return Err(invalid());
    }
    Ok(Json(
        service
            .resolve_combined_as(&actor, &request.answer.0, &request.expected_subject.0)
            .await?,
    ))
}
async fn evaluate(
    RequireOwner(actor): RequireOwner,
    Extension(service): Extension<Arc<Service>>,
    ApiJson(ObjectBody(request)): ApiJson<ObjectBody<EvaluateBody>>,
) -> Result<Json<settlement::EvaluationReceipt>> {
    Ok(Json(
        owner(&service)
            .evaluate(&actor, &request.into_request())
            .await?,
    ))
}
async fn lookup(
    RequireOwner(actor): RequireOwner,
    Extension(service): Extension<Arc<Service>>,
    Path(id): Path<Uuid>,
) -> Result<Json<settlement::EvaluationReceipt>> {
    Ok(Json(owner(&service).lookup(&actor, id).await?))
}
async fn reconcile(
    RequireOwner(actor): RequireOwner,
    Extension(service): Extension<Arc<Service>>,
    Path(id): Path<Uuid>,
    ApiJson(request): ApiJson<serde_json::Value>,
) -> Result<Json<settlement::CompletionStatus>> {
    // A derived empty struct also accepts an empty sequence in serde. Require
    // the literal empty-object shape, not []/null or an unbound extra option.
    if !request.as_object().is_some_and(|object| object.is_empty()) {
        return Err(AppError::BadRequest(
            "reconcile requires an empty JSON object".into(),
        ));
    }
    Ok(Json(owner(&service).complete(&actor, id).await?))
}
