//! Explicit admitted-metadata fixtures; no real runtime/host is qualified here.
use crate::common;
use aq_admission::transport::{Feature, Register, WorkerIdentity};
use aq_server::profiled_execution::{
    config::{ProfiledConfig, QualifiedRuntime},
    registration::{self, NativeRegistration},
};
use common::profiled::{NOW, artifact, fixture};
use std::sync::Arc;
use uuid::Uuid;

#[tokio::test]
async fn startup_installs_policy_anchor_and_refuses_stale_configuration() {
    let mut f = fixture().await;
    let mut config = (*f.state.config).clone();
    config.profiled_execution = Some(Arc::new(ProfiledConfig {
        definition_bodies: vec![],
        context_qualifications: vec![],
        revision: 1,
        qualified_runtimes: vec![],
        projects: vec![],
    }));
    f.state.config = Arc::new(config);
    let admin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let worker = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(5),
        aq_server::run_with_listeners(f.state.clone(), admin, worker, async {}),
    )
    .await
    .unwrap()
    .unwrap();
    let pool = f.db.pool().await;
    let mut tx = pool.begin().await.unwrap();
    aq_server::profiled_execution::policies::fence_configuration_locked(
        &mut tx,
        f.state.config.profiled_execution.as_deref().unwrap(),
    )
    .await
    .unwrap();
    tx.rollback().await.unwrap();
    let stale = f.state.clone();
    let mut config = (*f.state.config).clone();
    Arc::make_mut(config.profiled_execution.as_mut().unwrap()).revision = 2;
    f.state.config = Arc::new(config);
    aq_server::profiled_execution::policies::install(&f.state)
        .await
        .unwrap();
    let admin = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let worker = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let error = aq_server::run_with_listeners(stale, admin, worker, async {})
        .await
        .unwrap_err();
    // The startup error keeps its stable prefix and names the typed cause.
    let message = error.to_string();
    assert!(
        message.starts_with("profiled policy installation failed: Conflict("),
        "{message}"
    );
    assert!(
        message.contains("profiled configuration revision is stale or has changed content"),
        "{message}"
    );
}

#[tokio::test]
async fn configured_allowlist_does_not_synthesize_registration_and_admitted_identity_is_exact() {
    let mut f = fixture().await;
    let request = Register {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: WorkerIdentity {
            node_id: f.node.node_id,
            boot_id: f.node.boot_id,
        },
        runtime: artifact("synthetic-native-runtime", b"synthetic-code"),
        capabilities: vec![],
        features: vec![Feature::ArtifactTransferV1, Feature::DurableHostJournalV1],
    };
    let qualification = artifact(
        "synthetic-runtime-qualification",
        b"synthetic-qualification",
    );
    let mut config = (*f.state.config).clone();
    config.profiled_execution = Some(Arc::new(ProfiledConfig {
        definition_bodies: vec![],
        context_qualifications: vec![],
        revision: 1,
        qualified_runtimes: vec![QualifiedRuntime {
            runtime: request.runtime.clone(),
            qualification: qualification.clone(),
            capabilities: vec![],
            features: request.features.clone(),
            material_delivery: None,
        }],
        projects: vec![],
    }));
    f.state.config = Arc::new(config);
    aq_server::profiled_execution::policies::install(&f.state)
        .await
        .unwrap();
    let pool = f.db.pool().await;
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(f.node.node_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    assert!(
        registration::current_runtime_locked(&f.state, &mut tx, f.node.node_id, f.node.boot_id)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    let legacy = aq_worker_protocol::RegisterRequest {
        identity: f.node.clone(),
        operation_id: Uuid::new_v4(),
        protocol_version: 2,
        worker_version: "0.1.0".into(),
        capabilities: vec![aq_worker_protocol::FAKE_HARNESS_CAPABILITY.into()],
    };
    let historical = f.state.store.execution_register(&legacy).await.unwrap();
    let metadata = NativeRegistration {
        schema_version: 1,
        semantic_digest: registration::semantic_digest(&request).unwrap(),
        request: request.clone(),
        qualification: qualification.clone(),
        configuration_revision: 1,
        registered_at_ms: NOW,
    };
    sqlx::query("UPDATE execution_nodes SET state=jsonb_set(state,'{native}',$2) WHERE node_id=$1")
        .bind(f.node.node_id)
        .bind(serde_json::json!(metadata))
        .execute(&pool)
        .await
        .unwrap();
    // Old exact acknowledgments remain readable, but cannot reinstall a
    // fixture incarnation or make the native runtime eligible for fixture offers.
    let before: serde_json::Value =
        sqlx::query_scalar("SELECT state FROM execution_nodes WHERE node_id=$1")
            .bind(f.node.node_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        serde_json::to_value(f.state.store.execution_register(&legacy).await.unwrap()).unwrap(),
        serde_json::to_value(historical).unwrap()
    );
    let mut changed = legacy.clone();
    changed.operation_id = Uuid::new_v4();
    assert!(f.state.store.execution_register(&changed).await.is_err());
    assert!(
        f.state
            .store
            .execution_replace_incarnation(&changed)
            .await
            .is_err()
    );
    let after: serde_json::Value =
        sqlx::query_scalar("SELECT state FROM execution_nodes WHERE node_id=$1")
            .bind(f.node.node_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(before, after);
    assert!(
        f.state
            .store
            .execution_reconcile(
                &aq_worker_protocol::ReconcileRequest {
                    identity: f.node.clone(),
                    operation_id: Uuid::new_v4(),
                    attempts: vec![],
                    cursor: None,
                    max_items: 16,
                },
                NOW
            )
            .await
            .is_err()
    );
    assert!(
        f.state
            .store
            .execution_poll(
                &aq_worker_protocol::PollRequest {
                    identity: f.node.clone(),
                    operation_id: Uuid::new_v4(),
                    inventory_sequence: 1,
                    available: aq_worker_protocol::Resources {
                        slots: 1,
                        cpu_millis: 100,
                        memory_bytes: 1024,
                        disk_bytes: 1024
                    },
                    max_items: 1,
                },
                NOW
            )
            .await
            .is_err()
    );

    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(f.node.node_id)
        .execute(&mut *tx)
        .await
        .unwrap();
    let actual =
        registration::current_runtime_locked(&f.state, &mut tx, f.node.node_id, f.node.boot_id)
            .await
            .unwrap();
    assert_eq!(actual.runtime, request.runtime);
    assert_eq!(actual.qualification, qualification);
    assert_eq!(actual.boot_id, f.node.boot_id);
    assert!(
        registration::current_runtime_locked(&f.state, &mut tx, f.node.node_id, Uuid::new_v4())
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
    let mut corrupted = metadata.clone();
    corrupted.semantic_digest = aq_admission::Digest::of(b"changed");
    sqlx::query("UPDATE execution_nodes SET state=jsonb_set(state,'{native}',$2) WHERE node_id=$1")
        .bind(f.node.node_id)
        .bind(serde_json::json!(corrupted))
        .execute(&pool)
        .await
        .unwrap();
    let mut tx = pool.begin().await.unwrap();
    assert!(
        registration::current_runtime_locked(&f.state, &mut tx, f.node.node_id, f.node.boot_id)
            .await
            .is_err()
    );
    tx.rollback().await.unwrap();
}
#[test]
fn native_semantics_ignore_only_operation_id_not_runtime_or_capability_change() {
    let first = Register {
        schema_version: 1,
        operation_id: Uuid::from_u128(1),
        identity: WorkerIdentity {
            node_id: Uuid::from_u128(2),
            boot_id: Uuid::from_u128(3),
        },
        runtime: artifact("synthetic-native-runtime", b"runtime"),
        capabilities: vec![],
        features: vec![Feature::ArtifactTransferV1],
    };
    let mut retry = first.clone();
    retry.operation_id = Uuid::from_u128(4);
    assert_eq!(
        registration::semantic_digest(&first).unwrap(),
        registration::semantic_digest(&retry).unwrap()
    );
    retry.features.push(Feature::DurableHostJournalV1);
    assert_ne!(
        registration::semantic_digest(&first).unwrap(),
        registration::semantic_digest(&retry).unwrap()
    );
}

#[tokio::test]
async fn native_writer_replays_without_reinstall_and_fences_held_claims_and_credentials() {
    use axum::{
        body::Body,
        http::{Request as HttpRequest, StatusCode},
    };
    use tower::ServiceExt;
    let mut f = fixture().await;
    let pool = f.db.pool().await;
    let request = Register {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: WorkerIdentity {
            node_id: f.node.node_id,
            boot_id: f.node.boot_id,
        },
        runtime: artifact("synthetic-native-runtime", b"code"),
        capabilities: vec![],
        features: vec![Feature::ArtifactTransferV1, Feature::DurableHostJournalV1],
    };
    let mut config = (*f.state.config).clone();
    config.profiled_execution = Some(Arc::new(ProfiledConfig {
        definition_bodies: vec![],
        context_qualifications: vec![],
        revision: 1,
        qualified_runtimes: vec![QualifiedRuntime {
            runtime: request.runtime.clone(),
            qualification: artifact("synthetic-qualification", b"qualification"),
            capabilities: vec![],
            features: request.features.clone(),
            material_delivery: None,
        }],
        projects: vec![],
    }));
    f.state.config = Arc::new(config);
    aq_server::profiled_execution::policies::install(&f.state)
        .await
        .unwrap();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        format!("Bearer {}", f.credential).parse().unwrap(),
    );
    let auth = aq_server::worker_api::authenticate_context(&f.state, &headers)
        .await
        .unwrap();
    let router = aq_server::build_worker_router(f.state.clone());
    let http_request = |token: &str, body: Vec<u8>| {
        HttpRequest::builder()
            .method("POST")
            .uri("/v1/worker/profiled/register")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    };
    let document = aq_admission::transport::TransportDocument::Register(request.clone());
    let bytes = serde_json::to_vec(&document).unwrap();
    assert_eq!(
        router
            .clone()
            .oneshot(http_request("aqn_wrong", bytes.clone()))
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let response = router
        .clone()
        .oneshot(http_request(&f.credential, bytes.clone()))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["cache-control"], "no-store");
    let response = axum::body::to_bytes(response.into_body(), 65536)
        .await
        .unwrap();
    let decoded = aq_admission::transport::decode_transport(&response).unwrap();
    assert_eq!(
        serde_json::to_value(decoded).unwrap(),
        serde_json::to_value(document).unwrap()
    );
    assert_eq!(
        router
            .oneshot(http_request(&f.credential, vec![b' '; 65537]))
            .await
            .unwrap()
            .status(),
        StatusCode::PAYLOAD_TOO_LARGE
    );
    let attempt = common::profiled::insert(&f).await;
    sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,1,1,0)").bind(attempt).bind(serde_json::json!(aq_server::execution::authority::hot::HotState::from_compat(&f.profiled).unwrap())).execute(&pool).await.unwrap();
    assert!(sqlx::query("UPDATE execution_nodes SET state=jsonb_set(state,'{worker_version}','\"changed\"') WHERE node_id=$1")
        .bind(f.node.node_id).execute(&pool).await.is_err());
    let mut changed = request.clone();
    changed.operation_id = Uuid::new_v4();
    changed.identity.boot_id = Uuid::new_v4();
    assert!(
        registration::register(&f.state, &auth, &changed)
            .await
            .is_err()
    );
    let mut semantic_retry = request.clone();
    semantic_retry.operation_id = Uuid::new_v4();
    registration::register(&f.state, &auth, &semantic_retry)
        .await
        .unwrap();
    // Changed bytes under the same operation conflict, even while policy matches.
    let mut conflict = request.clone();
    conflict.identity.boot_id = Uuid::new_v4();
    assert!(
        registration::register(&f.state, &auth, &conflict)
            .await
            .is_err()
    );
    f.time
        .store((NOW + 30_001) as i64, std::sync::atomic::Ordering::SeqCst);
    aq_server::profiled_execution::no_grant::close_expired(&f.state, f.node.node_id, attempt)
        .await
        .unwrap()
        .unwrap();
    registration::register(&f.state, &auth, &changed)
        .await
        .unwrap();
    // Historical original replay never restores the old boot.
    registration::register(&f.state, &auth, &request)
        .await
        .unwrap();
    let current: serde_json::Value =
        sqlx::query_scalar("SELECT state FROM execution_nodes WHERE node_id=$1")
            .bind(f.node.node_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        current["identity"]["boot_id"],
        changed.identity.boot_id.to_string()
    );
    let mut newer = f.state.clone();
    let mut newer_config = (*f.state.config).clone();
    let mut newer_profiled = (**newer_config.profiled_execution.as_ref().unwrap()).clone();
    newer_profiled.revision = 2;
    newer_profiled.qualified_runtimes.clear();
    newer_config.profiled_execution = Some(Arc::new(newer_profiled));
    newer.config = Arc::new(newer_config);
    aq_server::profiled_execution::policies::install(&newer)
        .await
        .unwrap();
    let mut stale_retry = changed.clone();
    stale_retry.operation_id = Uuid::new_v4();
    assert!(
        registration::register(&f.state, &auth, &stale_retry)
            .await
            .is_err()
    );
    let mut tx = pool.begin().await.unwrap();
    assert!(
        registration::current_runtime_locked(
            &f.state,
            &mut tx,
            f.node.node_id,
            changed.identity.boot_id
        )
        .await
        .is_err()
    );
    tx.rollback().await.unwrap();
    // Historical receipt readback survives policy retirement but remains
    // subject to current machine authentication.
    registration::register(&f.state, &auth, &request)
        .await
        .unwrap();
    sqlx::query("UPDATE nodes SET credential_hash=$2 WHERE id=$1")
        .bind(f.node.node_id)
        .bind(aq_server::tokens::verifier("rotated"))
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        registration::register(&f.state, &auth, &request)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn registration_receipt_cap_is_per_boot() {
    let mut f = fixture().await;
    let pool = f.db.pool().await;
    let request = Register {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: WorkerIdentity {
            node_id: f.node.node_id,
            boot_id: f.node.boot_id,
        },
        runtime: artifact("synthetic-native-runtime", b"code"),
        capabilities: vec![],
        features: vec![Feature::ArtifactTransferV1, Feature::DurableHostJournalV1],
    };
    let mut config = (*f.state.config).clone();
    config.profiled_execution = Some(Arc::new(ProfiledConfig {
        definition_bodies: vec![],
        context_qualifications: vec![],
        revision: 1,
        qualified_runtimes: vec![QualifiedRuntime {
            runtime: request.runtime.clone(),
            qualification: artifact("synthetic-qualification", b"qualification"),
            capabilities: vec![],
            features: request.features.clone(),
            material_delivery: None,
        }],
        projects: vec![],
    }));
    f.state.config = Arc::new(config);
    aq_server::profiled_execution::policies::install(&f.state)
        .await
        .unwrap();
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        format!("Bearer {}", f.credential).parse().unwrap(),
    );
    let auth = aq_server::worker_api::authenticate_context(&f.state, &headers)
        .await
        .unwrap();
    // Receipts are append-only, so the filled history is inserted directly.
    sqlx::query("INSERT INTO native_registration_receipts(node_id,operation_id,request_digest,response) SELECT $1,gen_random_uuid(),repeat('a',64),jsonb_build_object('identity',jsonb_build_object('boot_id',$2::text)) FROM generate_series(1,1024)")
        .bind(f.node.node_id)
        .bind(f.node.boot_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let mut boot_b = request.clone();
    boot_b.identity.boot_id = Uuid::new_v4();
    registration::register(&f.state, &auth, &boot_b)
        .await
        .unwrap();
    let mut boot_a = request.clone();
    boot_a.operation_id = Uuid::new_v4();
    let error = registration::register(&f.state, &auth, &boot_a)
        .await
        .unwrap_err();
    assert!(
        format!("{error:?}").contains("native registration receipt capacity exhausted"),
        "{error:?}"
    );
}
