use aq_server::{
    AppState, build_worker_router,
    config::Config,
    identity::verifier_from_config,
    source_retention::{Runtime, SourceRetentionConfig},
    store::NewNode,
    test_support::TestDb,
    tokens::{new_node_credential, verifier},
};
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
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tower::ServiceExt;
use uuid::Uuid;
fn hash(b: &[u8]) -> String {
    hex::encode(Sha256::digest(b))
}
struct Harness {
    app: Router,
    admin: Router,
    token: String,
    identity: NodeIdentity,
    offer: Offer,
    manifest: TransferManifest,
    bytes: Vec<u8>,
    dir: tempfile::TempDir,
    pool: sqlx::PgPool,
    _db: TestDb,
}
impl Harness {
    async fn new() -> Self {
        let db = TestDb::create().await;
        let store = db.store().await;
        let pool = db.pool().await;
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
                    name: "source".into(),
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
                capabilities: vec![FAKE_HARNESS_CAPABILITY.into(), SOURCE_CAPABILITY.into()],
            })
            .await
            .unwrap();
        let source = SourceBinding {
            repository_id: Uuid::new_v4(),
            binding_generation: 1,
            logical_identity: "example/repository".into(),
            base: ObjectId {
                algorithm: ObjectAlgorithm::Sha1,
                hex: "a".repeat(40),
            },
        };
        let spec = ExecutionSpec {
            bindings: Some(ExecutionBindings {
                source: Some(source.clone()),
                graph: None,
            }),
            project_id: Uuid::new_v4(),
            harness: FAKE_HARNESS_CAPABILITY.into(),
            resolved_spec_digest: String::new(),
            fixture_output: "source".into(),
            resources: Resources {
                slots: 1,
                cpu_millis: 100,
                memory_bytes: 1024,
                disk_bytes: 1024,
            },
            required_capabilities: vec![FAKE_HARNESS_CAPABILITY.into(), SOURCE_CAPABILITY.into()],
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
        store.execution_offer(Uuid::new_v4(), &offer).await.unwrap();
        let artifact = Uuid::new_v4();
        let checkpoint = ObjectId {
            algorithm: ObjectAlgorithm::Sha1,
            hex: "b".repeat(40),
        };
        // reference-only lets transport tests isolate exact central receipt semantics.
        let bytes=serde_json::to_vec_pretty(&json!({"format_version":1,"repository_id":source.repository_id,"repository_identity":source.logical_identity,"binding_generation":1,"transfer_id":artifact,"object_format":"sha1","base":source.base,"source":checkpoint,"prerequisites":[],"bundle_size":0,"bundle_sha256":null,"producer_installation_id":Uuid::new_v4(),"reference_only":true,"caller_correlation":null})).unwrap();
        let manifest = TransferManifest {
            transfer_id: Uuid::new_v4(),
            role: SourceRole::Checkpoint,
            checkpoint,
            attempt: offer.attempt.clone(),
            resolved_spec_digest: offer.spec.resolved_spec_digest.clone(),
            source,
            artifact_id: artifact,
            manifest_digest: hash(&bytes),
            bundle_digest: None,
            manifest_bytes: bytes.len() as u64,
            bundle_bytes: 0,
        };
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
            incomplete_ttl_secs: 1,
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
            admin: aq_server::build_admin_router(state.clone()),
            app: build_worker_router(state),
            token,
            identity,
            offer,
            manifest,
            bytes,
            dir,
            pool,
            _db: db,
        }
    }
    async fn json(&self, path: &str, body: Value) -> (StatusCode, Value) {
        self.request(
            "POST",
            path,
            body.to_string().into_bytes(),
            "application/json",
            Some(&self.token),
        )
        .await
    }
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Vec<u8>,
        content: &str,
        token: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut r = Request::builder()
            .method(method)
            .uri(path)
            .header("x-aq-request", "1")
            .header("content-type", content);
        if let Some(t) = token {
            r = r.header("authorization", format!("Bearer {t}"));
        }
        let app = if path.starts_with("/api/") {
            &self.admin
        } else {
            &self.app
        };
        let res = app
            .clone()
            .oneshot(r.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let code = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (code, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }
    fn begin_body(&self, op: Uuid) -> Value {
        json!({"identity":self.identity,"operation_id":op,"manifest":self.manifest})
    }
    fn status_body(&self) -> Value {
        json!({"identity":self.identity,"operation_id":Uuid::new_v4(),"transfer_id":self.manifest.transfer_id})
    }
    async fn upload(&self, op: Uuid, offset: u64, bytes: Vec<u8>) -> (StatusCode, Value) {
        self.request(
            "PUT",
            &format!(
                "/v1/execution/sources/{}/manifest?operation_id={op}&offset={offset}&digest={}",
                self.manifest.transfer_id,
                hash(&bytes)
            ),
            bytes,
            "application/octet-stream",
            Some(&self.token),
        )
        .await
    }
}
#[tokio::test]
async fn exact_replay_offsets_scope_and_upload_is_not_retention() {
    let h = Harness::new().await;
    let op = Uuid::new_v4();
    let (c, r) = h
        .json("/v1/execution/sources/begin", h.begin_body(op))
        .await;
    assert_eq!(c, StatusCode::OK, "{r}");
    assert!(r["receipt"].is_null());
    assert_eq!(
        h.json("/v1/execution/sources/begin", h.begin_body(op))
            .await
            .1,
        r
    );
    let mut wrong = h.begin_body(Uuid::new_v4());
    wrong["manifest"]["source"]["repository_id"] = json!(Uuid::new_v4());
    assert_eq!(
        h.json("/v1/execution/sources/begin", wrong).await.0,
        StatusCode::FORBIDDEN
    );
    let chunk = Uuid::new_v4();
    let bytes = h.bytes[..32].to_vec();
    assert_eq!(h.upload(chunk, 0, bytes.clone()).await.0, StatusCode::OK);
    assert_eq!(h.upload(chunk, 0, bytes).await.0, StatusCode::OK);
    assert_eq!(
        h.upload(Uuid::new_v4(), 0, vec![1]).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        h.json("/v1/execution/sources/finalize", h.status_body())
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        h.upload(Uuid::new_v4(), 32, h.bytes[32..].to_vec()).await.0,
        StatusCode::OK
    );
    let (c, r) = h
        .json("/v1/execution/sources/finalize", h.status_body())
        .await;
    assert_eq!(c, StatusCode::SERVICE_UNAVAILABLE, "{r}");
    assert!(
        h.json("/v1/execution/sources/status", h.status_body())
            .await
            .1["receipt"]
            .is_null()
    );
    let state: String =
        sqlx::query_scalar("SELECT state FROM source_transfers WHERE transfer_id=$1")
            .bind(h.manifest.transfer_id)
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(state, "import_pending");
    let status = sqlx::query_scalar::<_, String>(
        "SELECT state::text FROM execution_attempts WHERE attempt_id=$1",
    )
    .bind(h.offer.attempt.attempt_id)
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert!(status.contains("offered"));
    assert_eq!(
        h.request(
            "POST",
            "/v1/execution/sources/status",
            h.status_body().to_string().into_bytes(),
            "application/json",
            Some("aqa_agent_graph_token")
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}
#[tokio::test]
async fn pending_import_retries_same_operation_and_rejects_incompatible_receipt() {
    let h = Harness::new().await;
    assert_eq!(
        h.json("/v1/execution/sources/begin", h.begin_body(Uuid::new_v4()))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        h.upload(Uuid::new_v4(), 0, h.bytes.clone()).await.0,
        StatusCode::OK
    );
    let listener = tokio::net::UnixListener::bind(h.dir.path().join("git.sock")).unwrap();
    let task = tokio::spawn(async move {
        let mut ids = Vec::new();
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let r: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(r["operation"], "artifact.import");
            assert_eq!(r["params"].as_object().unwrap().len(), 1);
            ids.push(r["operation_id"].clone());
            let reply = json!({"protocol_version":2,"request_id":r["request_id"],"operation_id":r["operation_id"],"ok":true,"result":{}});
            reader
                .get_mut()
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        }
        assert_eq!(ids[0], ids[1]);
    });
    for _ in 0..2 {
        assert_eq!(
            h.json("/v1/execution/sources/finalize", h.status_body())
                .await
                .0,
            StatusCode::SERVICE_UNAVAILABLE
        );
    }
    task.await.unwrap();
    sqlx::query(
        "UPDATE source_transfers SET updated_at=now()-interval '10 days' WHERE transfer_id=$1",
    )
    .bind(h.manifest.transfer_id)
    .execute(&h.pool)
    .await
    .unwrap();
    assert!(
        h.dir
            .path()
            .join("git/artifacts/incoming")
            .join(h.manifest.artifact_id.to_string())
            .exists()
    );
}
#[derive(serde::Serialize, serde::Deserialize)]
struct CanonicalManifest {
    format_version: u32,
    repository_id: String,
    repository_identity: String,
    binding_generation: u64,
    transfer_id: String,
    object_format: String,
    base: ObjectId,
    source: ObjectId,
    prerequisites: Vec<ObjectId>,
    bundle_size: u64,
    bundle_sha256: Option<String>,
    producer_installation_id: String,
    reference_only: bool,
    caller_correlation: Value,
}
#[tokio::test]
async fn late_attempt_retains_only_verified_receipt_and_releases_transport_budget() {
    let h = Harness::new().await;
    assert_eq!(
        h.json("/v1/execution/sources/begin", h.begin_body(Uuid::new_v4()))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        h.upload(Uuid::new_v4(), 0, h.bytes.clone()).await.0,
        StatusCode::OK
    );
    sqlx::query("UPDATE execution_attempts SET state=jsonb_set(state,'{status}','\"finished\"'::jsonb),holds_task=false WHERE attempt_id=$1").bind(h.offer.attempt.attempt_id).execute(&h.pool).await.unwrap();
    let m: CanonicalManifest = serde_json::from_slice(&h.bytes).unwrap();
    let canonical = hash(&serde_json::to_vec(&m).unwrap());
    assert_ne!(canonical, h.manifest.manifest_digest);
    let receipt = json!({"receiver_installation_id":Uuid::new_v4(),"repository_id":h.manifest.source.repository_id,"binding_generation":1,"transfer_id":h.manifest.artifact_id,"source":h.manifest.checkpoint,"retention_handle":h.manifest.artifact_id,"manifest_digest":canonical,"retention_state":"active","historical":false,"accepted_at":"2026-10-30T00:00:00Z"});
    let listener = tokio::net::UnixListener::bind(h.dir.path().join("git.sock")).unwrap();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let r: Value = serde_json::from_str(&line).unwrap();
        let reply = json!({"protocol_version":1,"request_id":r["request_id"],"operation_id":r["operation_id"],"ok":true,"result":receipt});
        reader
            .get_mut()
            .write_all(format!("{reply}\n").as_bytes())
            .await
            .unwrap();
    });
    let (c, r) = h
        .json("/v1/execution/sources/finalize", h.status_body())
        .await;
    assert_eq!(c, StatusCode::OK, "{r}");
    assert_eq!(r["receipt"]["manifest_digest"], canonical);
    task.await.unwrap();
    assert!(
        !h.dir
            .path()
            .join("git/artifacts/incoming")
            .join(h.manifest.artifact_id.to_string())
            .exists()
    );
    assert_eq!(
        h.json("/v1/execution/sources/finalize", h.status_body())
            .await
            .1,
        r
    );
    let (state,released,central):(String,bool,String)=sqlx::query_as("SELECT state,staging_released,central_receipt::text FROM source_transfers WHERE transfer_id=$1").bind(h.manifest.transfer_id).fetch_one(&h.pool).await.unwrap();
    assert_eq!(state, "retained");
    assert!(released);
    assert!(central.contains("receiver_installation_id"));
    let status: String =
        sqlx::query_scalar("SELECT state->>'status' FROM execution_attempts WHERE attempt_id=$1")
            .bind(h.offer.attempt.attempt_id)
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(status, "finished");
}
#[tokio::test]
async fn stale_upload_cleanup_and_limits_preserve_unknown_imports() {
    let h = Harness::new().await;
    assert_eq!(
        h.json("/v1/execution/sources/begin", h.begin_body(Uuid::new_v4()))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        h.upload(Uuid::new_v4(), 0, h.bytes[..8].to_vec()).await.0,
        StatusCode::OK
    );
    let oversized = vec![0; 256 * 1024 + 1];
    assert_eq!(
        h.upload(Uuid::new_v4(), 8, oversized).await.0,
        StatusCode::BAD_REQUEST
    );
    sqlx::query(
        "UPDATE source_transfers SET updated_at=now()-interval '10 days' WHERE transfer_id=$1",
    )
    .bind(h.manifest.transfer_id)
    .execute(&h.pool)
    .await
    .unwrap();
    let mut next = h.begin_body(Uuid::new_v4());
    next["manifest"]["transfer_id"] = json!(Uuid::new_v4());
    next["manifest"]["artifact_id"] = json!(Uuid::new_v4());
    assert_eq!(
        h.json("/v1/execution/sources/begin", next).await.0,
        StatusCode::OK
    );
    assert!(
        !h.dir
            .path()
            .join("staging")
            .join(h.manifest.transfer_id.to_string())
            .exists()
    );
    let state: String =
        sqlx::query_scalar("SELECT state FROM source_transfers WHERE transfer_id=$1")
            .bind(h.manifest.transfer_id)
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert_eq!(state, "rejected");
}
async fn git_cmd(path: &std::path::Path, args: &[&str]) -> String {
    let git = std::env::var("AQ_TEST_GIT_EXECUTABLE").expect("set pinned bookworm Git binary");
    let out = tokio::process::Command::new(git)
        .current_dir(path)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.test")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.test")
        .output()
        .await
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().into()
}
async fn real_call(socket: &std::path::Path, repo: Uuid, operation: &str, params: Value) -> Value {
    let op = Uuid::new_v4();
    let r = json!({"protocol_version":1,"request_id":op.to_string(),"operation":operation,"repository_id":repo,"operation_id":op,"expected_binding_generation":if operation=="repo.register" {0}else{1},"params":params});
    let stream = tokio::net::UnixStream::connect(socket).await.unwrap();
    let mut reader = BufReader::new(stream);
    reader
        .get_mut()
        .write_all(format!("{r}\n").as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    reader.read_line(&mut line).await.unwrap();
    let result: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(result["ok"], true, "{result}");
    result["result"].clone()
}
#[tokio::test]
#[ignore = "qualification requires immutable aq-git and bookworm Git2.56.0 binaries"]
async fn selected_real_aq_git_import_survives_worker_volume_loss_and_base_download_is_scoped() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let mut h = Harness::new().await;
    let worker = tempfile::tempdir().unwrap();
    let seed = worker.path().join("seed");
    tokio::fs::create_dir(&seed).await.unwrap();
    git_cmd(&seed, &["init", "-b", "main"]).await;
    tokio::fs::write(seed.join("source"), "base\n")
        .await
        .unwrap();
    git_cmd(&seed, &["add", "."]).await;
    git_cmd(&seed, &["commit", "-m", "base"]).await;
    let base = git_cmd(&seed, &["rev-parse", "HEAD"]).await;
    tokio::fs::write(seed.join("source"), "survives complete worker loss\n")
        .await
        .unwrap();
    git_cmd(&seed, &["add", "."]).await;
    git_cmd(&seed, &["commit", "-m", "source"]).await;
    let source = git_cmd(&seed, &["rev-parse", "HEAD"]).await;
    let transfer_ref = format!("refs/aq-git/transfers/{}", h.manifest.artifact_id);
    git_cmd(&seed, &["update-ref", &transfer_ref, &source]).await;
    let bundle = worker.path().join("source.bundle");
    git_cmd(
        &seed,
        &["bundle", "create", bundle.to_str().unwrap(), &transfer_ref],
    )
    .await;
    let bundle_bytes = tokio::fs::read(&bundle).await.unwrap();
    h.manifest.source.base.hex = base.clone();
    h.manifest.checkpoint.hex = source.clone();
    h.offer.spec.bindings.as_mut().unwrap().source = Some(h.manifest.source.clone());
    h.offer.spec = h.offer.spec.clone().seal();
    h.manifest.resolved_spec_digest = h.offer.spec.resolved_spec_digest.clone();
    sqlx::query("UPDATE execution_attempts SET state=jsonb_set(state,'{offer}',$2::jsonb) WHERE attempt_id=$1").bind(h.offer.attempt.attempt_id).bind(serde_json::to_string(&h.offer).unwrap()).execute(&h.pool).await.unwrap();
    let mut m: CanonicalManifest = serde_json::from_slice(&h.bytes).unwrap();
    m.base = h.manifest.source.base.clone();
    m.source = h.manifest.checkpoint.clone();
    m.bundle_size = bundle_bytes.len() as u64;
    m.bundle_sha256 = Some(hash(&bundle_bytes));
    m.reference_only = false;
    h.bytes = serde_json::to_vec_pretty(&m).unwrap();
    h.manifest.manifest_digest = hash(&h.bytes);
    h.manifest.manifest_bytes = h.bytes.len() as u64;
    h.manifest.bundle_digest = m.bundle_sha256;
    h.manifest.bundle_bytes = m.bundle_size;
    let binary = std::env::var("AQ_TEST_GIT_CANDIDATE").expect("set immutable aq-git candidate");
    let git = std::env::var("AQ_TEST_GIT_EXECUTABLE").unwrap();
    let socket = h.dir.path().join("git.sock");
    let mut child = tokio::process::Command::new(binary)
        .arg("--state-dir")
        .arg(h.dir.path().join("git"))
        .arg("--git-executable")
        .arg(git)
        .arg("--listen")
        .arg(&socket)
        .arg("serve")
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        assert!(child.try_wait().unwrap().is_none());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let repo = h.manifest.source.repository_id;
    real_call(&socket,repo,"repo.register",json!({"identity":h.manifest.source.logical_identity,"mode":"central","remote_url":seed,"allow_file_transport":true,"allowed_refs":["refs/heads/main"],"allowed_targets":["refs/heads/main"]})).await;
    assert_eq!(
        h.json("/v1/execution/sources/begin", h.begin_body(Uuid::new_v4()))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        h.upload(Uuid::new_v4(), 0, h.bytes.clone()).await.0,
        StatusCode::OK
    );
    let path = format!(
        "/v1/execution/sources/{}/bundle?operation_id={}&offset=0&digest={}",
        h.manifest.transfer_id,
        Uuid::new_v4(),
        hash(&bundle_bytes)
    );
    assert_eq!(
        h.request(
            "PUT",
            &path,
            bundle_bytes,
            "application/octet-stream",
            Some(&h.token)
        )
        .await
        .0,
        StatusCode::OK
    );
    let (code, receipt) = h
        .json("/v1/execution/sources/finalize", h.status_body())
        .await;
    assert_eq!(code, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["receipt"]["source"]["hex"], source);
    worker.close().unwrap(); // Complete stopped producer volume disappears.
    let candidate=real_call(&socket,repo,"candidate.create",json!({"base":h.manifest.source.base,"sources":[{"oid":h.manifest.checkpoint,"retention_handle":receipt["receipt"]["retention_handle"]}],"commit":{"name":"Coordinator","email":"coordinator@example.test","message":"central-only candidate"}})).await;
    assert_eq!(candidate["state"], "ready");
    // A trusted operator provisions only the frozen base from a real retained handle.
    let cp = real_call(
        &socket,
        repo,
        "workspace.create",
        json!({"base":h.manifest.source.base}),
    )
    .await;
    let checkpoint=real_call(&socket,repo,"workspace.checkpoint",json!({"workspace_id":cp["workspace_id"],"expected_generation":1,"expected_head":h.manifest.source.base,"quiescent":true,"mode":"head"})).await;
    let body = json!({"attempt_id":h.offer.attempt.attempt_id,"operation_id":Uuid::new_v4(),"retention_handle":checkpoint["retention_handle"]});
    let (c, base_manifest) = h.json("/api/source-bases/provision", body).await;
    assert_eq!(c, StatusCode::OK, "{base_manifest}");
    assert_eq!(base_manifest["role"], "base");
    let base_path = format!("/v1/execution/sources/base/{}", h.offer.attempt.attempt_id);
    assert_eq!(
        h.request(
            "GET",
            &base_path,
            vec![],
            "application/json",
            Some(&h.token)
        )
        .await
        .1,
        base_manifest
    );
    let res = h
        .app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("{base_path}/manifest?offset=0&limit=256"))
                .header("authorization", format!("Bearer {}", h.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    assert_eq!(
        res.into_body().collect().await.unwrap().to_bytes().len(),
        256
    );
    assert_eq!(
        h.request(
            "GET",
            &format!("/v1/execution/sources/base/{}", Uuid::new_v4()),
            vec![],
            "application/json",
            Some(&h.token)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}
#[tokio::test]
async fn slow_import_does_not_hold_attempt_or_node_locks_and_control_status_progresses() {
    let h = Harness::new().await;
    assert_eq!(
        h.json("/v1/execution/sources/begin", h.begin_body(Uuid::new_v4()))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        h.upload(Uuid::new_v4(), 0, h.bytes.clone()).await.0,
        StatusCode::OK
    );
    let listener = tokio::net::UnixListener::bind(h.dir.path().join("git.sock")).unwrap();
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let signal = entered.clone();
    let go = release.clone();
    let socket = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        signal.notify_one();
        go.notified().await;
        let r: Value = serde_json::from_str(&line).unwrap();
        reader.get_mut().write_all(format!("{}\n",json!({"protocol_version":2,"request_id":r["request_id"],"operation_id":r["operation_id"],"ok":true,"result":{}})).as_bytes()).await.unwrap();
    });
    let app = h.app.clone();
    let token = h.token.clone();
    let body = h.status_body();
    let pending = tokio::spawn(async move {
        app.oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/execution/sources/finalize")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    let (code, _) = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        h.json("/v1/execution/sources/status", h.status_body()),
    )
    .await
    .expect("control status must progress during import");
    assert_eq!(code, StatusCode::OK);
    sqlx::query(
        "UPDATE source_transfers SET updated_at=now()-interval '10 days' WHERE transfer_id=$1",
    )
    .bind(h.manifest.transfer_id)
    .execute(&h.pool)
    .await
    .unwrap();
    let mut next = h.begin_body(Uuid::new_v4());
    next["manifest"]["transfer_id"] = json!(Uuid::new_v4());
    next["manifest"]["artifact_id"] = json!(Uuid::new_v4());
    assert_eq!(
        h.json("/v1/execution/sources/begin", next).await.0,
        StatusCode::OK
    );
    assert!(
        h.dir
            .path()
            .join("git/artifacts/incoming")
            .join(h.manifest.artifact_id.to_string())
            .exists(),
        "TTL cleanup must not remove protected import bytes"
    );
    tokio::time::timeout(std::time::Duration::from_secs(1),sqlx::query("UPDATE execution_attempts SET state=jsonb_set(state,'{status}','\"cancelling\"'::jsonb) WHERE attempt_id=$1").bind(h.offer.attempt.attempt_id).execute(&h.pool)).await.expect("cancellation row must not be locked by socket I/O").unwrap();
    let (code, body) = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        h.json(
            &format!("/api/workers/{}/revoke", h.identity.node_id),
            json!({}),
        ),
    )
    .await
    .expect("node revocation must progress during import");
    assert_eq!(code, StatusCode::NO_CONTENT, "{body}");
    release.notify_one();
    socket.await.unwrap();
    assert_eq!(pending.await.unwrap().status(), StatusCode::UNAUTHORIZED);
    assert_eq!(
        h.json("/v1/execution/sources/status", h.status_body())
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn invalid_pre_dispatch_manifest_is_rejected_without_leaking_quota() {
    let h = Harness::new().await;
    assert_eq!(
        h.json("/v1/execution/sources/begin", h.begin_body(Uuid::new_v4()))
            .await
            .0,
        StatusCode::OK
    );
    let mut corrupt = h.bytes.clone();
    corrupt[0] = b'!';
    assert_eq!(h.upload(Uuid::new_v4(), 0, corrupt).await.0, StatusCode::OK);
    assert_eq!(
        h.json("/v1/execution/sources/finalize", h.status_body())
            .await
            .0,
        StatusCode::CONFLICT
    );
    let (state, released, dispatched): (String, bool, bool) = sqlx::query_as(
        "SELECT state,staging_released,dispatch_intent FROM source_transfers WHERE transfer_id=$1",
    )
    .bind(h.manifest.transfer_id)
    .fetch_one(&h.pool)
    .await
    .unwrap();
    assert_eq!(state, "rejected");
    assert!(released);
    assert!(!dispatched);
    assert!(
        !h.dir
            .path()
            .join("staging")
            .join(h.manifest.transfer_id.to_string())
            .exists()
    );
    assert_eq!(
        h.json("/v1/execution/sources/finalize", h.status_body())
            .await
            .0,
        StatusCode::CONFLICT
    );
}
#[test]
fn runtime_rejects_symlink_before_changing_target_permissions() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target");
    std::fs::create_dir(&target).unwrap();
    std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755)).unwrap();
    let link = dir.path().join("link");
    symlink(&target, &link).unwrap();
    let cfg = SourceRetentionConfig {
        keep_per_task: 2,
        max_age: 30 * 86400,
        staging_root: link,
        git_root: dir.path().join("git"),
        git_socket: dir.path().join("git.sock"),
        max_staging_bytes: 4096,
        max_bundle_bytes: 1024,
        bulk_concurrency: 1,
        incomplete_ttl_secs: 1,
    };
    assert!(Runtime::new(cfg).is_err());
    assert_eq!(
        std::fs::metadata(target).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

#[tokio::test]
async fn released_attempt_refuses_new_intents_but_preserves_owned_transfer_recovery() {
    let h = Harness::new().await;
    let op = Uuid::new_v4();
    let body = h.begin_body(op);
    let original = h.json("/v1/execution/sources/begin", body.clone()).await;
    assert_eq!(original.0, StatusCode::OK);
    let disposition = ResultResponse {
        report_id: Uuid::new_v4(),
        disposition: ResultDisposition::Accepted,
        retention_release: true,
    };
    sqlx::query("UPDATE execution_attempts SET state=jsonb_set(state,'{disposition}',$2::jsonb),holds_task=false WHERE attempt_id=$1").bind(h.offer.attempt.attempt_id).bind(serde_json::to_string(&disposition).unwrap()).execute(&h.pool).await.unwrap();
    assert_eq!(h.json("/v1/execution/sources/begin", body).await, original);
    let mut next = h.begin_body(Uuid::new_v4());
    next["manifest"]["transfer_id"] = json!(Uuid::new_v4());
    next["manifest"]["artifact_id"] = json!(Uuid::new_v4());
    assert_eq!(
        h.json("/v1/execution/sources/begin", next).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        h.json("/v1/execution/sources/status", h.status_body())
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        h.upload(Uuid::new_v4(), 0, h.bytes.clone()).await.0,
        StatusCode::OK
    );
    assert_eq!(
        h.json("/v1/execution/sources/finalize", h.status_body())
            .await
            .0,
        StatusCode::SERVICE_UNAVAILABLE
    );
    let holds: bool =
        sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
            .bind(h.offer.attempt.attempt_id)
            .fetch_one(&h.pool)
            .await
            .unwrap();
    assert!(!holds);
}

#[tokio::test]
async fn credential_rotation_after_observed_node_wait_cannot_create_transfer_and_new_key_replays() {
    let h = Harness::new().await;
    let op = Uuid::new_v4();
    let body = h.begin_body(op);
    let mut lock = h.pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM nodes WHERE id=$1 FOR UPDATE")
        .bind(h.identity.node_id)
        .fetch_one(&mut *lock)
        .await
        .unwrap();
    let app = h.app.clone();
    let token = h.token.clone();
    let input = body.clone();
    let blocked = tokio::spawn(async move {
        app.oneshot(
            Request::post("/v1/execution/sources/begin")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(input.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
    });
    tokio::time::timeout(std::time::Duration::from_secs(2),async {loop {
        let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query LIKE 'SELECT id FROM nodes WHERE id=%credential_hash=%FOR UPDATE')").fetch_one(&h.pool).await.unwrap();
        if waiting{break;}tokio::task::yield_now().await;
    }}).await.expect("must observe original credential's node lock wait");
    let replacement = new_node_credential();
    sqlx::query("UPDATE nodes SET credential_hash=$2 WHERE id=$1")
        .bind(h.identity.node_id)
        .bind(verifier(&replacement))
        .execute(&mut *lock)
        .await
        .unwrap();
    lock.commit().await.unwrap();
    assert_eq!(blocked.await.unwrap().status(), StatusCode::UNAUTHORIZED);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM source_transfers")
        .fetch_one(&h.pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let call = || {
        h.request(
            "POST",
            "/v1/execution/sources/begin",
            body.to_string().into_bytes(),
            "application/json",
            Some(&replacement),
        )
    };
    let (code, first) = call().await;
    assert_eq!(code, StatusCode::OK, "{first}");
    assert_eq!(
        call().await.1,
        first,
        "new current credential replays original immutable operation"
    );
}

#[tokio::test]
async fn credential_rotation_during_import_keeps_uncertain_intent_and_current_key_recovers_same_op()
{
    // Synthetic socket owner isolates auth/recovery; actual Git retention is
    // independently qualified by phase_source_retention's explicit pinned cases.
    let h = Harness::new().await;
    assert_eq!(
        h.json("/v1/execution/sources/begin", h.begin_body(Uuid::new_v4()))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        h.upload(Uuid::new_v4(), 0, h.bytes.clone()).await.0,
        StatusCode::OK
    );
    let native: CanonicalManifest = serde_json::from_slice(&h.bytes).unwrap();
    let canonical = hash(&serde_json::to_vec(&native).unwrap());
    let acknowledgement = json!({"receiver_installation_id":Uuid::new_v4(),"repository_id":h.manifest.source.repository_id,"binding_generation":1,"transfer_id":h.manifest.artifact_id,"source":h.manifest.checkpoint,"retention_handle":h.manifest.artifact_id,"manifest_digest":canonical,"retention_state":"active","historical":false});
    let listener = tokio::net::UnixListener::bind(h.dir.path().join("git.sock")).unwrap();
    let entered = Arc::new(tokio::sync::Semaphore::new(0));
    let resume = Arc::new(tokio::sync::Semaphore::new(0));
    let signal = entered.clone();
    let go = resume.clone();
    let peer = tokio::spawn(async move {
        let mut operations = vec![];
        for index in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let request: Value = serde_json::from_str(&line).unwrap();
            operations.push(request["operation_id"].clone());
            if index == 0 {
                signal.add_permits(1);
                go.acquire().await.unwrap().forget();
            }
            let reply = json!({"protocol_version":1,"request_id":request["request_id"],"operation_id":request["operation_id"],"ok":true,"result":acknowledgement});
            reader
                .get_mut()
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        }
        assert_eq!(
            operations[0], operations[1],
            "recovery must reuse admitted import identity"
        );
    });
    let app = h.app.clone();
    let token = h.token.clone();
    let input = h.status_body();
    let pending = tokio::spawn(async move {
        app.oneshot(
            Request::post("/v1/execution/sources/finalize")
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Body::from(input.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.acquire())
        .await
        .unwrap()
        .unwrap()
        .forget();
    let replacement = new_node_credential();
    sqlx::query("UPDATE nodes SET credential_hash=$2 WHERE id=$1")
        .bind(h.identity.node_id)
        .bind(verifier(&replacement))
        .execute(&h.pool)
        .await
        .unwrap();
    resume.add_permits(1);
    assert_eq!(pending.await.unwrap().status(), StatusCode::UNAUTHORIZED);
    let pending_fact:Value=sqlx::query_scalar("SELECT jsonb_build_object('state',state,'dispatch',dispatch_intent,'receipt',receipt,'staging_released',staging_released) FROM source_transfers WHERE transfer_id=$1").bind(h.manifest.transfer_id).fetch_one(&h.pool).await.unwrap();
    assert_eq!(
        pending_fact,
        json!({"state":"import_pending","dispatch":true,"receipt":null,"staging_released":false})
    );
    assert!(
        h.dir
            .path()
            .join("git/artifacts/incoming")
            .join(h.manifest.artifact_id.to_string())
            .exists()
    );
    let body = h.status_body();
    let (code, recovered) = h
        .request(
            "POST",
            "/v1/execution/sources/finalize",
            body.to_string().into_bytes(),
            "application/json",
            Some(&replacement),
        )
        .await;
    assert_eq!(code, StatusCode::OK, "{recovered}");
    assert!(recovered["receipt"].is_object());
    assert_eq!(
        h.request(
            "POST",
            "/v1/execution/sources/finalize",
            body.to_string().into_bytes(),
            "application/json",
            Some(&replacement)
        )
        .await
        .1,
        recovered
    );
    tokio::time::timeout(std::time::Duration::from_secs(2), peer)
        .await
        .unwrap()
        .unwrap();
}

#[path = "source_retention/delivery.rs"]
mod delivery;
#[path = "source_retention/publication.rs"]
mod publication;
