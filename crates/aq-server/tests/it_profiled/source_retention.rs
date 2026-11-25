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
#[path = "source_retention/harness.rs"]
mod harness;
use harness::Harness;
#[path = "source_retention/git_fixture.rs"]
mod git_fixture;
use git_fixture::{WorkerBundle, check_base_download, git_cmd, real_call, start_real_import};
#[path = "source_retention/pending.rs"]
mod pending;
use pending::{assert_unconfirmed_offer, paused_retry_peer};
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
    assert_unconfirmed_offer(&h).await;
    assert_eq!(
        h.request(
            Request::post("/v1/execution/sources/status")
                .header("content-type", "application/json"),
            h.status_body().to_string().into_bytes(),
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
    h.upload_manifest().await;
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
    late_attempt_retains(false).await;
}

/// An import that completed after an earlier call gave up is answered as historical
/// on the retry of the same operation; it must still be recorded (gate 25b).
#[tokio::test]
async fn retry_after_a_late_import_completion_records_the_historical_receipt() {
    late_attempt_retains(true).await;
}

async fn late_attempt_retains(historical: bool) {
    let h = Harness::new().await;
    h.upload_manifest().await;
    sqlx::query("UPDATE execution_attempts SET state=jsonb_set(state,'{status}','\"finished\"'::jsonb),holds_task=false WHERE attempt_id=$1").bind(h.offer.attempt.attempt_id).execute(&h.pool).await.unwrap();
    let m: CanonicalManifest = serde_json::from_slice(&h.bytes).unwrap();
    let canonical = hash(&serde_json::to_vec(&m).unwrap());
    assert_ne!(canonical, h.manifest.manifest_digest);
    let receipt = json!({"receiver_installation_id":Uuid::new_v4(),"repository_id":h.manifest.source.repository_id,"binding_generation":1,"transfer_id":h.manifest.artifact_id,"source":h.manifest.checkpoint,"retention_handle":h.manifest.artifact_id,"manifest_digest":canonical,"retention_state":"active","historical":historical,"accepted_at":"2026-10-30T00:00:00Z"});
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
#[tokio::test]
#[ignore = "qualification requires immutable aq-git and bookworm Git2.56.0 binaries"]
async fn selected_real_aq_git_import_survives_worker_volume_loss_and_base_download_is_scoped() {
    let _ = tracing_subscriber::fmt().with_test_writer().try_init();
    let mut h = Harness::new().await;
    let bundle = WorkerBundle::create(&mut h).await;
    let (socket, child) = start_real_import(&h).await;
    let repo = h.manifest.source.repository_id;
    real_call(&socket,repo,"repo.register",json!({"identity":h.manifest.source.logical_identity,"mode":"central","remote_url":bundle.seed,"allow_file_transport":true,"allowed_refs":["refs/heads/main"],"allowed_targets":["refs/heads/main"]})).await;
    let receipt = bundle.retain(&h).await;
    let candidate=real_call(&socket,repo,"candidate.create",json!({"base":h.manifest.source.base,"sources":[{"oid":h.manifest.checkpoint,"retention_handle":receipt["receipt"]["retention_handle"]}],"commit":{"name":"Coordinator","email":"coordinator@example.test","message":"central-only candidate"}})).await;
    assert_eq!(candidate["state"], "ready");
    check_base_download(&h, &socket, repo).await;
    child.stop().await;
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
        git_deadline_ms: 30_000,
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

#[path = "source_retention/credentials.rs"]
mod credentials;
#[path = "source_retention/delivery.rs"]
mod delivery;
#[path = "source_retention/publication.rs"]
mod publication;
