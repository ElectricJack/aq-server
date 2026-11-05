//! H5 server follow-up (crisp-ember-25): arbitrary-artifact retention that
//! returns a lifecycle `RetentionReceipt`, AQ-issued `ExecutionPermit`s and the
//! `reconcile_effects` surface.
use aq_lifecycle_api::worker_lifecycle as lc;
use aq_server::{
    AppState, build_worker_router,
    config::Config,
    identity::verifier_from_config,
    source_retention::{Runtime, SourceRetentionConfig},
    store::NewNode,
    test_support::TestDb,
    tokens::{new_node_credential, verifier},
};
use aq_session_host::MAX_PERMIT_MS;
use aq_worker_protocol::*;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use chrono::{Duration, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tower::ServiceExt;
use uuid::Uuid;
fn hash(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}
struct Harness {
    app: Router,
    token: String,
    identity: NodeIdentity,
    task_id: Uuid,
    offer: Offer,
    graph: GraphBinding,
    _dir: tempfile::TempDir,
    _db: TestDb,
}
impl Harness {
    async fn new() -> Self {
        let db = TestDb::create().await;
        let store = db.store().await;
        let now = Utc::now();
        let owner = store
            .bootstrap_owner("owner@localhost", "dev", "owner@localhost", now)
            .await
            .unwrap()
            .unwrap();
        store
            .create_enrollment("enroll", owner.id, now, now + Duration::minutes(5))
            .await
            .unwrap();
        let token = new_node_credential();
        let node = store
            .redeem_enrollment(
                "enroll",
                NewNode {
                    name: "artifacts".into(),
                    credential_hash: verifier(&token),
                    worker_version: "0.1.0".into(),
                    protocol_version: 1,
                },
                now,
            )
            .await
            .unwrap()
            .unwrap();
        let identity = NodeIdentity {
            node_id: node.id,
            boot_id: Uuid::new_v4(),
        };
        store
            .execution_register(&RegisterRequest {
                identity: identity.clone(),
                operation_id: Uuid::new_v4(),
                protocol_version: EXECUTION_PROTOCOL_VERSION,
                worker_version: "0.1.0".into(),
                capabilities: vec![
                    FAKE_HARNESS_CAPABILITY.into(),
                    GRAPH_ACCESS_CAPABILITY.into(),
                ],
            })
            .await
            .unwrap();
        let graph = GraphBinding {
            workspace_id: Uuid::new_v4(),
            profile_id: "profile".into(),
            profile_revision: "1".into(),
            profile_digest: "a".repeat(64),
            context_digest: "b".repeat(64),
        };
        let spec = ExecutionSpec {
            bindings: Some(ExecutionBindings {
                source: None,
                graph: Some(graph.clone()),
            }),
            project_id: Uuid::new_v4(),
            harness: FAKE_HARNESS_CAPABILITY.into(),
            resolved_spec_digest: String::new(),
            fixture_output: "artifacts".into(),
            resources: Resources {
                slots: 1,
                cpu_millis: 100,
                memory_bytes: 1024,
                disk_bytes: 1024,
            },
            required_capabilities: vec![
                FAKE_HARNESS_CAPABILITY.into(),
                GRAPH_ACCESS_CAPABILITY.into(),
            ],
        }
        .seal();
        let offer = Offer {
            offer_id: Uuid::new_v4(),
            identity: identity.clone(),
            attempt: AttemptRef {
                attempt_id: Uuid::new_v4(),
                session_id: Uuid::new_v4(),
                generation: 1,
            },
            spec,
            offer_expires_at_ms: 100000,
            reservation_ttl_ms: 30000,
        };
        let task_id = Uuid::new_v4();
        store.execution_offer(task_id, &offer).await.unwrap();
        let dir = tempfile::tempdir().unwrap();
        let settings = SourceRetentionConfig {
            keep_per_task: 2,
            max_age: 30 * 86400,
            staging_root: dir.path().join("staging"),
            git_root: dir.path().join("git"),
            git_socket: dir.path().join("git.sock"),
            max_staging_bytes: 2 * 1024 * 1024,
            max_bundle_bytes: 4096,
            bulk_concurrency: 2,
            incomplete_ttl_secs: 60,
        };
        let config:Config=toml::from_str("format_version=1\nenvironment='development'\n[server]\nadmin_bind='127.0.0.1:0'\nworker_bind='127.0.0.1:0'\npublic_origin='http://127.0.0.1:8080'\n[auth]\nmode='dev'\ndev_email='owner@localhost'\n[bootstrap]\nowner_email='owner@localhost'\nenabled=true\n").unwrap();
        let state = AppState {
            agent_access: None,
            source_retention: Some(Arc::new(Runtime::new(settings).unwrap())),
            identity: verifier_from_config(&config.auth, reqwest::Client::new()),
            config: Arc::new(config),
            store,
            clock: Arc::new(Utc::now),
        };
        Self {
            app: build_worker_router(state),
            token,
            identity,
            task_id,
            offer,
            graph,
            _dir: dir,
            _db: db,
        }
    }
    fn binding(&self) -> lc::Binding {
        lc::Binding {
            project_id: self.offer.spec.project_id,
            task_id: self.task_id,
            lineage_id: aq_admission::transport::task_lineage_id(
                self.offer.spec.project_id,
                self.graph.workspace_id,
                self.task_id,
            )
            .unwrap(),
            attempt_id: self.offer.attempt.attempt_id,
            attempt_generation: self.offer.attempt.generation,
            session: lc::SessionIdentity::Remote {
                node_id: self.identity.node_id,
                boot_id: self.identity.boot_id,
                attempt_id: self.offer.attempt.attempt_id,
                assignment_generation: self.offer.attempt.generation,
                session_id: self.offer.attempt.session_id,
            },
        }
    }
    async fn json(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let (code, bytes) = self
            .raw(
                "POST",
                path,
                body.to_string().into_bytes(),
                "application/json",
            )
            .await;
        (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
    async fn raw(
        &self,
        method: &str,
        path: &str,
        body: Vec<u8>,
        content: &str,
    ) -> (StatusCode, Vec<u8>) {
        let r = Request::builder()
            .method(method)
            .uri(path)
            .header("x-aq-request", "1")
            .header("content-type", content)
            .header("authorization", format!("Bearer {}", self.token));
        let res = self
            .app
            .clone()
            .oneshot(r.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let code = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (code, bytes.to_vec())
    }
    async fn chunk(
        &self,
        transfer: &str,
        op: Uuid,
        offset: u64,
        bytes: Vec<u8>,
    ) -> (StatusCode, Value) {
        let (code, out) = self
            .raw(
                "PUT",
                &format!(
                    "/v1/execution/artifacts/{transfer}/data?operation_id={op}&offset={offset}&digest={}",
                    hash(&bytes)
                ),
                bytes,
                "application/octet-stream",
            )
            .await;
        (code, serde_json::from_slice(&out).unwrap_or(Value::Null))
    }
    fn begin(&self, op: Uuid, bytes: &[u8]) -> Value {
        json!({"operation_id":op,"attempt_id":self.offer.attempt.attempt_id,
            "artifact":{"id":"checkpoint-1","version":"1","digest":hash(bytes)},
            "role":"checkpoint","bytes":bytes.len()})
    }
}

#[tokio::test]
async fn artifact_upload_commit_returns_lifecycle_receipt_and_replays() {
    let h = Harness::new().await;
    let payload: Vec<u8> = (0..700u32).map(|i| (i % 251) as u8).collect();
    let op = Uuid::new_v4();
    let (code, begun) = h
        .json("/v1/execution/artifacts/begin", h.begin(op, &payload))
        .await;
    assert_eq!(code, StatusCode::OK, "{begun}");
    assert_eq!(begun["state"], "uploading");
    let transfer = begun["transfer_id"].as_str().unwrap().to_string();
    // Same operation, same body: byte-for-byte replay. Different body: conflict.
    let (code, again) = h
        .json("/v1/execution/artifacts/begin", h.begin(op, &payload))
        .await;
    assert_eq!((code, &again), (StatusCode::OK, &begun));
    let (code, _) = h
        .json("/v1/execution/artifacts/begin", h.begin(op, &payload[..10]))
        .await;
    assert_eq!(code, StatusCode::CONFLICT);
    // Chunks are exact-offset; a stale offset is refused and nothing is written.
    let (code, c1) = h
        .chunk(&transfer, Uuid::new_v4(), 0, payload[..400].to_vec())
        .await;
    assert_eq!(code, StatusCode::OK, "{c1}");
    assert_eq!(c1["offset"], 400);
    let (code, _) = h
        .chunk(&transfer, Uuid::new_v4(), 0, payload[..400].to_vec())
        .await;
    assert_eq!(code, StatusCode::CONFLICT);
    // Commit before completion is refused; upload completion is not retention.
    let (code, _) = h
        .json(
            &format!("/v1/execution/artifacts/{transfer}/commit"),
            json!({"operation_id":Uuid::new_v4()}),
        )
        .await;
    assert_eq!(code, StatusCode::CONFLICT);
    let (code, c2) = h
        .chunk(&transfer, Uuid::new_v4(), 400, payload[400..].to_vec())
        .await;
    assert_eq!(code, StatusCode::OK, "{c2}");
    assert_eq!(c2["offset"], 700);
    assert_eq!(c2["state"], "uploading");
    assert!(c2["receipt"].is_null());
    let commit_op = Uuid::new_v4();
    let (code, done) = h
        .json(
            &format!("/v1/execution/artifacts/{transfer}/commit"),
            json!({"operation_id":commit_op}),
        )
        .await;
    assert_eq!(code, StatusCode::OK, "{done}");
    assert_eq!(done["state"], "retained");
    let receipt: lc::RetentionReceipt = serde_json::from_value(done["receipt"].clone()).unwrap();
    // The receipt is accepted unchanged by the lifecycle contract for this binding.
    lc::validate_retention(std::slice::from_ref(&receipt), &h.binding()).unwrap();
    assert_eq!(receipt.role, lc::ArtifactRole::Checkpoint);
    assert_eq!(receipt.bytes, 700);
    assert_eq!(receipt.artifact.digest.as_str(), hash(&payload));
    assert!(receipt.retained_until_ms > receipt.retained_at_ms);
    let (code, replay) = h
        .json(
            &format!("/v1/execution/artifacts/{transfer}/commit"),
            json!({"operation_id":commit_op}),
        )
        .await;
    assert_eq!((code, &replay), (StatusCode::OK, &done));
    // Fetch returns the retained bytes to an attempt in the same lineage.
    let (code, body) = h
        .raw(
            "GET",
            &format!(
                "/v1/execution/artifacts/retained/{}?attempt_id={}&offset=100&limit=1000",
                receipt.receipt_id, h.offer.attempt.attempt_id
            ),
            Vec::new(),
            "application/octet-stream",
        )
        .await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(body, payload[100..].to_vec());
    let (code, _) = h
        .raw(
            "GET",
            &format!(
                "/v1/execution/artifacts/retained/{}?attempt_id={}",
                Uuid::new_v4(),
                h.offer.attempt.attempt_id
            ),
            Vec::new(),
            "application/octet-stream",
        )
        .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
    // A digest mismatch rejects the transfer instead of retaining it.
    let mut wrong = h.begin(Uuid::new_v4(), b"expected");
    wrong["bytes"] = json!(5);
    let (code, b2) = h.json("/v1/execution/artifacts/begin", wrong).await;
    assert_eq!(code, StatusCode::OK, "{b2}");
    let t2 = b2["transfer_id"].as_str().unwrap().to_string();
    let (code, _) = h.chunk(&t2, Uuid::new_v4(), 0, b"other".to_vec()).await;
    assert_eq!(code, StatusCode::OK);
    let (code, _) = h
        .json(
            &format!("/v1/execution/artifacts/{t2}/commit"),
            json!({"operation_id":Uuid::new_v4()}),
        )
        .await;
    assert_eq!(code, StatusCode::CONFLICT);
    let (code, st) = h
        .raw(
            "GET",
            &format!("/v1/execution/artifacts/{t2}/status"),
            Vec::new(),
            "application/json",
        )
        .await;
    assert_eq!(code, StatusCode::OK);
    let st: Value = serde_json::from_slice(&st).unwrap();
    assert_eq!(st["state"], "rejected");
}

#[tokio::test]
async fn execution_permit_issuance_is_sequenced_and_pinned() {
    let h = Harness::new().await;
    let digest = "c".repeat(64);
    let req = |op: Uuid, digest: &str, boot: Uuid, span: u64| {
        json!({"operation_id":op,"attempt_id":h.offer.attempt.attempt_id,"manifest_digest":digest,
            "machine_boot_id":boot,"now_boot_ms":5000,"span_ms":span})
    };
    let op = Uuid::new_v4();
    let (code, first) = h
        .json(
            "/v1/execution/permits",
            req(op, &digest, h.identity.boot_id, 30000),
        )
        .await;
    assert_eq!(code, StatusCode::OK, "{first}");
    let permit: aq_session_host::ExecutionPermit =
        serde_json::from_value(first["permit"].clone()).unwrap();
    assert_eq!(permit.sequence, 1);
    assert_eq!(permit.expires_boot_ms, 35000);
    // The permit validates locally for the attempt's frozen session identity.
    let lc::Binding { session, .. } = h.binding();
    let expected: lc::Digest = serde_json::from_value(json!(digest)).unwrap();
    permit.validate_at(&session, &expected, 6000).unwrap();
    let (code, replay) = h
        .json(
            "/v1/execution/permits",
            req(op, &digest, h.identity.boot_id, 30000),
        )
        .await;
    assert_eq!((code, &replay), (StatusCode::OK, &first));
    let (code, second) = h
        .json(
            "/v1/execution/permits",
            req(Uuid::new_v4(), &digest, h.identity.boot_id, 30000),
        )
        .await;
    assert_eq!(code, StatusCode::OK, "{second}");
    assert_eq!(second["permit"]["sequence"], 2);
    let (code, _) = h
        .json(
            "/v1/execution/permits",
            req(Uuid::new_v4(), &"d".repeat(64), h.identity.boot_id, 30000),
        )
        .await;
    assert_eq!(code, StatusCode::CONFLICT);
    let (code, _) = h
        .json(
            "/v1/execution/permits",
            req(Uuid::new_v4(), &digest, Uuid::new_v4(), 30000),
        )
        .await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    let (code, _) = h
        .json(
            "/v1/execution/permits",
            req(
                Uuid::new_v4(),
                &digest,
                h.identity.boot_id,
                MAX_PERMIT_MS + 1,
            ),
        )
        .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn effects_reconcile_reports_missing_operations() {
    let h = Harness::new().await;
    let known = Uuid::new_v4();
    let unknown = Uuid::new_v4();
    let (code, _) = h
        .json("/v1/execution/permits", json!({"operation_id":known,"attempt_id":h.offer.attempt.attempt_id,
            "manifest_digest":"e".repeat(64),"machine_boot_id":h.identity.boot_id,"now_boot_ms":1,"span_ms":1000}))
        .await;
    assert_eq!(code, StatusCode::OK);
    let (code, pending) = h
        .json(
            "/v1/worker/lifecycle/effects",
            json!({"attempt_id":h.offer.attempt.attempt_id,"operation_ids":[known,unknown]}),
        )
        .await;
    assert_eq!(code, StatusCode::OK, "{pending}");
    let obs: lc::EffectsObservation = serde_json::from_value(pending).unwrap();
    assert_eq!(
        obs,
        lc::EffectsObservation::Pending {
            operation_ids: vec![unknown]
        }
    );
    let (code, done) = h
        .json(
            "/v1/worker/lifecycle/effects",
            json!({"attempt_id":h.offer.attempt.attempt_id,"operation_ids":[known]}),
        )
        .await;
    assert_eq!(code, StatusCode::OK, "{done}");
    assert!(matches!(
        serde_json::from_value::<lc::EffectsObservation>(done).unwrap(),
        lc::EffectsObservation::Reconciled { .. }
    ));
    let (code, _) = h
        .json(
            "/v1/worker/lifecycle/effects",
            json!({"attempt_id":h.offer.attempt.attempt_id,"operation_ids":[known,known]}),
        )
        .await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
}
