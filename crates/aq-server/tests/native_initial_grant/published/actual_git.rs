//! Actual external Git owner across publication, scoped source reads and grant.
//! Runtime/material installation claims remain synthetic: no worker/model launch.
use super::*;
use aq_admission::control::{ControlReply, decode_reply};
use http_body_util::BodyExt;
use std::{path::Path, process::Stdio};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tower::ServiceExt;

async fn git(path: &Path, args: &[&str]) -> String {
    let output = tokio::process::Command::new(
        std::env::var("AQ_TEST_GIT_EXECUTABLE").expect("explicit Git executable required"),
    )
    .current_dir(path)
    .args(args)
    .env_clear()
    .env("PATH", "/usr/bin:/bin")
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
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}
async fn rpc(socket: &Path, repo: Uuid, operation: &str, params: Value) -> Value {
    let operation_id = Uuid::new_v4();
    let body = json!({"protocol_version":1,"request_id":operation_id.to_string(),"repository_id":repo,"operation_id":operation_id,"expected_binding_generation":if operation=="repo.register" {0}else{1},"operation":operation,"params":params});
    let stream = tokio::net::UnixStream::connect(socket).await.unwrap();
    let mut reader = BufReader::new(stream);
    reader
        .get_mut()
        .write_all(format!("{body}\n").as_bytes())
        .await
        .unwrap();
    let mut line = String::new();
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        reader.read_line(&mut line),
    )
    .await
    .unwrap()
    .unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["ok"], true, "{response}");
    response["result"].clone()
}
struct Actual {
    prepared: Prepared,
    process: tokio::process::Child,
    socket: std::path::PathBuf,
    seed: std::path::PathBuf,
    base: String,
}
struct Unpublished {
    case: Case,
    process: tokio::process::Child,
    socket: std::path::PathBuf,
    seed: std::path::PathBuf,
    base: String,
}
async fn setup_actual_unpublished() -> Unpublished {
    let mut case = native_offer::setup().await;
    let dir = case.env.env._dir.path();
    let seed = dir.join("actual-seed");
    std::fs::create_dir(&seed).unwrap();
    git(&seed, &["init", "-b", "main"]).await;
    std::fs::write(seed.join("source"), b"actual source bytes\n").unwrap();
    git(&seed, &["add", "."]).await;
    git(&seed, &["commit", "-m", "source"]).await;
    let base = git(&seed, &["rev-parse", "HEAD"]).await;
    let socket = dir.join("actual-git.sock");
    let root = dir.join("actual-git");
    let mut process = tokio::process::Command::new(
        std::env::var("AQ_TEST_GIT_CANDIDATE").expect("explicit aq-git candidate required"),
    )
    .args([
        "--state-dir",
        root.to_str().unwrap(),
        "--git-executable",
        &std::env::var("AQ_TEST_GIT_EXECUTABLE").unwrap(),
        "--listen",
        socket.to_str().unwrap(),
        "serve",
    ])
    .kill_on_drop(true)
    .stdout(Stdio::null())
    .spawn()
    .unwrap();
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        assert!(process.try_wait().unwrap().is_none());
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert!(socket.exists());
    let repo = case.env.frozen.project.binding.repository_id;
    rpc(&socket,repo,"repo.register",json!({"identity":"native-real-git-fixture","mode":"central","remote_url":seed,"allow_file_transport":true,"allowed_refs":["refs/heads/main"],"allowed_targets":["refs/heads/main"]})).await;
    rpc(&socket, repo, "repo.sync", json!({})).await;
    let oid = json!({"algorithm":"sha1","hex":base});
    let workspace = rpc(&socket, repo, "workspace.create", json!({"base":oid})).await;
    let checkpoint=rpc(&socket,repo,"workspace.checkpoint",json!({"workspace_id":workspace["workspace_id"],"expected_generation":1,"expected_head":oid,"quiescent":true,"mode":"head"})).await;
    let handle: Uuid = serde_json::from_value(checkpoint["retention_handle"].clone()).unwrap();
    let mut config = (*case.env.env.state.config).clone();
    let mut profiled = (**config.profiled_execution.as_ref().unwrap()).clone();
    profiled.revision += 1;
    profiled.projects[0].retention_handle = handle;
    profiled.projects[0].source = aq_admission::SourceSelection::Immutable {
        object: aq_admission::ObjectId {
            format: aq_admission::ObjectFormat::Sha1,
            hex: base.clone(),
        },
    };
    config.profiled_execution = Some(Arc::new(profiled));
    case.env.env.state.config = Arc::new(config);
    case.env.env.state.source_retention = Some(Arc::new(
        aq_server::source_retention::Runtime::new(
            aq_server::source_retention::SourceRetentionConfig {
                keep_per_task: 2,
                max_age: 30 * 86400,
                git_root: root,
                git_socket: socket.clone(),
                staging_root: dir.join("actual-staging"),
                max_staging_bytes: 4_194_304,
                max_bundle_bytes: 1_048_576,
                bulk_concurrency: 1,
                incomplete_ttl_secs: 60,
            },
        )
        .unwrap(),
    ));
    aq_server::profiled_execution::policies::install(&case.env.env.state)
        .await
        .unwrap();
    Unpublished {
        case,
        process,
        socket,
        seed,
        base,
    }
}
async fn setup_actual() -> Actual {
    let Unpublished {
        case,
        process,
        socket,
        seed,
        base,
    } = setup_actual_unpublished().await;
    let (status, body) = native_offer::poll(&case).await;
    assert_eq!(status, 200, "{body}");
    let ControlReply::Poll(mut page) = decode_reply(&serde_json::to_vec(&body).unwrap()).unwrap()
    else {
        panic!("{body}")
    };
    page.validate_for(&case.request).unwrap();
    assert_eq!(page.offers.len(), 1);
    let mut prepared = setup_from(case, page.offers.remove(0)).await;
    // Actual-clock qualification: no post-Accept clock substitution or TTL stretch.
    prepared.case.env.env.state.clock = Arc::new(chrono::Utc::now);
    Actual {
        prepared,
        process,
        socket,
        seed,
        base,
    }
}
async fn download(c: &Prepared, member: &str) -> Vec<u8> {
    let expected = c
        .request
        .artifacts
        .iter()
        .find(|r| {
            r.artifact
                .id
                .as_str()
                .starts_with(&format!("aq.source-{member}/"))
        })
        .unwrap()
        .bytes;
    assert!(expected > 0 && expected <= 1_048_576);
    let mut bytes = Vec::new();
    while (bytes.len() as u64) < expected {
        let response = aq_server::build_worker_router(c.case.env.env.state.clone())
            .oneshot(
                axum::http::Request::get(format!(
                    "/v1/worker/profiled/source/{}/{}?offset={}&limit=128",
                    c.request.binding.attempt_id,
                    member,
                    bytes.len()
                ))
                .header(
                    "authorization",
                    format!("Bearer {}", c.case.env.env.credential),
                )
                .header("aq-source-api-version", "1")
                .body(axum::body::Body::empty())
                .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(response.headers()["aq-source-api-version"], "1");
        let part = response.into_body().collect().await.unwrap().to_bytes();
        assert!(!part.is_empty());
        bytes.extend_from_slice(&part);
        assert!((bytes.len() as u64) <= expected);
    }
    bytes
}
#[tokio::test]
#[ignore = "qualification requires explicit actual aq-git and Git executable paths"]
async fn real_git_publication_download_and_initial_grant_share_exact_export() {
    let mut actual = setup_actual().await;
    let c = &actual.prepared;
    let manifest_bytes = download(c, "manifest").await;
    let bundle = download(c, "bundle").await;
    let manifest: Value = serde_json::from_slice(&manifest_bytes).unwrap();
    assert_eq!(manifest["base"]["hex"], actual.base);
    assert_eq!(manifest["source"]["hex"], actual.base);
    assert_eq!(manifest["bundle_size"], bundle.len());
    assert_eq!(manifest["bundle_sha256"], Digest::of(&bundle).as_str());
    for (name, bytes) in [("manifest", &manifest_bytes), ("bundle", &bundle)] {
        let receipt = c
            .request
            .artifacts
            .iter()
            .find(|r| {
                r.artifact
                    .id
                    .as_str()
                    .starts_with(&format!("aq.source-{name}/"))
            })
            .unwrap();
        assert_eq!(receipt.digest, Digest::of(bytes));
        assert_eq!(receipt.bytes, bytes.len() as u64);
    }
    let file = actual.seed.join("downloaded.bundle");
    std::fs::write(&file, &bundle).unwrap();
    git(&actual.seed, &["bundle", "verify", file.to_str().unwrap()]).await;
    let first = grant(c).await.unwrap();
    first
        .validate_for(&c.request, &c.plan, &c.offer, first.delivery.nonce)
        .unwrap();
    let next = grant(c).await.unwrap();
    assert_eq!(
        serde_json::to_value(&first.original).unwrap(),
        serde_json::to_value(&next.original).unwrap()
    );
    assert_eq!(first.versions, next.versions);
    assert_eq!(first.execution_deadline_ms, next.execution_deadline_ms);
    actual.process.kill().await.unwrap();
    actual.process.wait().await.unwrap();
}
#[tokio::test]
#[ignore = "qualification requires explicit actual aq-git and Git executable paths"]
async fn actual_released_export_refuses_grant_without_erasing_preparation() {
    let mut actual = setup_actual().await;
    let c = &actual.prepared;
    let manifest: Value = serde_json::from_slice(&download(c, "manifest").await).unwrap();
    let repo = c.case.env.frozen.project.binding.repository_id;
    let handle = &manifest["transfer_id"];
    let pin = rpc(
        &actual.socket,
        repo,
        "retention.inspect",
        json!({"handle_id":handle}),
    )
    .await;
    rpc(&actual.socket,repo,"retention.release",json!({"handle_id":handle,"expected_generation":pin["generation"],"refs":pin["refs"],"reason":"qualification explicit export retirement"})).await;
    assert!(grant(c).await.is_err());
    ungranted(c).await;
    actual.process.kill().await.unwrap();
    actual.process.wait().await.unwrap();
}

#[path = "actual_git/worker_assembly.rs"]
mod worker_assembly;

#[path = "actual_git/accepted_v2.rs"]
mod accepted_v2;

#[path = "actual_git/worker_refresh_assembly.rs"]
mod worker_refresh_assembly;
