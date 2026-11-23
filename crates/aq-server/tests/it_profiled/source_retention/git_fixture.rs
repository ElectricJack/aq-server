use super::*;

pub(super) async fn git_cmd(path: &std::path::Path, args: &[&str]) -> String {
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
pub(super) async fn real_call(
    socket: &std::path::Path,
    repo: Uuid,
    operation: &str,
    params: Value,
) -> Value {
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

pub(super) struct WorkerBundle {
    worker: tempfile::TempDir,
    pub(super) seed: std::path::PathBuf,
    pub(super) base: String,
    source: String,
    bytes: Vec<u8>,
}
impl WorkerBundle {
    pub(super) async fn create(h: &mut Harness) -> Self {
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
        let bundle = Self {
            worker,
            seed,
            base,
            source,
            bytes: bundle_bytes,
        };
        bundle.bind(h).await;
        bundle
    }
    async fn bind(&self, h: &mut Harness) {
        h.manifest.source.base.hex = self.base.clone();
        h.manifest.checkpoint.hex = self.source.clone();
        h.offer.spec.bindings.as_mut().unwrap().source = Some(h.manifest.source.clone());
        h.offer.spec = h.offer.spec.clone().seal();
        h.manifest.resolved_spec_digest = h.offer.spec.resolved_spec_digest.clone();
        sqlx::query("UPDATE execution_attempts SET state=jsonb_set(state,'{offer}',$2::jsonb) WHERE attempt_id=$1").bind(h.offer.attempt.attempt_id).bind(serde_json::to_string(&h.offer).unwrap()).execute(&h.pool).await.unwrap();
        let mut m: CanonicalManifest = serde_json::from_slice(&h.bytes).unwrap();
        m.base = h.manifest.source.base.clone();
        m.source = h.manifest.checkpoint.clone();
        m.bundle_size = self.bytes.len() as u64;
        m.bundle_sha256 = Some(hash(&self.bytes));
        m.reference_only = false;
        h.bytes = serde_json::to_vec_pretty(&m).unwrap();
        h.manifest.manifest_digest = hash(&h.bytes);
        h.manifest.manifest_bytes = h.bytes.len() as u64;
        h.manifest.bundle_digest = m.bundle_sha256;
        h.manifest.bundle_bytes = m.bundle_size;
    }
    pub(super) async fn retain(self, h: &Harness) -> Value {
        h.upload_manifest().await;
        let path = format!(
            "/v1/execution/sources/{}/bundle?operation_id={}&offset=0&digest={}",
            h.manifest.transfer_id,
            Uuid::new_v4(),
            hash(&self.bytes)
        );
        assert_eq!(
            h.request(
                Request::put(&path).header("content-type", "application/octet-stream"),
                self.bytes,
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
        assert_eq!(receipt["receipt"]["source"]["hex"], self.source);
        self.worker.close().unwrap(); // Complete stopped producer volume disappears.
        receipt
    }
}

pub(super) async fn start_real_import(
    h: &Harness,
) -> (
    std::path::PathBuf,
    aq_delivery::source_transport::test_peer::TestGit,
) {
    let socket = h.dir.path().join("git.sock");
    let child = aq_delivery::source_transport::test_peer::TestGit::start(
        &h.dir.path().join("git"),
        &socket,
        200,
    )
    .await;
    (socket, child)
}
pub(super) async fn check_base_download(h: &Harness, socket: &std::path::Path, repo: Uuid) {
    // A trusted operator provisions only the frozen base from a real retained handle.
    let cp = real_call(
        socket,
        repo,
        "workspace.create",
        json!({"base":h.manifest.source.base}),
    )
    .await;
    let checkpoint=real_call(socket,repo,"workspace.checkpoint",json!({"workspace_id":cp["workspace_id"],"expected_generation":1,"expected_head":h.manifest.source.base,"quiescent":true,"mode":"head"})).await;
    let body = json!({"attempt_id":h.offer.attempt.attempt_id,"operation_id":Uuid::new_v4(),"retention_handle":checkpoint["retention_handle"]});
    let (c, base_manifest) = h.json("/api/source-bases/provision", body).await;
    assert_eq!(c, StatusCode::OK, "{base_manifest}");
    assert_eq!(base_manifest["role"], "base");
    let base_path = format!("/v1/execution/sources/base/{}", h.offer.attempt.attempt_id);
    assert_eq!(
        h.request(
            Request::get(&base_path).header("content-type", "application/json"),
            vec![],
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
            Request::get(&format!("/v1/execution/sources/base/{}", Uuid::new_v4()))
                .header("content-type", "application/json"),
            vec![],
            Some(&h.token)
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}
