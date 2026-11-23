use crate::common::profiled::{Fixture, NOW};
use aq_admission::{
    ArtifactRef,
    transport::{ArtifactFetch, Feature, Register, WorkerIdentity},
};
use aq_server::profiled_execution::{
    config::{ProfiledConfig, QualifiedRuntime},
    registration::{self, NativeRegistration},
};
use axum::{Router, body::Body, http::Request};
use serde_json::json;
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;

pub(crate) async fn register(
    f: &mut Fixture,
    runtime: ArtifactRef,
    qualification: ArtifactRef,
) -> sqlx::PgPool {
    let registration = Register {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: WorkerIdentity {
            node_id: f.node.node_id,
            boot_id: f.node.boot_id,
        },
        runtime,
        capabilities: vec![],
        features: vec![Feature::ArtifactTransferV1],
    };
    let mut config = (*f.state.config).clone();
    config.profiled_execution = Some(Arc::new(ProfiledConfig {
        definition_bodies: vec![],
        revision: 1,
        qualified_runtimes: vec![QualifiedRuntime {
            runtime: registration.runtime.clone(),
            qualification: qualification.clone(),
            capabilities: vec![],
            features: registration.features.clone(),
            material_delivery: None,
        }],
        projects: vec![],
    }));
    f.state.config = Arc::new(config);
    aq_server::profiled_execution::policies::install(&f.state)
        .await
        .unwrap();
    let native = NativeRegistration {
        schema_version: 1,
        semantic_digest: registration::semantic_digest(&registration).unwrap(),
        request: registration,
        qualification,
        configuration_revision: 1,
        registered_at_ms: NOW,
    };
    let pool = f.db.pool().await;
    sqlx::query("UPDATE execution_nodes SET state=jsonb_set(state,'{native}',$2) WHERE node_id=$1")
        .bind(f.node.node_id)
        .bind(json!(native))
        .execute(&pool)
        .await
        .unwrap();
    pool
}
pub(crate) async fn post(
    app: Router,
    body: &ArtifactFetch,
    token: &str,
) -> axum::response::Response {
    app.oneshot(
        Request::post("/v1/worker/profiled/artifact")
            .header("content-type", "application/json")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::from(serde_json::to_vec(body).unwrap()))
            .unwrap(),
    )
    .await
    .unwrap()
}
