//! Shared synthetic native admission/Started fixture; not provider qualification.
use super::profiled::{Fixture, NOW, fixture, insert};
use aq_admission::{
    Digest,
    started::Request,
    transport::{Feature, Register, WorkerIdentity},
};
use aq_server::{
    execution::{
        authority::hot::{HotState, LaunchOwner},
        profiled::Status,
    },
    profiled_execution::{
        config::{ProfiledConfig, QualifiedRuntime},
        journal,
        registration::{self, NativeRegistration},
    },
};
use serde_json::json;
use std::sync::Arc;
pub async fn setup(prepared: bool) -> (Fixture, Request) {
    setup_native(prepared, true).await
}
pub async fn setup_native(prepared: bool, native_admitted: bool) -> (Fixture, Request) {
    let mut f = fixture().await;
    let mut request: Request = serde_json::from_str(include_str!(
        "../../../aq-admission/artifacts/started.fixture.json"
    ))
    .unwrap();
    request.binding = f.profiled.offer.binding.clone();
    request.observation.guardian.intent.session = request.binding.session.clone();
    request.observation.guardian.intent.manifest_digest =
        request.binding.launch_manifest_digest.clone();
    request.observation_digest = request.observation.digest().unwrap();
    let registered = Register {
        schema_version: 1,
        operation_id: uuid::Uuid::new_v4(),
        identity: WorkerIdentity {
            node_id: f.node.node_id,
            boot_id: f.node.boot_id,
        },
        runtime: request.runtime.clone(),
        capabilities: vec![],
        features: vec![Feature::DurableHostJournalV1, Feature::HostStopEvidenceV1],
    };
    let mut config = (*f.state.config).clone();
    config.profiled_execution = Some(Arc::new(ProfiledConfig {
        revision: 1,
        qualified_runtimes: vec![QualifiedRuntime {
            runtime: request.runtime.clone(),
            qualification: request.qualification.clone(),
            capabilities: vec![],
            features: registered.features.clone(),
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
        semantic_digest: registration::semantic_digest(&registered).unwrap(),
        request: registered,
        qualification: request.qualification.clone(),
        configuration_revision: 1,
        registered_at_ms: NOW,
    };
    let pool = f.db.pool().await;
    if native_admitted {
        sqlx::query(
            "UPDATE execution_nodes SET state=jsonb_set(state,'{native}',$2) WHERE node_id=$1",
        )
        .bind(f.node.node_id)
        .bind(json!(native))
        .execute(&pool)
        .await
        .unwrap();
    }
    let attempt = insert(&f).await;
    let mut hot = HotState::from_compat(&f.profiled).unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(f.node.node_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("SELECT attempt_id FROM execution_attempts WHERE attempt_id=$1 FOR UPDATE")
        .bind(attempt)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,1,1,0)").bind(attempt).bind(json!(hot)).execute(&mut *tx).await.unwrap();
    let owner = LaunchOwner {
        guardian_id: request.guardian_id,
        launch_id: request.launch_id,
        reservation_id: request.reservation_id,
    };
    hot.owner = Some(owner.clone());
    hot.status = Status::Preparing;
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(hot))
        .execute(&mut *tx)
        .await
        .unwrap();
    journal::record_reserved(
        &mut tx,
        &request.binding,
        &owner,
        1,
        &Digest::of(b"reserved"),
    )
    .await
    .unwrap();
    hot.status = Status::Granted;
    hot.execution_deadline_ms = Some(NOW + 30_000);
    hot.grant = Some(aq_session_protocol::ProfiledGrant {
        binding: request.binding.clone(),
        grant_id: request.grant_id,
        expires_at_ms: NOW + 1_000,
    });
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(hot))
        .execute(&mut *tx)
        .await
        .unwrap();
    if prepared {
        journal::record_prepared(
            &mut tx,
            &request.binding,
            &owner,
            request.grant_id,
            2,
            &Digest::of(b"prepared"),
        )
        .await
        .unwrap();
    }
    tx.commit().await.unwrap();
    (f, request)
}
pub async fn context(f: &Fixture) -> aq_server::worker_api::AuthenticatedNode {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        "authorization",
        format!("Bearer {}", f.credential).parse().unwrap(),
    );
    aq_server::worker_api::authenticate_context(&f.state, &headers)
        .await
        .unwrap()
}
pub async fn state(f: &Fixture, request: &Request) -> serde_json::Value {
    sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
        .bind(request.binding.attempt_id)
        .fetch_one(&f.db.pool().await)
        .await
        .unwrap()
}
pub fn stop_request(previous: &Request) -> aq_admission::stopped::Request {
    let mut request: aq_admission::stopped::Request = serde_json::from_str(include_str!(
        "../../../aq-admission/artifacts/stopped.fixture.json"
    ))
    .unwrap();
    request.evidence.binding = previous.binding.clone();
    request.evidence.guardian_id = previous.guardian_id;
    request.evidence.launch_id = previous.launch_id;
    request.reservation_id = previous.reservation_id;
    request.runtime = previous.runtime.clone();
    request.qualification = previous.qualification.clone();
    request.proof.observation.guardian = previous.observation.guardian.clone();
    request.proof.observation.pane = previous.observation.pane.clone();
    request.evidence.disposition = aq_admission::transport::StopDisposition::Stopped {
        grant_id: previous.grant_id,
        stop_receipt_digest: request.proof.digest().unwrap(),
    };
    request.validate_after(previous).unwrap();
    request
}
