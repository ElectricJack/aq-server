//! Explicit cross-repository qualification. The normal server suite does not
//! require a worker checkout or silently substitute a fake owner client.
use crate::common;
use aq_observation_admission::{Operation, Request};
use common::profiled::{Fixture, fixture};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    collections::BTreeSet,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
use uuid::Uuid;
fn executable() -> PathBuf {
    let path = PathBuf::from(
        std::env::var("AQ_OBSERVATION_OWNER_PEER")
            .expect("set exact extracted owner_peer artifact"),
    );
    assert!(path.is_absolute());
    let expected = std::env::var("AQ_OBSERVATION_OWNER_PEER_SHA256")
        .expect("set independently qualified artifact SHA256");
    assert_eq!(expected.len(), 64);
    assert_eq!(
        format!("{:x}", Sha256::digest(std::fs::read(&path).unwrap())),
        expected
    );
    path
}
fn private_file(path: &Path, bytes: &[u8]) {
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}
fn config(path: &Path, origin: &str, node: Uuid, boot: Uuid, token: &Path) {
    let bytes = serde_json::to_vec(&json!({
        "version": 1,
        "origin": origin,
        "node_id": node,
        "boot_id": boot,
        "credential_file": token,
        "allow_loopback_http": true,
    }))
    .unwrap();
    private_file(path, &bytes);
}
async fn invoke(
    binary: &Path,
    cfg: &Path,
    root: &Path,
    action: &str,
    ids: &[Uuid],
    credential: &str,
    success: bool,
) -> Option<Value> {
    let mut cmd = tokio::process::Command::new(binary);
    cmd.arg(cfg).arg(root).arg(action);
    for id in ids {
        cmd.arg(id.to_string());
    }
    let out = tokio::time::timeout(Duration::from_secs(20), cmd.output())
        .await
        .expect("bounded peer completion")
        .unwrap();
    let stdout = String::from_utf8(out.stdout).unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        !stdout.contains(credential) && !stderr.contains(credential),
        "credential escaped private transport input"
    );
    assert_eq!(
        out.status.success(),
        success,
        "action {action} stdout={stdout} stderr={stderr}"
    );
    if success {
        Some(serde_json::from_str(&stdout).unwrap())
    } else {
        None
    }
}
async fn start(f: &Fixture) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let app = aq_server::build_worker_router(f.state.clone());
    let h = tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (addr, h)
}
async fn counts(f: &Fixture) -> (i64, i64) {
    let p = f.db.pool().await;
    (
        sqlx::query_scalar("SELECT count(*) FROM observation_transport_owners")
            .fetch_one(&p)
            .await
            .unwrap(),
        sqlx::query_scalar("SELECT count(*) FROM observation_transport_receipts")
            .fetch_one(&p)
            .await
            .unwrap(),
    )
}
fn header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|b| b == b"\r\n\r\n").map(|n| n + 4)
}
fn read_request(s: &mut TcpStream) -> Vec<u8> {
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut b = vec![];
    let mut chunk = [0; 2048];
    loop {
        let n = s.read(&mut chunk).unwrap();
        assert!(n > 0, "incomplete synthetic proxy request");
        b.extend_from_slice(&chunk[..n]);
        assert!(b.len() <= 24576);
        if let Some(end) = header_end(&b) {
            assert!(end <= 8192);
            let header = std::str::from_utf8(&b[..end]).unwrap();
            assert!(!header.to_ascii_lowercase().contains("transfer-encoding:"));
            let size = header
                .lines()
                .filter_map(|l| l.split_once(':'))
                .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
                .map(|(_, v)| v.trim().parse::<usize>().unwrap())
                .unwrap_or(0);
            assert!(size <= 16384);
            if b.len() >= end + size {
                assert_eq!(b.len(), end + size);
                return b;
            }
        }
    }
}
fn proxy_request(raw: &[u8], upstream: SocketAddr) -> (Vec<u8>, Option<Request>) {
    let end = header_end(raw).unwrap();
    let headers = std::str::from_utf8(&raw[..end]).unwrap();
    let first = headers.lines().next().unwrap();
    assert!(matches!(
        first,
        "POST /v1/worker/observations/transport HTTP/1.1"
            | "GET /v1/worker/observations/transport HTTP/1.1"
    ));
    let mutation = first
        .starts_with("POST ")
        .then(|| serde_json::from_slice(&raw[end..]).unwrap());
    let mut forward = format!("{first}\r\nHost: {upstream}\r\nConnection: close\r\n").into_bytes();
    for line in headers.lines().skip(1).filter(|s| !s.is_empty()) {
        let (key, _) = line.split_once(':').unwrap();
        if !key.eq_ignore_ascii_case("host") && !key.eq_ignore_ascii_case("connection") {
            forward.extend_from_slice(line.as_bytes());
            forward.extend_from_slice(b"\r\n");
        }
    }
    forward.extend_from_slice(b"\r\n");
    forward.extend_from_slice(&raw[end..]);
    (forward, mutation)
}

fn upstream_reply(upstream: SocketAddr, request: &[u8]) -> Vec<u8> {
    let mut stream = TcpStream::connect_timeout(&upstream, Duration::from_secs(3)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(request).unwrap();
    let mut response = vec![];
    stream.take(24577).read_to_end(&mut response).unwrap();
    assert!(response.len() <= 24576);
    assert!(
        response.starts_with(b"HTTP/1.1 200"),
        "unexpected synthetic upstream status"
    );
    response
}

fn proxy_reply(
    downstream: &mut TcpStream,
    upstream: SocketAddr,
    mutations: &Mutex<Vec<Request>>,
    drops: &mut BTreeSet<String>,
) {
    let (forward, mutation) = proxy_request(&read_request(downstream), upstream);
    let kind = mutation.map(|request| {
        let kind = match request.operation {
            Operation::Prepare { .. } => "prepare",
            Operation::Activate { .. } => "activate",
            Operation::AdvanceBoot { .. } => "advance_boot",
        };
        mutations.lock().unwrap().push(request);
        kind
    });
    let response = upstream_reply(upstream, &forward);
    if !kind.is_some_and(|k| drops.remove(k)) {
        downstream.write_all(&response).unwrap();
    }
}

/// This fault proxy can connect only to the synthetic listener supplied by this
/// test. It never takes a caller URL or forwards any other route.
struct LostReplyProxy {
    origin: String,
    mutations: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl LostReplyProxy {
    fn new(upstream: SocketAddr, drop_kinds: &[&str]) -> Self {
        assert!(upstream.ip().is_loopback());
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let origin = format!("http://{}", l.local_addr().unwrap());
        l.set_nonblocking(true).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let mutations = Arc::new(Mutex::new(vec![]));
        let captured = mutations.clone();
        let mut drops: BTreeSet<String> = drop_kinds.iter().map(|s| s.to_string()).collect();
        let thread = std::thread::spawn(move || {
            while !stopped.load(Ordering::SeqCst) {
                let (mut downstream, _) = match l.accept() {
                    Ok(v) => v,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                proxy_reply(&mut downstream, upstream, &captured, &mut drops);
            }
        });
        Self {
            origin,
            mutations,
            stop,
            thread: Some(thread),
        }
    }
}
impl Drop for LostReplyProxy {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn proxy_preserves_get_headers_without_capturing_a_mutation() {
    let upstream = "127.0.0.1:12345".parse().unwrap();
    let credential = Uuid::new_v4().to_string();
    let raw = format!(
        "GET /v1/worker/observations/transport HTTP/1.1\r\nhOsT: old.invalid\r\ncOnNeCtIoN: keep-alive\r\nAuthorization: Bearer {credential}\r\n\r\n"
    );
    let (forward, mutation) = proxy_request(raw.as_bytes(), upstream);
    assert!(mutation.is_none());
    assert_eq!(
        String::from_utf8(forward).unwrap(),
        format!(
            "GET /v1/worker/observations/transport HTTP/1.1\r\nHost: {upstream}\r\nConnection: close\r\nAuthorization: Bearer {credential}\r\n\r\n"
        )
    );
}

#[test]
fn proxy_preserves_post_body_and_captures_exact_mutation() {
    let upstream = "127.0.0.1:12345".parse().unwrap();
    let request = Request {
        version: aq_observation_admission::VERSION,
        operation_id: Uuid::new_v4(),
        node_id: Uuid::new_v4(),
        operation: Operation::Prepare {
            boot_id: Uuid::new_v4(),
            store_id: Uuid::new_v4(),
        },
    };
    let body = serde_json::to_vec_pretty(&request).unwrap();
    let headers = format!(
        "POST /v1/worker/observations/transport HTTP/1.1\r\nHost: old.invalid\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n",
        body.len()
    );
    let mut raw = headers.into_bytes();
    raw.extend_from_slice(&body);
    let (forward, mutation) = proxy_request(&raw, upstream);
    assert_eq!(mutation, Some(request));
    let end = header_end(&forward).unwrap();
    assert_eq!(&forward[end..], body);
    assert_eq!(
        std::str::from_utf8(&forward[..end]).unwrap(),
        format!(
            "POST /v1/worker/observations/transport HTTP/1.1\r\nHost: {upstream}\r\nConnection: close\r\nContent-Length: {}\r\nContent-Type: application/json\r\n\r\n",
            body.len()
        )
    );
}

#[test]
#[should_panic]
fn proxy_rejects_other_routes() {
    proxy_request(
        b"GET /v2/worker/register HTTP/1.1\r\nHost: old.invalid\r\n\r\n",
        "127.0.0.1:12345".parse().unwrap(),
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires separately qualified exact owner_peer artifact and scripts/test-db disposable Postgres"]
async fn actual_http_lost_prepare_activate_reply_restart_and_missing_active_store_never_reseed() {
    let binary = executable();
    let f = fixture().await;
    let (addr, server) = start(&f).await;
    let proxy = LostReplyProxy::new(addr, &["prepare", "activate"]);
    let t = tempfile::tempdir().unwrap();
    std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let cfg = t.path().join("peer.json");
    let token = t.path().join("node.token");
    private_file(&token, f.credential.as_bytes());
    config(&cfg, &proxy.origin, f.node.node_id, f.node.boot_id, &token);
    let root = t.path().join("owner");
    let initial = Uuid::new_v4();
    let store = Uuid::new_v4();
    let activation = Uuid::new_v4();
    invoke(
        &binary,
        &cfg,
        &root,
        "initialize",
        &[initial, store],
        &f.credential,
        false,
    )
    .await;
    assert_eq!(counts(&f).await, (1, 1));
    assert_eq!(
        proxy.mutations.lock().unwrap().len(),
        1,
        "transport silently retried dropped reply"
    );
    assert!(!root.join("store").exists());
    invoke(&binary, &cfg, &root, "prepare", &[], &f.credential, true).await;
    invoke(&binary, &cfg, &root, "create", &[], &f.credential, true).await;
    assert_eq!(counts(&f).await, (1, 1));
    invoke(
        &binary,
        &cfg,
        &root,
        "activate",
        &[activation],
        &f.credential,
        false,
    )
    .await;
    assert_eq!(counts(&f).await, (1, 2));
    let active = invoke(&binary, &cfg, &root, "resume", &[], &f.credential, true)
        .await
        .unwrap();
    assert_eq!(active["stage"], "active");
    assert_eq!(active["next_sequence"], 1);
    assert_eq!(active["accepted_through"], 0);
    let requests = proxy.mutations.lock().unwrap().clone();
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.operation_id == initial)
            .count(),
        3
    );
    let original = requests.iter().find(|r| r.operation_id == initial).unwrap();
    assert!(
        requests
            .iter()
            .filter(|r| r.operation_id == initial)
            .all(|r| r == original)
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r.operation_id == activation)
            .count(),
        1
    );
    let local = std::fs::read_to_string(root.join("intent.json")).unwrap();
    assert!(!local.contains(&f.credential));
    std::fs::remove_file(root.join("store/store.redb")).unwrap();
    invoke(&binary, &cfg, &root, "resume", &[], &f.credential, false).await;
    assert!(!root.join("store/store.redb").exists());
    // A completely different empty local root cannot turn retained central Active
    // state into a fresh store, including when accepted-through remains zero.
    let other = t.path().join("lost-all-local-state");
    let before = proxy.mutations.lock().unwrap().len();
    // Use the direct listener for the expected409: the loss proxy intentionally
    // accepts successful synthetic replies only, so it cannot mask this refusal.
    config(
        &cfg,
        &format!("http://{addr}"),
        f.node.node_id,
        f.node.boot_id,
        &token,
    );
    invoke(
        &binary,
        &cfg,
        &other,
        "initialize",
        &[Uuid::new_v4(), Uuid::new_v4()],
        &f.credential,
        false,
    )
    .await;
    assert!(!other.join("store").exists());
    assert_eq!(counts(&f).await, (1, 2));
    assert_eq!(proxy.mutations.lock().unwrap().len(), before);
    drop(proxy);
    server.abort();
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires separately qualified exact owner_peer artifact and scripts/test-db disposable Postgres"]
async fn actual_http_boot_advance_lost_reply_exact_recovery_and_revocation() {
    let binary = executable();
    let mut f = fixture().await;
    // The shared profiled fixture deliberately retains its old assignment. Add
    // a distinct enrolled/registered node with no execution writers instead of
    // deleting that assignment or inventing stop/retention evidence.
    let now = (f.state.clock)();
    let credential = aq_server::tokens::new_node_credential();
    f.state
        .store
        .create_enrollment(
            "owner-peer-node",
            f.actor,
            now,
            now + chrono::Duration::minutes(5),
        )
        .await
        .unwrap();
    let fresh = f
        .state
        .store
        .redeem_enrollment(
            "owner-peer-node",
            aq_server::store::NewNode {
                name: "owner-peer-node".into(),
                credential_hash: aq_server::tokens::verifier(&credential),
                worker_version: "synthetic-owner-peer-qualification".into(),
                protocol_version: 1,
            },
            now,
        )
        .await
        .unwrap()
        .unwrap();
    f.node = aq_worker_protocol::NodeIdentity {
        node_id: fresh.id,
        boot_id: Uuid::new_v4(),
    };
    f.credential = credential;
    f.state
        .store
        .execution_register(&aq_worker_protocol::RegisterRequest {
            identity: f.node.clone(),
            operation_id: Uuid::new_v4(),
            protocol_version: 2,
            worker_version: "synthetic-owner-peer-qualification".into(),
            capabilities: vec![aq_worker_protocol::FAKE_HARNESS_CAPABILITY.into()],
        })
        .await
        .unwrap();
    let held: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM execution_attempts WHERE node_id=$1 AND holds_task",
    )
    .bind(f.node.node_id)
    .fetch_one(&f.db.pool().await)
    .await
    .unwrap();
    assert_eq!(held, 0);
    let (addr, server) = start(&f).await;
    let proxy = LostReplyProxy::new(addr, &["advance_boot"]);
    let t = tempfile::tempdir().unwrap();
    std::fs::set_permissions(t.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let cfg = t.path().join("peer.json");
    let token = t.path().join("node.token");
    private_file(&token, f.credential.as_bytes());
    config(&cfg, &proxy.origin, f.node.node_id, f.node.boot_id, &token);
    let root = t.path().join("owner");
    invoke(
        &binary,
        &cfg,
        &root,
        "initialize",
        &[Uuid::new_v4(), Uuid::new_v4()],
        &f.credential,
        true,
    )
    .await;
    invoke(&binary, &cfg, &root, "create", &[], &f.credential, true).await;
    invoke(
        &binary,
        &cfg,
        &root,
        "activate",
        &[Uuid::new_v4()],
        &f.credential,
        true,
    )
    .await;
    let boot = Uuid::new_v4();
    let register = aq_worker_protocol::RegisterRequest {
        identity: aq_worker_protocol::NodeIdentity {
            node_id: f.node.node_id,
            boot_id: boot,
        },
        operation_id: Uuid::new_v4(),
        protocol_version: 2,
        worker_version: "synthetic-owner-peer-qualification".into(),
        capabilities: vec![aq_worker_protocol::FAKE_HARNESS_CAPABILITY.into()],
    };
    // Incarnation replacement is an explicit operator action even with no
    // writers. Transport ownership never grants this permission. Use the real
    // guarded operator storage API as a fixture precondition, then qualify the
    // current worker registration over the actual HTTP listener.
    let mut operator = register.clone();
    operator.operation_id = Uuid::new_v4();
    f.state
        .store
        .execution_replace_incarnation(&operator)
        .await
        .unwrap();
    let response = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .unwrap()
        .post(format!("http://{addr}/v2/worker/register"))
        .bearer_auth(&f.credential)
        .json(&register)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    config(&cfg, &proxy.origin, f.node.node_id, boot, &token);
    invoke(&binary, &cfg, &root, "resume", &[], &f.credential, false).await;
    let op = Uuid::new_v4();
    invoke(&binary, &cfg, &root, "advance", &[op], &f.credential, false).await;
    assert_eq!(counts(&f).await, (1, 3));
    assert_eq!(
        proxy
            .mutations
            .lock()
            .unwrap()
            .iter()
            .filter(|r| r.operation_id == op)
            .count(),
        1
    );
    let result = invoke(
        &binary,
        &cfg,
        &root,
        "recover-boot",
        &[],
        &f.credential,
        true,
    )
    .await
    .unwrap();
    assert_eq!(result["active_boot"], boot.to_string());
    assert_eq!(result["next_sequence"], 1);
    {
        let requests = proxy.mutations.lock().unwrap();
        let saved: Vec<_> = requests.iter().filter(|r| r.operation_id == op).collect();
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[0], saved[1]);
    }

    drop(proxy);
    config(
        &cfg,
        &format!("http://{addr}"),
        f.node.node_id,
        boot,
        &token,
    );
    sqlx::query("UPDATE nodes SET revoked_at=now() WHERE id=$1")
        .bind(f.node.node_id)
        .execute(&f.db.pool().await)
        .await
        .unwrap();
    invoke(&binary, &cfg, &root, "resume", &[], &f.credential, false).await;
    assert_eq!(counts(&f).await, (1, 3));
    server.abort();
}
