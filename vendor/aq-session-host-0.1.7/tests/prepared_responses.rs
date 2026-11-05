#![cfg(feature = "test-support")]
use anyhow::{Context, Result, ensure};
use aq_responses_transport::{
    ResponsesPolicy, RetainedUpstreamCredential, ScopedResponsesTransport, UpstreamEndpoint,
};
use aq_session_host::{capsule::*, ownership};
use aq_session_protocol::{ArtifactRef, Digest, ModelSettings, Text};
use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Notify,
};
use zeroize::Zeroizing;

const _: () = assert!(!PRODUCTION_ELIGIBLE);
const SYNTHETIC_UPSTREAM: &[u8] = b"parent-only-synthetic-upstream-key";
const FINITE_SSE: &[u8] = b"event: response.completed\ndata: {\"type\":\"response.completed\",\"marker\":\"finite-capsule-sse\"}\n\n";

struct Credential {
    reads: AtomicUsize,
    active: AtomicUsize,
    entered: Notify,
    block_second: bool,
    released: Mutex<bool>,
    release: Condvar,
}
impl Credential {
    fn new(block_second: bool) -> Arc<Self> {
        Arc::new(Self {
            reads: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            entered: Notify::new(),
            block_second,
            released: Mutex::new(false),
            release: Condvar::new(),
        })
    }
    async fn second_read(&self) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while self.reads.load(Ordering::SeqCst) < 2 {
                self.entered.notified().await;
            }
        })
        .await
        .expect("complete guest request reached retained callback");
    }
    fn release(&self) {
        *self.released.lock().unwrap() = true;
        self.release.notify_all();
    }
    async fn drain(&self) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while self.active.load(Ordering::SeqCst) > 0 {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .expect("cooperative synthetic callback drained");
    }
}
impl RetainedUpstreamCredential for Credential {
    fn read_current(&self) -> Result<Zeroizing<Vec<u8>>> {
        self.active.fetch_add(1, Ordering::SeqCst);
        let read = self.reads.fetch_add(1, Ordering::SeqCst) + 1;
        if read == 2 {
            self.entered.notify_one();
            if self.block_second {
                let mut released = self.released.lock().unwrap();
                while !*released {
                    released = self.release.wait(released).unwrap();
                }
            }
        }
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(Zeroizing::new(SYNTHETIC_UPSTREAM.to_vec()))
    }
}
// Panic/cancellation never leaves the deliberately paused blocking read hung.
struct CallbackRelease(Arc<Credential>);
impl Drop for CallbackRelease {
    fn drop(&mut self) {
        self.0.release();
    }
}

struct Peer {
    origin: String,
    requests: Arc<AtomicUsize>,
    task: Option<tokio::task::JoinHandle<Result<()>>>,
}
impl Peer {
    async fn new() -> Self {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let count = requests.clone();
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await?;
            count.fetch_add(1, Ordering::SeqCst);
            let mut head = Zeroizing::new(Vec::new());
            while !head.ends_with(b"\r\n\r\n") {
                head.push(socket.read_u8().await?);
                ensure!(head.len() <= 8192, "finite upstream header bound");
            }
            let text = std::str::from_utf8(&head)?;
            ensure!(
                text.starts_with("POST /v1/responses HTTP/1.1\r\n"),
                "finite upstream route"
            );
            let mut length = None;
            let mut authorized = false;
            for line in text.split("\r\n").skip(1).filter(|line| !line.is_empty()) {
                let (name, value) = line.split_once(':').context("finite upstream header")?;
                match name.to_ascii_lowercase().as_str() {
                    "content-length" => length = Some(value.trim().parse::<usize>()?),
                    "authorization" => {
                        authorized = value
                            .trim()
                            .strip_prefix("Bearer ")
                            .is_some_and(|key| key.as_bytes() == SYNTHETIC_UPSTREAM)
                    }
                    _ => {}
                }
            }
            ensure!(authorized, "upstream receives only parent synthetic key");
            let length = length.context("finite upstream content length")?;
            ensure!(length <= 4096, "finite upstream body bound");
            let mut bytes = Zeroizing::new(vec![0; length]);
            socket.read_exact(&mut bytes).await?;
            let body: serde_json::Value = serde_json::from_slice(&bytes)?;
            ensure!(
                body["model"] == "offline-model"
                    && body["reasoning"]["effort"] == "medium"
                    && body["max_output_tokens"] == 100,
                "exact selected model/reasoning/injected cap"
            );
            ensure!(
                body["input"][0]["content"] == "finite capsule input"
                    && !bytes.windows(13).any(|window| window == b"aq-responses-")
                    && !bytes
                        .windows(SYNTHETIC_UPSTREAM.len())
                        .any(|window| window == SYNTHETIC_UPSTREAM),
                "body excludes both credentials"
            );
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",FINITE_SSE.len()).as_bytes()).await?;
            socket.write_all(FINITE_SSE).await?;
            socket.shutdown().await?;
            Ok(())
        });
        Self {
            origin,
            requests,
            task: Some(task),
        }
    }
    async fn finish(&mut self, expected: usize) {
        assert_eq!(
            self.requests.load(Ordering::SeqCst),
            expected,
            "actual upstream connection count"
        );
        if expected == 1 {
            tokio::time::timeout(Duration::from_secs(2), self.task.take().unwrap())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
        } else {
            let task = self.task.take().unwrap();
            task.abort();
            let _ = task.await;
        }
    }
}
impl Drop for Peer {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

struct Fixture {
    root: Option<tempfile::TempDir>,
    workspace: PathBuf,
    auth: PathBuf,
}
fn reference(id: &str, path: &Path) -> ArtifactRef {
    ArtifactRef {
        id: Text::new(id).unwrap(),
        version: Text::new("1").unwrap(),
        digest: Digest::of(&std::fs::read(path).unwrap()),
    }
}
impl Fixture {
    fn prepare(mode: &str, lifetime_ms: u64) -> (Self, CandidatePreparedHandle) {
        let root = tempfile::tempdir().unwrap();
        for name in ["root", "workspace", "auth"] {
            let path = root.path().join(name);
            std::fs::create_dir(&path).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let program = root.path().join("peer");
        assert!(
            std::process::Command::new("/usr/bin/cc")
                .args(["-static", "-O2", "-Wall", "-Wextra", "-Werror"])
                .arg(
                    Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("tests/fixtures/capsule-responses-peer.c")
                )
                .arg("-o")
                .arg(&program)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .status()
                .unwrap()
                .success()
        );
        let helper =
            std::fs::canonicalize(env!("CARGO_BIN_EXE_aq-session-capsule-candidate")).unwrap();
        let helper = PinnedHelper::open(&helper, &reference("synthetic-helper", &helper)).unwrap();
        let workspace = root.path().join("workspace");
        let auth = root.path().join("auth");
        let grants = vec![
            MountGrant::open(&workspace, MountTarget::Workspace, true).unwrap(),
            MountGrant::open(&auth, MountTarget::DerivedAuth, false).unwrap(),
        ];
        let plan = StaticCapsulePlan::new(
            &root.path().join("root"),
            &program,
            &reference("finite-responses-peer", &program),
            b"finite scoped SSE stdin\n",
            grants,
            ownership::raw_boot_ms() + lifetime_ms,
        )
        .unwrap()
        .arguments(vec![mode.into()])
        .unwrap();
        let owner = ArtifactRef {
            id: Text::new("synthetic-composition-owner").unwrap(),
            version: Text::new("1").unwrap(),
            digest: Digest::of(b"no real authority"),
        };
        let prepared = CandidatePreparedHandle::prepare(&helper, plan, owner).unwrap();
        assert!(!workspace.join("guest-ready").exists());
        (
            Self {
                root: Some(root),
                workspace,
                auth,
            },
            prepared,
        )
    }
}
enum Physical {
    Prepared(CandidatePreparedHandle),
    Running(CapsuleHandle),
    Failed(CandidateCommitFailure),
}
struct Aggregate {
    physical: Option<Physical>,
    transport: Option<ScopedResponsesTransport>,
    fixture: Fixture,
    transport_closed: bool,
    echild: bool,
}
impl Aggregate {
    async fn start(mode: &str, lifetime_ms: u64, peer: &Peer, credential: Arc<Credential>) -> Self {
        let (fixture, mut prepared) = Fixture::prepare(mode, lifetime_ms);
        let listener = prepared.take_listener().unwrap();
        assert!(
            matches!(listener.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock)
        );
        let deadline = prepared
            .info()
            .deadline_boot_ms()
            .checked_mul(1_000_000)
            .unwrap();
        let model:ModelSettings=serde_json::from_value(serde_json::json!({"provider":"openai","model":"offline-model","reasoning":"medium","max_output_tokens":100,"spending_budget":null})).unwrap();
        let policy = ResponsesPolicy::new(
            prepared.info().preparation_digest().clone(),
            &model,
            deadline,
        )
        .unwrap();
        let mut owner = Self {
            physical: Some(Physical::Prepared(prepared)),
            transport: None,
            fixture,
            transport_closed: false,
            echild: false,
        };
        let transport = ScopedResponsesTransport::start_on_listener(
            listener,
            owner.info().origin(),
            policy,
            UpstreamEndpoint::development_loopback(&peer.origin).unwrap(),
            credential,
        )
        .await
        .unwrap();
        assert_eq!(transport.original_deadline_boot_ns(), deadline);
        assert_eq!(transport.origin(), owner.info().origin());
        assert_eq!(transport.owner_scope(), owner.info().preparation_digest());
        owner.transport = Some(transport);
        owner
    }
    fn info(&self) -> &CandidatePreparedInfo {
        match self.physical.as_ref().unwrap() {
            Physical::Prepared(handle) => handle.info(),
            _ => panic!("not prepared"),
        }
    }
    fn commit(&self) -> CandidateCommit {
        let transport = self.transport.as_ref().unwrap();
        let cap = transport.local_capability();
        assert!(cap.as_slice() != SYNTHETIC_UPSTREAM);
        let published =
            CandidatePublishedInputs::publish_synthetic(self.info(), &self.fixture.auth, &cap)
                .unwrap();
        let auth = Zeroizing::new(std::fs::read(self.fixture.auth.join("auth.json")).unwrap());
        assert!(
            !auth
                .windows(SYNTHETIC_UPSTREAM.len())
                .any(|window| window == SYNTHETIC_UPSTREAM)
        );
        assert_eq!(Digest::of(&cap), transport.local_capability_digest());
        CandidateCommit::new(
            self.info(),
            published,
            CandidateStaticRecipe::new(self.info()),
            transport.scope_digest().clone(),
        )
        .unwrap()
    }
    fn release(&mut self, commit: CandidateCommit) -> Result<()> {
        let Some(Physical::Prepared(handle)) = self.physical.take() else {
            panic!("one commit only")
        };
        match handle.commit_retaining(commit) {
            Ok(handle) => {
                self.physical = Some(Physical::Running(handle));
                Ok(())
            }
            Err(failure) => {
                self.physical = Some(Physical::Failed(failure));
                anyhow::bail!("retained commit failure")
            }
        }
    }
    async fn shutdown(&mut self) -> Result<bool> {
        // Await resolves only once the serving task has actually joined, even
        // when its result records expiry. It does not forcibly stop callbacks.
        let service_ok = self
            .transport
            .take()
            .context("transport already taken")?
            .stop()
            .await
            .is_ok();
        self.transport_closed = true;
        match self
            .physical
            .take()
            .context("physical owner already taken")?
        {
            Physical::Prepared(handle) => self.echild = handle.abort()?.kernel_echild(),
            Physical::Running(handle) => self.echild = handle.stop()?.kernel_echild(),
            Physical::Failed(failure) => {
                assert!(self.transport_closed);
                drop(failure);
            }
        }
        Ok(service_ok)
    }
    fn send(&self) {
        std::fs::write(self.fixture.workspace.join("send"), b"send one request\n").unwrap();
    }
    async fn marker(&self, name: &str) {
        tokio::time::timeout(Duration::from_secs(3), async {
            while !self.fixture.workspace.join(name).exists() {
                if self.fixture.workspace.join("guest-error").exists() {
                    panic!(
                        "static guest error line {}",
                        std::fs::read_to_string(self.fixture.workspace.join("guest-error"))
                            .unwrap()
                    );
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("missing {name}"));
    }
}
impl Drop for Aggregate {
    fn drop(&mut self) {
        if let Some(transport) = self.transport.take() {
            transport.cancel();
            drop(transport);
        }
        self.physical.take(); // Physical Drop remains bounded/uncertain.
        if !(self.transport_closed && self.echild) {
            // No implicit auth/workspace release authority on async Drop or an
            // uncertain commit. Preserve the tiny synthetic root for inspection.
            if let Some(root) = self.fixture.root.take() {
                let _ = root.keep();
            }
        }
    }
}
fn pidfds(owner: &Aggregate) -> Vec<std::os::fd::OwnedFd> {
    [owner.info().namespace_init(), owner.info().exec_stub()]
        .into_iter()
        .map(|identity| ownership::verified_pidfd(identity).unwrap())
        .collect()
}
async fn ended(fds: &[std::os::fd::OwnedFd]) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while fds.iter().any(|fd| ownership::pidfd_is_live(fd).unwrap()) {
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit finite synthetic capsule+Responses composition; no Codex/provider"]
async fn actual_isolated_guest_uses_derived_cap_and_receives_finite_scoped_sse() {
    let mut peer = Peer::new().await;
    let credential = Credential::new(false);
    let mut owner = Aggregate::start("positive", 8000, &peer, credential.clone()).await;
    let fds = pidfds(&owner);
    let commit = owner.commit();
    owner.release(commit).unwrap();
    owner.marker("guest-ready").await;
    assert_eq!(peer.requests.load(Ordering::SeqCst), 0);
    owner.send();
    owner.marker("response-complete").await;
    assert!(!owner.fixture.workspace.join("response-refused").exists());
    assert!(owner.shutdown().await.unwrap());
    assert!(owner.transport_closed && owner.echild);
    ended(&fds).await;
    assert_eq!(credential.reads.load(Ordering::SeqCst), 2);
    credential.drain().await;
    peer.finish(1).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit closed model/cap negative guest requests; no Codex/provider"]
async fn wrong_model_or_local_cap_never_reaches_parent_upstream() {
    for mode in ["wrong-model", "wrong-key"] {
        let mut peer = Peer::new().await;
        let credential = Credential::new(false);
        let mut owner = Aggregate::start(mode, 8000, &peer, credential.clone()).await;
        let fds = pidfds(&owner);
        let commit = owner.commit();
        owner.release(commit).unwrap();
        owner.marker("guest-ready").await;
        owner.send();
        owner.marker("response-refused").await;
        assert!(owner.fixture.workspace.join("request-attempt").exists());
        assert!(!owner.fixture.workspace.join("response-complete").exists());
        assert!(owner.shutdown().await.unwrap());
        assert!(owner.echild);
        ended(&fds).await;
        assert_eq!(credential.reads.load(Ordering::SeqCst), 1);
        peer.finish(0).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit aggregate cancellation/original expiry; no Codex/provider"]
async fn cancellation_and_original_expiry_fence_completed_guest_requests_before_upstream() {
    for mode in ["cancel", "expiry"] {
        let mut peer = Peer::new().await;
        let credential = Credential::new(true);
        let _release = CallbackRelease(credential.clone());
        let mut owner = Aggregate::start(
            mode,
            if mode == "expiry" { 1600 } else { 8000 },
            &peer,
            credential.clone(),
        )
        .await;
        let deadline = owner.info().deadline_boot_ms();
        let fds = pidfds(&owner);
        let commit = owner.commit();
        owner.release(commit).unwrap();
        owner.marker("guest-ready").await;
        owner.send();
        credential.second_read().await;
        assert!(owner.fixture.workspace.join("request-attempt").exists());
        if mode == "expiry" {
            while ownership::raw_boot_ms() <= deadline {
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
        let _service_ok = owner.shutdown().await.unwrap();
        assert!(owner.transport_closed && owner.echild);
        credential.release();
        credential.drain().await;
        ended(&fds).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!owner.fixture.workspace.join("response-complete").exists());
        peer.finish(0).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit retained commit failure cleanup ordering; no Codex/provider"]
async fn failed_commit_retains_physical_owner_until_transport_has_joined() {
    let mut peer = Peer::new().await;
    let credential = Credential::new(false);
    let mut owner = Aggregate::start("positive", 8000, &peer, credential).await;
    let fds = pidfds(&owner);
    let mut commit = serde_json::to_value(owner.commit()).unwrap();
    commit["recipe"]["environment"]["PATH"] = serde_json::json!("/unqualified");
    assert!(
        owner
            .release(serde_json::from_value(commit).unwrap())
            .is_err()
    );
    let Some(Physical::Failed(failure)) = owner.physical.as_ref() else {
        panic!("failure owner missing")
    };
    assert!(!failure.error().to_string().is_empty());
    assert!(fds.iter().all(|fd| ownership::pidfd_is_live(fd).unwrap()));
    assert!(!owner.fixture.workspace.join("guest-ready").exists());
    assert!(!owner.transport_closed);
    assert!(owner.shutdown().await.unwrap());
    assert!(owner.transport_closed && !owner.echild);
    ended(&fds).await;
    peer.finish(0).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "explicit aggregate Drop cancellation/uncertainty; no Codex/provider"]
async fn dropped_aggregate_fences_late_callback_without_claiming_closed_or_stopped() {
    let mut peer = Peer::new().await;
    let credential = Credential::new(true);
    let _release = CallbackRelease(credential.clone());
    let mut owner = Aggregate::start("cancel", 8000, &peer, credential.clone()).await;
    let fds = pidfds(&owner);
    let root = owner.fixture.root.as_ref().unwrap().path().to_path_buf();
    let commit = owner.commit();
    owner.release(commit).unwrap();
    owner.marker("guest-ready").await;
    owner.send();
    credential.second_read().await;
    drop(owner); // No joined transport or ECHILD receipt is returned by Drop.
    assert!(
        root.exists(),
        "uncertain Drop must retain synthetic private roots"
    );
    credential.release();
    credential.drain().await;
    ended(&fds).await;
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(!root.join("workspace/response-complete").exists());
    peer.finish(0).await;
}
