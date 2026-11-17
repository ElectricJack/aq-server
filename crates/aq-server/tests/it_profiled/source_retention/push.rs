//! Finished work arrives by `git push`. Every push here is a real `git push`
//! subprocess against the worker router served on a local HTTP port.
use super::*;
use std::{
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

/// Bytes Git can neither compress nor delta, so sizes on the wire are known.
fn noise(seed: &str, len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len + 32);
    let mut block = Sha256::digest(seed.as_bytes());
    while out.len() < len {
        out.extend_from_slice(&block);
        block = Sha256::digest(block);
    }
    out.truncate(len);
    out
}
fn tree_bytes(path: &Path) -> usize {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if !meta.is_dir() {
        return meta.len() as usize;
    }
    std::fs::read_dir(path)
        .unwrap()
        .map(|e| tree_bytes(&e.unwrap().path()))
        .sum()
}
async fn git(dir: &Path, args: &[&str]) -> std::process::Output {
    tokio::process::Command::new("git")
        .current_dir(dir)
        .args(args)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.test")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.test")
        .output()
        .await
        .unwrap()
}
async fn git_ok(dir: &Path, args: &[&str]) -> String {
    let out = git(dir, args).await;
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().into()
}
fn stderr(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}
fn oid(hex: &str) -> ObjectId {
    ObjectId {
        algorithm: ObjectAlgorithm::Sha1,
        hex: hex.into(),
    }
}

/// A worker's repository with a base commit on `main` and the attempt's work
/// on top of it, and a server whose attempt is frozen on that base.
struct Pushing {
    h: Harness,
    seed: tempfile::TempDir,
    base: String,
    work: String,
    url: String,
    server: tokio::task::JoinHandle<()>,
    aq_git: Option<tokio::process::Child>,
}
impl Drop for Pushing {
    fn drop(&mut self) {
        self.server.abort();
    }
}
impl Pushing {
    /// The central pool is a plain bare repository holding the base, as aq-git
    /// lays it out; the import itself is answered by `fake_aq_git`.
    async fn start(base_bytes: usize, work_bytes: usize, max_bundle_bytes: usize) -> Self {
        let p = Self::prepare(base_bytes, work_bytes, max_bundle_bytes).await;
        let root = p.h.dir.path().join("git");
        let pool = root
            .join("repositories")
            .join(p.h.manifest.source.repository_id.to_string())
            .join("objects.git");
        std::fs::create_dir_all(&pool).unwrap();
        git_ok(&pool, &["init", "-q", "--bare"]).await;
        let refspec = format!("{}:refs/heads/main", p.base);
        git_ok(
            &pool,
            &["fetch", "-q", p.seed.path().to_str().unwrap(), &refspec],
        )
        .await;
        std::fs::write(
            root.join("format.json"),
            json!({"format_version":1,"installation_id":Uuid::new_v4(),"created_at":"2026-11-17T00:00:00Z"})
                .to_string(),
        )
        .unwrap();
        p
    }
    /// The central pool is owned by the real aq-git, which fetched the base.
    async fn start_real(base_bytes: usize, work_bytes: usize, max_bundle_bytes: usize) -> Self {
        let mut p = Self::prepare(base_bytes, work_bytes, max_bundle_bytes).await;
        let binary =
            std::env::var("AQ_TEST_GIT_CANDIDATE").expect("set immutable aq-git candidate");
        let git = std::env::var("AQ_TEST_GIT_EXECUTABLE").expect("set Git executable");
        let socket = p.h.dir.path().join("git.sock");
        let mut child = tokio::process::Command::new(binary)
            .arg("--state-dir")
            .arg(p.h.dir.path().join("git"))
            .arg("--git-executable")
            .arg(git)
            .arg("--listen")
            .arg(&socket)
            .arg("serve")
            .kill_on_drop(true)
            .stdout(std::process::Stdio::null())
            .spawn()
            .unwrap();
        for _ in 0..500 {
            if socket.exists() {
                break;
            }
            assert!(child.try_wait().unwrap().is_none());
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        p.aq_git = Some(child);
        let source = &p.h.manifest.source;
        real_call(&socket, source.repository_id, "repo.register", json!({"identity":source.logical_identity,"mode":"central","remote_url":p.seed.path(),"allow_file_transport":true,"allowed_refs":["refs/heads/main"],"allowed_targets":["refs/heads/main"]})).await;
        real_call(&socket, source.repository_id, "repo.sync", json!({})).await;
        p
    }
    async fn prepare(base_bytes: usize, work_bytes: usize, max_bundle_bytes: usize) -> Self {
        let seed = tempfile::tempdir().unwrap();
        let dir = seed.path();
        git_ok(dir, &["init", "-q", "-b", "main"]).await;
        std::fs::write(dir.join("base.bin"), noise("base", base_bytes)).unwrap();
        git_ok(dir, &["add", "."]).await;
        git_ok(dir, &["commit", "-q", "-m", "base"]).await;
        let base = git_ok(dir, &["rev-parse", "HEAD"]).await;
        git_ok(dir, &["checkout", "-q", "-b", "work"]).await;
        std::fs::write(dir.join("work.bin"), noise("work", work_bytes)).unwrap();
        git_ok(dir, &["add", "."]).await;
        git_ok(dir, &["commit", "-q", "-m", "work"]).await;
        let work = git_ok(dir, &["rev-parse", "HEAD"]).await;
        let h = Harness::with(base.clone(), 64 * MIB as u64, max_bundle_bytes as u64, 3600).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let app = h.app.clone();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            h,
            seed,
            base,
            work,
            url,
            server,
            aq_git: None,
        }
    }
    fn attempt(&self) -> Uuid {
        self.h.offer.attempt.attempt_id
    }
    fn attempt_ref(&self) -> String {
        format!("refs/aq/attempts/{}", self.attempt())
    }
    /// `git push` as the worker runs it: the credential travels in a header.
    async fn push_as(
        &self,
        token: &str,
        attempt: Uuid,
        refspec: &str,
        extra: &[&str],
    ) -> std::process::Output {
        let header = format!("http.extraHeader=Authorization: Bearer {token}");
        let url = format!("{}/v1/execution/git/{attempt}", self.url);
        let mut args = vec!["-c", &header];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["push", &url, refspec]);
        git(self.seed.path(), &args).await
    }
    async fn push(&self, commit: &str) -> std::process::Output {
        let refspec = format!("{commit}:{}", self.attempt_ref());
        self.push_as(&self.h.token, self.attempt(), &refspec, &[])
            .await
    }
    fn quarantine(&self) -> PathBuf {
        self.h
            .dir
            .path()
            .join("staging/pushed")
            .join(format!("{}.git", self.attempt()))
    }
    async fn quarantined(&self) -> String {
        git_ok(
            &self.quarantine(),
            &["for-each-ref", "--format=%(objectname) %(refname)"],
        )
        .await
    }
    fn body(&self, transfer: Uuid, checkpoint: &str) -> Value {
        json!({"identity":self.h.identity,"operation_id":Uuid::new_v4(),"transfer_id":transfer,"attempt":self.h.offer.attempt,"resolved_spec_digest":self.h.offer.spec.resolved_spec_digest,"source":self.h.manifest.source,"checkpoint":oid(checkpoint)})
    }
    async fn admit(&self, transfer: Uuid, checkpoint: &str) -> (StatusCode, Value) {
        self.h
            .json(
                "/v1/execution/sources/pushed",
                self.body(transfer, checkpoint),
            )
            .await
    }
    async fn rows(&self) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM source_transfers")
            .fetch_one(&self.h.pool)
            .await
            .unwrap()
    }
    async fn refs_status(&self, attempt: Uuid, token: Option<&str>) -> StatusCode {
        self.h
            .request(
                "GET",
                &format!("/v1/execution/git/{attempt}/info/refs?service=git-receive-pack"),
                vec![],
                "application/json",
                token,
            )
            .await
            .0
    }
}

/// Stands in for aq-git's `artifact.import`. It checks the artifact the server
/// staged the way the real import begins (manifest identity, bundle digest, the
/// one advertised ref, prerequisites present in the pool) and acknowledges it.
fn fake_aq_git(p: &Pushing) -> Arc<AtomicUsize> {
    let root = p.h.dir.path().join("git");
    let listener = tokio::net::UnixListener::bind(p.h.dir.path().join("git.sock")).unwrap();
    let imports = Arc::new(AtomicUsize::new(0));
    let seen = imports.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let r: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(r["operation"], "artifact.import");
            let handle = r["params"]["artifact_handle"].as_str().unwrap().to_owned();
            let incoming = root.join("artifacts/incoming").join(&handle);
            let m: CanonicalManifest =
                serde_json::from_slice(&std::fs::read(incoming.join("manifest.json")).unwrap())
                    .unwrap();
            let bundle = incoming.join("objects.bundle");
            let bytes = std::fs::read(&bundle).unwrap();
            let format: Value =
                serde_json::from_slice(&std::fs::read(root.join("format.json")).unwrap()).unwrap();
            assert_eq!(m.transfer_id, handle);
            assert_eq!(m.repository_id, r["repository_id"].as_str().unwrap());
            assert_eq!(m.bundle_size, bytes.len() as u64);
            assert_eq!(m.bundle_sha256, Some(hash(&bytes)));
            assert_eq!(m.producer_installation_id, format["installation_id"]);
            assert!(!m.reference_only);
            let pool = root
                .join("repositories")
                .join(&m.repository_id)
                .join("objects.git");
            let bundle = bundle.to_str().unwrap();
            git_ok(&pool, &["bundle", "verify", "-q", bundle]).await;
            assert_eq!(
                git_ok(&pool, &["bundle", "list-heads", bundle]).await,
                format!("{} refs/aq-git/transfers/{handle}", m.source.hex)
            );
            let listed = String::from_utf8(
                bytes[..bytes.windows(2).position(|w| w == b"\n\n").unwrap()].to_vec(),
            )
            .unwrap();
            let listed: Vec<&str> = listed
                .lines()
                .filter_map(|l| l.strip_prefix('-'))
                .map(|l| &l[..40])
                .collect();
            let declared: Vec<&str> = m.prerequisites.iter().map(|o| o.hex.as_str()).collect();
            assert_eq!(listed, declared);
            let receipt = json!({"receiver_installation_id":format["installation_id"],"repository_id":m.repository_id,"binding_generation":m.binding_generation,"transfer_id":handle,"source":m.source,"retention_handle":handle,"manifest_digest":hash(&serde_json::to_vec(&m).unwrap()),"retention_state":"active","historical":false,"accepted_at":"2026-11-17T00:00:00Z"});
            seen.fetch_add(1, Ordering::SeqCst);
            let reply = json!({"protocol_version":1,"request_id":r["request_id"],"operation_id":r["operation_id"],"ok":true,"result":receipt});
            reader
                .get_mut()
                .write_all(format!("{reply}\n").as_bytes())
                .await
                .unwrap();
        }
    });
    imports
}

#[tokio::test]
async fn pushed_work_is_retained_once_and_feeds_result_intake() {
    let p = Pushing::start(2 * MIB, MIB + MIB / 4, 16 * MIB).await;
    let imports = fake_aq_git(&p);
    let out = p.push(&p.work).await;
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(
        p.quarantined().await,
        format!("{} {}", p.work, p.attempt_ref())
    );
    // The pack is thin: the work arrived, the 2 MiB of base history did not.
    let held = tree_bytes(&p.quarantine().join("objects"));
    assert!(held > MIB + MIB / 4 && held < 2 * MIB, "{held}");
    // A push alone is not retention.
    assert_eq!(p.rows().await, 0);

    let transfer = Uuid::new_v4();
    let (code, first) = p.admit(transfer, &p.work).await;
    assert_eq!(code, StatusCode::OK, "{first}");
    assert_eq!(first["transfer_id"], json!(transfer));
    assert_eq!(first["receipt"]["source"]["hex"], p.work);
    let row = sqlx::query("SELECT state,staging_released,manifest,manifest_offset,bundle_offset,receipt,central_receipt FROM source_transfers WHERE transfer_id=$1")
        .bind(transfer).fetch_one(&p.h.pool).await.unwrap();
    use sqlx::Row;
    let manifest: TransferManifest = serde_json::from_value(row.get("manifest")).unwrap();
    let receipt: CentralRetentionReceipt = serde_json::from_value(row.get("receipt")).unwrap();
    assert_eq!(row.get::<String, _>("state"), "retained");
    assert!(row.get::<bool, _>("staging_released"));
    assert_eq!(
        row.get::<i64, _>("manifest_offset") as u64,
        manifest.manifest_bytes
    );
    assert_eq!(
        row.get::<i64, _>("bundle_offset") as u64,
        manifest.bundle_bytes
    );
    assert_eq!(manifest.role, SourceRole::Checkpoint);
    assert_eq!(manifest.checkpoint.hex, p.work);
    assert_eq!(manifest.attempt, p.h.offer.attempt);
    assert!(manifest.bundle_bytes > (MIB + MIB / 4) as u64);
    assert_eq!(
        row.get::<Value, _>("central_receipt")["retention_handle"],
        json!(manifest.artifact_id)
    );
    assert_eq!(first["receipt"], json!(receipt));
    // Nothing of the transport is left once the commit is retained.
    assert!(!p.quarantine().exists());
    assert!(
        !p.h.dir
            .path()
            .join("staging")
            .join(transfer.to_string())
            .exists()
    );
    assert!(
        !p.h.dir
            .path()
            .join("git/artifacts/incoming")
            .join(manifest.artifact_id.to_string())
            .exists()
    );

    // A retry after a lost response is answered from the row.
    let (code, again) = p.admit(transfer, &p.work).await;
    assert_eq!((code, &again), (StatusCode::OK, &first));
    assert_eq!(imports.load(Ordering::SeqCst), 1);
    // The same transfer cannot name another checkpoint.
    assert_eq!(p.admit(transfer, &p.base).await.0, StatusCode::CONFLICT);
    assert_eq!(p.rows().await, 1);

    // Result intake depends on the retained row and takes it as it is.
    let store = p.h._db.store().await;
    let offer = &p.h.offer;
    store
        .execution_accept(
            &AcceptRequest {
                identity: offer.identity.clone(),
                operation_id: Uuid::new_v4(),
                offer_id: offer.offer_id,
                attempt: offer.attempt.clone(),
                resolved_spec_digest: offer.spec.resolved_spec_digest.clone(),
                reservation_id: Uuid::new_v4(),
                reservation_deadline_ms: 80_000,
                reservation_remaining_ms: 20_000,
            },
            50_000u64,
        )
        .await
        .unwrap();
    let mut report = ResultReport {
        identity: offer.identity.clone(),
        report_id: Uuid::new_v4(),
        supersedes_report_id: None,
        attempt: offer.attempt.clone(),
        resolved_spec_digest: offer.spec.resolved_spec_digest.clone(),
        outcome: ProcessOutcome::Success,
        stop_certainty: StopCertainty::ConfirmedStopped,
        claimed_success: true,
        output: "finished".into(),
        artifacts: vec![ArtifactRef {
            artifact_id: manifest.artifact_id,
            digest: manifest.manifest_digest.clone(),
            bytes: manifest.manifest_bytes + manifest.bundle_bytes,
            central_receipt: Some(Uuid::new_v4().to_string()),
        }],
        pending_operation_ids: vec![],
    };
    assert!(store.execution_result(&report, 60_000u64).await.is_err());
    report.artifacts[0].central_receipt = Some(receipt.receipt_id.to_string());
    let accepted = store.execution_result(&report, 60_000u64).await.unwrap();
    assert_eq!(accepted.disposition, ResultDisposition::Accepted);
    assert!(accepted.retention_release);
}

#[tokio::test]
async fn an_attempt_without_changes_is_retained_from_its_base() {
    let p = Pushing::start(64 * KIB, KIB, 16 * MIB).await;
    let imports = fake_aq_git(&p);
    let out = p.push(&p.base).await;
    assert!(out.status.success(), "{}", stderr(&out));
    // The server already holds the base: nothing but the ref arrived.
    assert!(tree_bytes(&p.quarantine().join("objects")) < KIB);
    let transfer = Uuid::new_v4();
    let (code, body) = p.admit(transfer, &p.base).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    assert_eq!(body["receipt"]["source"]["hex"], p.base);
    // With nothing after the base the bundle carries the base itself.
    let bundle: i64 = sqlx::query_scalar(
        "SELECT (manifest->>'bundle_bytes')::bigint FROM source_transfers WHERE transfer_id=$1",
    )
    .bind(transfer)
    .fetch_one(&p.h.pool)
    .await
    .unwrap();
    assert!(bundle > 64 * KIB as i64);
    assert_eq!(imports.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_push_reaches_only_its_own_attempt_ref_under_its_own_credential() {
    let p = Pushing::start(64 * KIB, 16 * KIB, 16 * MIB).await;
    let token = p.h.token.clone();
    // Any ref but the attempt's own is hidden, and a hidden ref is not written.
    for name in [
        "refs/heads/main".to_owned(),
        "refs/aq/base".to_owned(),
        format!("refs/aq/attempts/{}", Uuid::new_v4()),
        format!("refs/aq-git/transfers/{}", Uuid::new_v4()),
    ] {
        let refspec = format!("{}:{name}", p.work);
        let out = p.push_as(&token, p.attempt(), &refspec, &[]).await;
        assert!(!out.status.success(), "{name} was accepted");
        assert!(stderr(&out).contains("hidden ref"), "{}", stderr(&out));
    }
    assert_eq!(p.quarantined().await, "");

    // Another attempt's URL is not this node's to push to.
    let other = Uuid::new_v4();
    let refspec = format!("{}:refs/aq/attempts/{other}", p.work);
    let out = p.push_as(&token, other, &refspec, &[]).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("403"), "{}", stderr(&out));
    assert_eq!(
        p.refs_status(other, Some(&token)).await,
        StatusCode::FORBIDDEN
    );

    // Another node's credential does not reach this attempt; none reaches nothing.
    let store = p.h._db.store().await;
    let now = Utc::now();
    store
        .create_enrollment("enroll-other", p.h.owner, now, now + Duration::minutes(5))
        .await
        .unwrap();
    let foreign = new_node_credential();
    store
        .redeem_enrollment(
            "enroll-other",
            NewNode {
                name: "other".into(),
                credential_hash: verifier(&foreign),
                worker_version: "0.1.0".into(),
                protocol_version: 1,
            },
            now,
        )
        .await
        .unwrap()
        .unwrap();
    let out = p
        .push_as(
            &foreign,
            p.attempt(),
            &format!("{}:{}", p.work, p.attempt_ref()),
            &[],
        )
        .await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("403"), "{}", stderr(&out));
    assert_eq!(
        p.refs_status(p.attempt(), Some(&foreign)).await,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        p.refs_status(p.attempt(), None).await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(p.quarantined().await, "");
    let quarantines = std::fs::read_dir(p.h.dir.path().join("staging/pushed"))
        .unwrap()
        .count();
    assert_eq!(quarantines, 1);

    // The dumb protocol and fetching are not served.
    for path in [
        format!("/v1/execution/git/{}/info/refs", p.attempt()),
        format!(
            "/v1/execution/git/{}/info/refs?service=git-upload-pack",
            p.attempt()
        ),
    ] {
        let (code, _) =
            p.h.request("GET", &path, vec![], "application/json", Some(&token))
                .await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{path}");
    }
    let (code, _) =
        p.h.request(
            "POST",
            &format!("/v1/execution/git/{}/git-receive-pack", p.attempt()),
            b"0000".to_vec(),
            "application/x-git-receive-pack-request",
            Some(&token),
        )
        .await;
    assert_eq!(code, StatusCode::OK);
    let gzip = Request::builder()
        .method("POST")
        .uri(format!(
            "/v1/execution/git/{}/git-receive-pack",
            p.attempt()
        ))
        .header("authorization", format!("Bearer {token}"))
        .header("content-type", "application/x-git-receive-pack-request")
        .header("content-encoding", "gzip")
        .body(Body::from("0000"))
        .unwrap();
    assert_eq!(
        p.h.app.clone().oneshot(gzip).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );

    // Once retention is released the attempt accepts no further work.
    let out = p.push(&p.work).await;
    assert!(out.status.success(), "{}", stderr(&out));
    let disposition = ResultResponse {
        report_id: Uuid::new_v4(),
        disposition: ResultDisposition::Accepted,
        retention_release: true,
    };
    sqlx::query("UPDATE execution_attempts SET state=jsonb_set(state,'{disposition}',$2::jsonb),holds_task=false WHERE attempt_id=$1").bind(p.attempt()).bind(serde_json::to_string(&disposition).unwrap()).execute(&p.h.pool).await.unwrap();
    let out = p.push(&p.work).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("409"), "{}", stderr(&out));
    assert_eq!(
        p.admit(Uuid::new_v4(), &p.work).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(p.rows().await, 0);
}

#[tokio::test]
async fn admission_refuses_what_was_not_pushed_as_a_descendant_of_the_base() {
    let p = Pushing::start(64 * KIB, 16 * KIB, 16 * MIB).await;
    // Nothing was pushed yet.
    assert_eq!(
        p.admit(Uuid::new_v4(), &p.work).await.0,
        StatusCode::CONFLICT
    );
    let out = p.push(&p.work).await;
    assert!(out.status.success(), "{}", stderr(&out));
    // The checkpoint must be the pushed commit, not merely one the server holds.
    let (code, body) = p.admit(Uuid::new_v4(), &p.base).await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    assert_eq!(
        p.admit(Uuid::new_v4(), &"c".repeat(40)).await.0,
        StatusCode::CONFLICT
    );
    // The request must repeat the frozen assignment and come from its node.
    for (pointer, value) in [
        ("/source/repository_id", json!(Uuid::new_v4())),
        ("/source/base/hex", json!("d".repeat(40))),
        ("/resolved_spec_digest", json!("e".repeat(64))),
        ("/attempt/generation", json!(2)),
        ("/identity/node_id", json!(Uuid::new_v4())),
    ] {
        let mut body = p.body(Uuid::new_v4(), &p.work);
        *body.pointer_mut(pointer).unwrap() = value;
        let (code, _) = p.h.json("/v1/execution/sources/pushed", body).await;
        assert_eq!(code, StatusCode::FORBIDDEN, "{pointer}");
    }
    let mut unknown = p.body(Uuid::new_v4(), &p.work);
    unknown["manifest"] = json!({});
    assert!(
        p.h.json("/v1/execution/sources/pushed", unknown)
            .await
            .0
            .is_client_error()
    );

    // A commit that does not descend from the base can be pushed to the
    // attempt's own ref, and is refused at admission.
    let dir = p.seed.path();
    git_ok(dir, &["checkout", "-q", "--orphan", "lone"]).await;
    git_ok(dir, &["commit", "-q", "-m", "unrelated"]).await;
    let lone = git_ok(dir, &["rev-parse", "HEAD"]).await;
    let refspec = format!("+{lone}:{}", p.attempt_ref());
    let out = p.push_as(&p.h.token, p.attempt(), &refspec, &[]).await;
    assert!(out.status.success(), "{}", stderr(&out));
    let (code, body) = p.admit(Uuid::new_v4(), &lone).await;
    assert_eq!(code, StatusCode::CONFLICT, "{body}");
    // A ref that is not a commit is refused as well.
    let tree = git_ok(dir, &["rev-parse", &format!("{}^{{tree}}", p.work)]).await;
    let refspec = format!("+{tree}:{}", p.attempt_ref());
    let out = p.push_as(&p.h.token, p.attempt(), &refspec, &[]).await;
    assert!(out.status.success(), "{}", stderr(&out));
    assert_eq!(p.admit(Uuid::new_v4(), &tree).await.0, StatusCode::CONFLICT);

    assert_eq!(p.rows().await, 0);
    let staged: Vec<_> = std::fs::read_dir(p.h.dir.path().join("staging"))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(staged, ["pushed"]);
}

#[tokio::test]
async fn a_push_larger_than_the_bundle_limit_is_refused() {
    let p = Pushing::start(16 * KIB, 600 * KIB, 256 * KIB).await;
    // Sent in one piece, the declared length alone refuses it.
    let out = p.push(&p.work).await;
    assert!(!out.status.success());
    assert!(stderr(&out).contains("413"), "{}", stderr(&out));
    // Sent in chunks of unknown total, it is cut off while streaming.
    let refspec = format!("{}:{}", p.work, p.attempt_ref());
    let out = p
        .push_as(
            &p.h.token,
            p.attempt(),
            &refspec,
            &["-c", "http.postBuffer=65536"],
        )
        .await;
    assert!(!out.status.success(), "{}", stderr(&out));
    assert_eq!(p.quarantined().await, "");
    assert!(tree_bytes(&p.quarantine()) < 64 * KIB);
    assert_eq!(
        p.admit(Uuid::new_v4(), &p.work).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(p.rows().await, 0);
}

#[tokio::test]
#[ignore = "qualification requires immutable aq-git and Git executables (AQ_TEST_GIT_CANDIDATE, AQ_TEST_GIT_EXECUTABLE)"]
async fn real_aq_git_retains_pushed_work_and_still_refuses_attribute_trees() {
    let p = Pushing::start_real(512 * KIB, 64 * KIB, 16 * MIB).await;
    let socket = p.h.dir.path().join("git.sock");
    let repo = p.h.manifest.source.repository_id;

    // An attempt that changed nothing: the checkpoint is the base.
    let out = p.push(&p.base).await;
    assert!(out.status.success(), "{}", stderr(&out));
    let (code, unchanged) = p.admit(Uuid::new_v4(), &p.base).await;
    assert_eq!(code, StatusCode::OK, "{unchanged}");

    // The attempt's work: a thin push, admitted through the real import.
    let out = p.push(&p.work).await;
    assert!(out.status.success(), "{}", stderr(&out));
    let held = tree_bytes(&p.quarantine().join("objects"));
    assert!(held > 64 * KIB && held < 512 * KIB, "{held}");
    let transfer = Uuid::new_v4();
    let (code, receipt) = p.admit(transfer, &p.work).await;
    assert_eq!(code, StatusCode::OK, "{receipt}");
    assert_eq!(receipt["receipt"]["source"]["hex"], p.work);
    assert_eq!(p.admit(transfer, &p.work).await.1, receipt);
    let state: String =
        sqlx::query_scalar("SELECT state FROM source_transfers WHERE transfer_id=$1")
            .bind(transfer)
            .fetch_one(&p.h.pool)
            .await
            .unwrap();
    assert_eq!(state, "retained");
    // The retained commit is usable centrally with the worker gone.
    let candidate = real_call(&socket, repo, "candidate.create", json!({"base":oid(&p.base),"sources":[{"oid":oid(&p.work),"retention_handle":receipt["receipt"]["retention_handle"]}],"commit":{"name":"Coordinator","email":"coordinator@example.test","message":"central-only candidate"}})).await;
    assert_eq!(candidate["state"], "ready");

    // aq-git's own refusals still apply to pushed work, and surface as they do
    // for an upload: the import stays pending and the row is not retained.
    let dir = p.seed.path();
    std::fs::write(dir.join(".gitattributes"), "*.bin binary\n").unwrap();
    git_ok(dir, &["add", "."]).await;
    git_ok(dir, &["commit", "-q", "-m", "attributes"]).await;
    let attributed = git_ok(dir, &["rev-parse", "HEAD"]).await;
    let out = p.push(&attributed).await;
    assert!(out.status.success(), "{}", stderr(&out));
    let refused = Uuid::new_v4();
    let (code, body) = p.admit(refused, &attributed).await;
    assert_eq!(code, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    let (state, receipt): (String, Option<Value>) =
        sqlx::query_as("SELECT state,receipt FROM source_transfers WHERE transfer_id=$1")
            .bind(refused)
            .fetch_one(&p.h.pool)
            .await
            .unwrap();
    assert_eq!((state.as_str(), receipt), ("import_pending", None));
    let mut p = p;
    let mut child = p.aq_git.take().unwrap();
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}
