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

fn registration_request(
    identity: &aq_worker_protocol::NodeIdentity,
    runtime_bytes: &[u8],
) -> Register {
    Register {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        identity: WorkerIdentity {
            node_id: identity.node_id,
            boot_id: identity.boot_id,
        },
        runtime: artifact("synthetic-native-runtime", runtime_bytes),
        capabilities: vec![],
        features: vec![Feature::ArtifactTransferV1, Feature::DurableHostJournalV1],
    }
}

async fn authenticate(f: &common::profiled::Fixture) -> aq_server::worker_api::AuthenticatedNode {
    let mut headers = axum::http::HeaderMap::new();
    headers.insert(
        axum::http::header::AUTHORIZATION,
        format!("Bearer {}", f.credential).parse().unwrap(),
    );
    aq_server::worker_api::authenticate_context(&f.state, &headers)
        .await
        .unwrap()
}

async fn execution_node_state(pool: &sqlx::PgPool, node: Uuid) -> serde_json::Value {
    sqlx::query_scalar("SELECT state FROM execution_nodes WHERE node_id=$1")
        .bind(node)
        .fetch_one(pool)
        .await
        .unwrap()
}

async fn install_allowlist(
    state: &mut aq_server::AppState,
    request: &Register,
    qualification: aq_admission::ArtifactRef,
) {
    let mut config = (*state.config).clone();
    config.profiled_execution = Some(Arc::new(ProfiledConfig {
        definition_bodies: vec![],
        revision: 1,
        qualified_runtimes: vec![QualifiedRuntime {
            runtime: request.runtime.clone(),
            qualification,
            capabilities: vec![],
            features: request.features.clone(),
            material_delivery: None,
        }],
        projects: vec![],
    }));
    state.config = Arc::new(config);
    aq_server::profiled_execution::policies::install(state)
        .await
        .unwrap();
}

#[tokio::test]
async fn startup_installs_policy_anchor_and_refuses_stale_configuration() {
    let mut f = fixture().await;
    let mut config = (*f.state.config).clone();
    config.profiled_execution = Some(Arc::new(ProfiledConfig {
        definition_bodies: vec![],
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
    let request = registration_request(&f.node, b"synthetic-code");
    let qualification = artifact(
        "synthetic-runtime-qualification",
        b"synthetic-qualification",
    );
    install_allowlist(&mut f.state, &request, qualification.clone()).await;
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
    let before = execution_node_state(&pool, f.node.node_id).await;
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
    let after = execution_node_state(&pool, f.node.node_id).await;
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
    let request = registration_request(&f.node, b"code");
    install_allowlist(
        &mut f.state,
        &request,
        artifact("synthetic-qualification", b"qualification"),
    )
    .await;
    let auth = authenticate(&f).await;
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
    let current = execution_node_state(&pool, f.node.node_id).await;
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
    let request = registration_request(&f.node, b"code");
    install_allowlist(
        &mut f.state,
        &request,
        artifact("synthetic-qualification", b"qualification"),
    )
    .await;
    let auth = authenticate(&f).await;
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

#[tokio::test]
async fn node_slot_defaults_are_snapshotted_and_legacy_registrations_stay_single_slot() {
    let f = fixture().await;
    let pool = f.db.pool().await;
    let mut conn = pool.acquire().await.unwrap();
    let node = f.node.node_id;
    assert_eq!(
        aq_machine_control::slots::count(&mut conn, node)
            .await
            .unwrap(),
        2
    );
    sqlx::query("UPDATE node_slot_default SET slots=3,revision=revision+1")
        .execute(&mut *conn)
        .await
        .unwrap();
    assert_eq!(
        aq_machine_control::slots::count(&mut conn, node)
            .await
            .unwrap(),
        2
    );
    sqlx::query("UPDATE worker_settings SET max_agents=1 WHERE kind='node' AND id=$1")
        .bind(node.to_string())
        .execute(&mut *conn)
        .await
        .unwrap();
    let view = aq_machine_control::slots::view(&mut conn, node)
        .await
        .unwrap();
    assert_eq!((view.total, view.occupied, view.idle), (1, 0, 1));
    assert!(view.free(1));
    assert!(!view.free(2));
    assert!(
        !aq_machine_control::slots::poll_free(&mut conn, node, 2)
            .await
            .unwrap()
    );
    sqlx::query("UPDATE worker_settings SET max_agents=0 WHERE kind='node' AND id=$1")
        .bind(node.to_string())
        .execute(&mut *conn)
        .await
        .unwrap();
    assert_eq!(
        aq_machine_control::slots::view(&mut conn, node)
            .await
            .unwrap()
            .idle,
        0
    );
    assert!(
        aq_machine_control::management::capacity(&mut conn, node)
            .await
            .unwrap()
            .full()
    );
}

#[tokio::test]
async fn queued_and_manual_allocation_share_one_count_and_unknown_holds_only_its_index() {
    let f = fixture().await;
    let node = f.node.node_id;
    let pool = f.db.pool().await;
    let attempt = common::profiled::insert(&f).await;
    let mut tx = pool.begin().await.unwrap();
    aq_machine_control::management::lock(&mut tx).await.unwrap();
    aq_machine_control::slots::assign(&mut tx, attempt, node, 1)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let manual = Uuid::new_v4();
    sqlx::query("INSERT INTO manual_agent_sessions(id,actor_id,node_id,project_id,spec,status,stage,created_at_ms,updated_at_ms) VALUES($1,$2,$3,$4,'{}','pending','Waiting',1,1)")
        .bind(manual).bind(f.actor).bind(node).bind(f.selection.lineage.project_id).execute(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    aq_machine_control::management::lock(&mut tx).await.unwrap();
    assert_eq!(
        aq_machine_control::slots::assign_manual(&mut tx, manual, node)
            .await
            .unwrap(),
        Some(2)
    );
    sqlx::query("UPDATE manual_agent_sessions SET status='preparing' WHERE id=$1")
        .bind(manual)
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let mut conn = pool.acquire().await.unwrap();
    let view = aq_machine_control::slots::view(&mut conn, node)
        .await
        .unwrap();
    assert_eq!((view.total, view.occupied, view.idle), (2, 2, 0));
    assert!(
        aq_machine_control::management::capacity(&mut conn, node)
            .await
            .unwrap()
            .full()
    );
    sqlx::query("UPDATE manual_agent_sessions SET status='stopped',stopped_at_ms=2 WHERE id=$1")
        .bind(manual)
        .execute(&mut *conn)
        .await
        .unwrap();
    let view = aq_machine_control::slots::view(&mut conn, node)
        .await
        .unwrap();
    assert!(!view.free(1));
    assert!(view.free(2));
    // Lowering the count does not mutate or release either running owner.
    sqlx::query("UPDATE worker_settings SET max_agents=0 WHERE kind='node' AND id=$1")
        .bind(node.to_string())
        .execute(&mut *conn)
        .await
        .unwrap();
    let view = aq_machine_control::slots::view(&mut conn, node)
        .await
        .unwrap();
    assert_eq!((view.total, view.occupied, view.idle), (0, 1, 0));
    assert!(
        sqlx::query_scalar::<_, bool>(
            "SELECT holds_task FROM execution_attempts WHERE attempt_id=$1"
        )
        .bind(attempt)
        .fetch_one(&mut *conn)
        .await
        .unwrap()
    );
}

#[tokio::test]
async fn two_nodes_on_one_machine_each_reserve_three_slots_and_refuse_a_seventh() {
    let f = fixture().await;
    let pool = f.db.pool().await;
    let nodes = [f.node.node_id, Uuid::new_v4()];
    sqlx::query("UPDATE nodes SET machine_key='slot-test-desktop',slot=1 WHERE id=$1")
        .bind(nodes[0])
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at,machine_key,slot) VALUES($1,'sibling','slot-test-sibling','test',2,now(),'slot-test-desktop',2)").bind(nodes[1]).execute(&pool).await.unwrap();
    for node in nodes {
        sqlx::query("UPDATE worker_settings SET max_agents=3 WHERE kind='node' AND id=$1")
            .bind(node.to_string())
            .execute(&pool)
            .await
            .unwrap();
        for slot in 1..=3 {
            // No launch or grant: synthetic native-shaped attempt tests only SQL allocation.
            let id = Uuid::new_v4();
            let mut tx = pool.begin().await.unwrap();
            aq_machine_control::management::lock(&mut tx).await.unwrap();
            sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,kind,state) VALUES($1,$2,$3,$4,$5,'profiled_v1',$6)")
                .bind(id).bind(Uuid::new_v4()).bind(node).bind(Uuid::new_v4()).bind(Uuid::new_v4()).bind(serde_json::json!(f.profiled)).execute(&mut *tx).await.unwrap();
            aq_machine_control::slots::assign(&mut tx, id, node, slot)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
        let mut conn = pool.acquire().await.unwrap();
        let view = aq_machine_control::slots::view(&mut conn, node)
            .await
            .unwrap();
        assert_eq!((view.total, view.occupied, view.idle), (3, 3, 0));
        assert!(!view.free(4));
        assert_eq!(
            aq_machine_control::management::capacity(&mut conn, node)
                .await
                .unwrap()
                .machine_used,
            if node == nodes[0] { 3 } else { 6 }
        );
        drop(conn);
        let mut tx = pool.begin().await.unwrap();
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,kind,state) VALUES($1,$2,$3,$4,$5,'profiled_v1',$6)")
            .bind(id).bind(Uuid::new_v4()).bind(node).bind(Uuid::new_v4()).bind(Uuid::new_v4()).bind(serde_json::json!(f.profiled)).execute(&mut *tx).await.unwrap();
        assert!(
            aq_machine_control::slots::assign(&mut tx, id, node, 1)
                .await
                .is_err()
        );
        tx.rollback().await.unwrap();
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM execution_attempt_slots")
            .fetch_one(&pool)
            .await
            .unwrap(),
        6
    );
}

#[tokio::test]
async fn placement_peers_use_the_same_setting_as_the_polling_node() {
    use aq_server::profiled_execution::placement::{Ledger, facts_in};
    use serde_json::json;
    let f = fixture().await;
    let pool = f.db.pool().await;
    let peer = Uuid::new_v4();
    let template = "a".repeat(64);
    sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at) VALUES($1,'peer','slot-peer','test',2,now())").bind(peer).execute(&pool).await.unwrap();
    for (node, kind) in [(f.node.node_id, "boat"), (peer, "docker-local")] {
        // Synthetic backend facts exercise placement; no runtime is qualified here.
        let registration = json!({"backend":{"kind":kind,"capabilities":["vm-isolation"],"max_concurrent_sandboxes":1,"spend_ceiling":null,"templates":[{"digest":template,"harnesses":[{"harness":"codex","version":"1"}],"estimated_cold_start_ms":1,"estimated_cold_start_cost":{"currency":"USD","micros":0}}]}});
        sqlx::query(
            "INSERT INTO delegated_node_boots(node_id,boot_id,registration) VALUES($1,$2,$3)",
        )
        .bind(node)
        .bind(Uuid::new_v4())
        .bind(registration)
        .execute(&pool)
        .await
        .unwrap();
    }
    sqlx::query("INSERT INTO execution_nodes(node_id,state) VALUES($1,$2)")
        .bind(peer)
        .bind(json!({"slots":3}))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE worker_settings SET max_agents=3 WHERE kind='node' AND id=$1")
        .bind(peer.to_string())
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO intelligence_node_snapshots(node_id,boot_id,sequence,revision,snapshot) VALUES($1,$2,1,1,$3)").bind(peer).bind(Uuid::new_v4()).bind(json!({"expires_at_ms":2000})).execute(&pool).await.unwrap();
    let attempt = Uuid::new_v4();
    sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,kind,state) VALUES($1,$2,$3,$4,$5,'profiled_v1',$6)")
        .bind(attempt).bind(Uuid::new_v4()).bind(peer).bind(Uuid::new_v4()).bind(Uuid::new_v4()).bind(json!(f.profiled)).execute(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    aq_machine_control::slots::assign(&mut tx, attempt, peer, 1)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    let book = aq_accounting::pricing::RateCardBook::default();
    let ledger = Ledger {
        pool: &pool,
        book: &book,
    };
    let facts = facts_in(
        &pool,
        &ledger,
        None,
        f.node.node_id,
        Some(&template),
        "test",
        1000,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        facts.preferred_peer,
        Some(aq_selection::BackendKind::DockerLocal)
    );
    sqlx::query("UPDATE worker_settings SET max_agents=1 WHERE kind='node' AND id=$1")
        .bind(peer.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let facts = facts_in(
        &pool,
        &ledger,
        None,
        f.node.node_id,
        Some(&template),
        "test",
        1000,
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(facts.preferred_peer, None);
}
