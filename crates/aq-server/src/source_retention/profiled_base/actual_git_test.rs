use super::*;
use crate::{config::Config, identity::verifier_from_config, test_support::TestDb};
async fn git(path: &std::path::Path, args: &[&str]) -> String {
    let out = tokio::process::Command::new(std::env::var("AQ_TEST_GIT_EXECUTABLE").unwrap())
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
    String::from_utf8(out.stdout).unwrap().trim().into()
}
async fn call(rt: &Runtime, repo: Uuid, operation: &str, params: Value) -> Value {
    let op = Uuid::new_v4();
    exchange(rt,&json!({"protocol_version":1,"request_id":op.to_string(),"operation":operation,"repository_id":repo,"operation_id":op,"expected_binding_generation":if operation=="repo.register" {0}else{1},"params":params}),MAX_MANIFEST,Duration::from_secs(30)).await.unwrap_or_else(|error| panic!("fixture {operation}: {error:?}"))
}
async fn seed_repository(
    dir: &std::path::Path,
) -> (std::path::PathBuf, aq_worker_protocol::ObjectId) {
    let seed = dir.join("seed");
    std::fs::create_dir(&seed).unwrap();
    git(&seed, &["init", "-b", "main"]).await;
    std::fs::write(seed.join("source"), b"retained pre-offer source\n").unwrap();
    git(&seed, &["add", "."]).await;
    git(&seed, &["commit", "-m", "source"]).await;
    let base = aq_worker_protocol::ObjectId {
        algorithm: aq_worker_protocol::ObjectAlgorithm::Sha1,
        hex: git(&seed, &["rev-parse", "HEAD"]).await,
    };
    (seed, base)
}
async fn start_git(runtime: &Runtime) -> tokio::process::Child {
    let mut child = tokio::process::Command::new(std::env::var("AQ_TEST_GIT_CANDIDATE").unwrap())
        .arg("--state-dir")
        .arg(&runtime.config.git_root)
        .arg("--git-executable")
        .arg(std::env::var("AQ_TEST_GIT_EXECUTABLE").unwrap())
        .arg("--listen")
        .arg(&runtime.config.git_socket)
        .arg("serve")
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..500 {
        if runtime.config.git_socket.exists() {
            break;
        }
        assert!(child.try_wait().unwrap().is_none());
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    child
}
async fn reserve_fixture(
    runtime: &Runtime,
    seed: &std::path::Path,
    base: aq_worker_protocol::ObjectId,
) -> ReservedExport {
    let repo = Uuid::new_v4();
    call(runtime,repo,"repo.register",json!({"identity":"real-preoffer","mode":"central","remote_url":seed,"allow_file_transport":true,"allowed_refs":["refs/heads/main"],"allowed_targets":["refs/heads/main"]})).await;
    call(runtime, repo, "repo.sync", json!({})).await;
    let workspace = call(runtime, repo, "workspace.create", json!({"base":base})).await;
    let checkpoint=call(runtime,repo,"workspace.checkpoint",json!({"workspace_id":workspace["workspace_id"],"expected_generation":1,"expected_head":base,"quiescent":true,"mode":"head"})).await;
    let request = Request {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        project_id: Uuid::new_v4(),
        project_revision: 1,
        repository_id: repo,
        logical_identity: "real-preoffer".into(),
        binding_generation: 1,
        base,
        source_retention_handle: serde_json::from_value(checkpoint["retention_handle"].clone())
            .unwrap(),
        source_observation: aq_admission::ArtifactRef {
            id: aq_admission::Text::new("synthetic-owner-observation").unwrap(),
            version: aq_admission::Text::new("1").unwrap(),
            digest: Digest::of(b"synthetic authorization fixture"),
        },
    };
    crate::profiled_execution::source_exports::synthetic_reserved_export(request, 2_097_152)
        .unwrap()
}
#[tokio::test]
#[ignore = "requires selected immutable aq-git and Git executables; disposable PostgreSQL"]
async fn real_export_exact_retry_and_fresh_inspection_preserve_original_fact() {
    let dir = tempfile::tempdir().unwrap();
    let (seed, base) = seed_repository(dir.path()).await;
    let settings = super::super::SourceRetentionConfig {
        keep_per_task: 2,
        max_age: 30 * 86400,
        staging_root: dir.path().join("staging"),
        git_root: dir.path().join("git"),
        git_socket: dir.path().join("git.sock"),
        max_staging_bytes: 4_194_304,
        max_bundle_bytes: 1_048_576,
        bulk_concurrency: 2,
        incomplete_ttl_secs: 60,
        git_deadline_ms: 30_000,
    };
    let runtime = std::sync::Arc::new(Runtime::new(settings).unwrap());
    let mut child = start_git(&runtime).await;
    let token = reserve_fixture(&runtime, &seed, base).await;
    let db = TestDb::create().await;
    let config:Config=toml::from_str("format_version=1\nenvironment='development'\n[server]\nadmin_bind='127.0.0.1:0'\nworker_bind='127.0.0.1:0'\npublic_origin='http://127.0.0.1:8080'\n[auth]\nmode='dev'\ndev_email='owner@localhost'\n[bootstrap]\nowner_email='owner@localhost'\nenabled=true\n").unwrap();
    let state = AppState {
        internal_work: Default::default(),
        agent_access: None,
        source_retention: Some(runtime.clone()),
        identity: verifier_from_config(&config.auth, reqwest::Client::new()),
        config: std::sync::Arc::new(config),
        store: db.store().await,
        clock: crate::system_clock(),
    };
    let first = verify_export(&state, &token).await.unwrap();
    let original = first.canonical_bytes().unwrap();
    let second = verify_export(&state, &token).await.unwrap();
    assert_eq!(first.manifest_bytes(), second.manifest_bytes());
    assert_eq!(first.bundle_digest(), second.bundle_digest());
    let current = inspect_retained(&state, &token, &first).await.unwrap();
    current.validate_for(&token).unwrap();
    assert!(current.observed_at_ms() >= first.observed_at_ms());
    assert_eq!(first.canonical_bytes().unwrap(), original);
    VerifiedExport::decode_stored(&token, &original).unwrap();
    let bundle = runtime
        .config
        .git_root
        .join("artifacts")
        .join(token.request().operation_id.to_string())
        .join("objects.bundle");
    let output = git(&seed, &["bundle", "list-heads", bundle.to_str().unwrap()]).await;
    assert!(output.contains(&token.request().base.hex));
    assert!(output.contains(&format!(
        "refs/aq-git/transfers/{}",
        token.request().operation_id
    )));
    std::fs::write(bundle, b"corrupt after verified export").unwrap();
    assert!(verify_export(&state, &token).await.is_err());
    assert_eq!(first.canonical_bytes().unwrap(), original);
    child.kill().await.unwrap();
    child.wait().await.unwrap();
    // The synthetic token deliberately does not claim durable SQL ownership,
    // task admission, attempt creation, worker hydration, or provider launch.
}
