//! Daemon-owned provisionability, never an installed key or provider assertion.
use crate::common;
use aq_admission::transport::{Feature, Register, WorkerIdentity};
use aq_selection::{AuthorizedCredential, DeliveryCapability};
use aq_server::{
    intelligence::config::{CredentialConfig, IntelligenceConfig, MaterialDeliveryMode},
    profiled_execution::{
        config::{ProfiledConfig, QualifiedRuntime},
        registration::{self, NativeRegistration},
    },
};
use axum::{Router, body::Body, http::Request};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, sync::Arc};
use tower::ServiceExt;
use uuid::Uuid;
async fn post(
    app: &Router,
    path: &str,
    body: Value,
    token: Option<&str>,
) -> (axum::http::StatusCode, Value) {
    let mut req = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json");
    if let Some(token) = token {
        req = req.header("authorization", format!("Bearer {token}"));
    } else {
        req = req
            .header("x-aq-dev-email", "owner@localhost")
            .header("X-AQ-Request", "1");
    }
    let response = app
        .clone()
        .oneshot(
            req.body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
#[tokio::test]
async fn cold_native_inventory_requires_exact_qualified_delivery_and_fresh_owner_file() {
    use common::profiled::{NOW, artifact, fixture};
    let mut f = fixture().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("private-material");
    std::fs::write(&path, b"synthetic-key").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let choice = &f.selection.choice;
    let mut config = (*f.state.config).clone();
    config.intelligence = Some(Arc::new(
        IntelligenceConfig {
            accounting: Default::default(),
            material_store: None,
            revision: 1,
            credentials: vec![CredentialConfig {
                kind: Default::default(),
                allowed_backends: aq_selection::default_allowed_backends(),
                credential: AuthorizedCredential {
                    profile: choice.credential_profile.clone(),
                    provider: choice.provider.clone(),
                    account_reference: choice.account_reference.clone(),
                    endpoint_reference: choice.endpoint_reference.clone(),
                    revision: f.selection.credential_revision,
                    enabled: true,
                    provisionable: false,
                    material_owner: None,
                    allowed_backends: aq_selection::default_allowed_backends(),
                },
                allowed_node_ids: vec![f.node.node_id],
                allow_enrolled_nodes: false,
                secret_file: Some(path.clone()),
                delivery_mode: Some(MaterialDeliveryMode::ServerMaterialV1),
            }],
            qualified_harnesses: vec![],
        }
        .load()
        .unwrap(),
    ));
    let request = Register {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: WorkerIdentity {
            node_id: f.node.node_id,
            boot_id: f.node.boot_id,
        },
        runtime: artifact("synthetic-runtime", b"code"),
        capabilities: vec![],
        features: vec![Feature::ServerMaterialV1],
    };
    let qualification = artifact("synthetic-qualification", b"qualification");
    let delivery = DeliveryCapability {
        adapter: artifact("synthetic-material-delivery", b"delivery"),
        version: 1,
    };
    config.profiled_execution = Some(Arc::new(ProfiledConfig {
        definition_bodies: vec![],
        context_qualifications: vec![],
        revision: 1,
        qualified_runtimes: vec![QualifiedRuntime {
            runtime: request.runtime.clone(),
            qualification: qualification.clone(),
            capabilities: vec![],
            features: request.features.clone(),
            material_delivery: Some(delivery.clone()),
        }],
        projects: vec![],
    }));
    f.state.config = Arc::new(config);
    aq_server::profiled_execution::policies::install(&f.state)
        .await
        .unwrap();
    // Explicit fixture admission only; qualification is synthetic, not a host.
    let metadata = NativeRegistration {
        schema_version: 1,
        semantic_digest: registration::semantic_digest(&request).unwrap(),
        request,
        qualification,
        configuration_revision: 1,
        registered_at_ms: NOW,
    };
    sqlx::query("UPDATE execution_nodes SET state=jsonb_set(state,'{native}',$2) WHERE node_id=$1")
        .bind(f.node.node_id)
        .bind(json!(metadata))
        .execute(&f.db.pool().await)
        .await
        .unwrap();
    let admin = aq_server::build_admin_router(f.state.clone());
    assert_eq!(
        post(&admin, "/api/intelligence/register", json!({}), None)
            .await
            .0,
        axum::http::StatusCode::OK
    );
    let worker = aq_server::build_worker_router(f.state.clone());
    let body = |sequence| json!({"operation_id":Uuid::new_v4(),"boot_id":f.node.boot_id,"sequence":sequence,"ttl_ms":1000,"qualifications":[],"credentials":[]});
    let (status, snapshot) = post(
        &worker,
        "/v1/worker/intelligence/inventory",
        body(1),
        Some(&f.credential),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{snapshot}");
    assert_eq!(snapshot["material_delivery"], json!(delivery));
    assert_eq!(snapshot["credential_bindings"][0]["available"], false);
    assert_eq!(
        snapshot["credential_bindings"][0]["preparation"]["target"]["boot_id"],
        f.node.boot_id.to_string()
    );
    assert!(
        !serde_json::to_string(&snapshot)
            .unwrap()
            .contains("synthetic-key")
    );
    let mut claimed = body(2);
    claimed["material_delivery"] = json!(delivery);
    assert!(
        post(
            &worker,
            "/v1/worker/intelligence/inventory",
            claimed,
            Some(&f.credential)
        )
        .await
        .0
        .is_client_error()
    );
    let mut forged = body(2);
    forged["credentials"] = json!([{"profile":choice.credential_profile,"credential_revision":f.selection.credential_revision,"available":true}]);
    assert_eq!(
        post(
            &worker,
            "/v1/worker/intelligence/inventory",
            forged,
            Some(&f.credential)
        )
        .await
        .0,
        axum::http::StatusCode::BAD_REQUEST
    );
    std::fs::remove_file(&path).unwrap();
    let (status, snapshot) = post(
        &worker,
        "/v1/worker/intelligence/inventory",
        body(2),
        Some(&f.credential),
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK, "{snapshot}");
    assert_eq!(snapshot["credential_bindings"], json!([]));
    let pool = f.db.pool().await;
    for (rotates, request_sequence) in [(false, 3), (true, 4)] {
        let mut lock = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
            .bind(f.node.node_id)
            .execute(&mut *lock)
            .await
            .unwrap();
        let blocker: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
            .fetch_one(&mut *lock)
            .await
            .unwrap();
        let waiting_worker = worker.clone();
        let waiting_token = f.credential.clone();
        let waiting_body = body(request_sequence);
        let pending = tokio::spawn(async move {
            post(
                &waiting_worker,
                "/v1/worker/intelligence/inventory",
                waiting_body,
                Some(&waiting_token),
            )
            .await
        });
        let mut observed_wait = false;
        for _ in 0..100 {
            observed_wait=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND $1=ANY(pg_blocking_pids(pid)))")
            .bind(blocker).fetch_one(&pool).await.unwrap();
            if observed_wait {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(observed_wait, "inventory did not reach observed node lock");
        if rotates {
            sqlx::query("UPDATE nodes SET credential_hash=$2 WHERE id=$1")
                .bind(f.node.node_id)
                .bind(aq_server::tokens::verifier("rotated"))
                .execute(&mut *lock)
                .await
                .unwrap();
        } else {
            f.time.fetch_add(1001, std::sync::atomic::Ordering::SeqCst);
        }
        lock.commit().await.unwrap();
        assert_eq!(
            pending.await.unwrap().0,
            if rotates {
                axum::http::StatusCode::UNAUTHORIZED
            } else {
                axum::http::StatusCode::CONFLICT
            }
        );
    }
    let sequence: i64 =
        sqlx::query_scalar("SELECT sequence FROM intelligence_node_snapshots WHERE node_id=$1")
            .bind(f.node.node_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(sequence, 2);
}
