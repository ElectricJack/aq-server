//! Fake public Graph API only; no DB, provider, claim or scheduler is started.
use aq_admission::TaskRevision;
use aq_server::agent_access::{
    AgentAccessConfig, Runtime, scheduling::SchedulingReadError as Error,
};
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::Response,
    routing::get,
};
use serde_json::{Value, json};
use std::{
    os::unix::fs::PermissionsExt,
    sync::{
        Arc,
        atomic::{AtomicU8, AtomicUsize, Ordering},
    },
};
use uuid::Uuid;
fn id(v: u128) -> Uuid {
    Uuid::from_u128(v)
}
fn node(workspace: Uuid) -> Value {
    json!({"id":id(2),"workspace_id":workspace,"kind":"task","version":1,"content_revision":1,"title":"Ready task","body":"Scoped task body","acceptance_criteria":"Verified outcome","labels":[],"metadata":{"example:weight":1.5},"task_state":"active","verification":"unverified","provenance":[],"retired":false,"deleted":false,"created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z","created_by":"fixture","last_verified_by":null,"last_verified_at":null,"retirement_reason":null,"content_type":null,"summary":null,"summary_of_revision":null,"valid_from":null,"valid_until":null,"review_at":null,"successor_id":null})
}
#[derive(Clone)]
struct Peer {
    mode: Arc<AtomicU8>,
    calls: Arc<AtomicUsize>,
}
async fn graph(State(peer): State<Peer>, uri: Uri, headers: HeaderMap) -> Response {
    peer.calls.fetch_add(1, Ordering::SeqCst);
    assert_eq!(
        headers["authorization"],
        "Bearer synthetic-graph-service-key-000000000000"
    );
    assert_eq!(headers["X-AQ-Graph-API-Version"], "1");
    let mode = peer.mode.load(Ordering::SeqCst);
    let mut value = route_document(&uri, mode);
    if mode == 9 {
        value["nodes"] = json!([node(id(99))]);
    }
    if (12..=17).contains(&mode) {
        apply_edge_fixture(&mut value, mode);
    }
    graph_response(value, mode)
}
fn route_document(uri: &Uri, mode: u8) -> Value {
    let mut root = node(if mode == 1 { id(99) } else { id(1) });
    if mode == 2 {
        root["version"] = 2.into();
    }
    if mode == 11 {
        root["version"] = (-1).into();
    }
    let url = reqwest::Url::parse(&format!("http://fixture{uri}")).unwrap();
    let query: std::collections::BTreeMap<_, _> = url.query_pairs().into_owned().collect();
    if uri.path().ends_with("/ready") {
        assert_eq!(uri.path(), format!("/v1/workspaces/{}/ready", id(1)));
        assert_eq!(query["limit"], "1");
        json!({"items":if mode==5{vec![root.clone(),root]}else{vec![root]},"next_cursor":if mode==10{Some("same-page")}else{None}})
    } else {
        assert_eq!(
            uri.path(),
            format!("/v1/workspaces/{}/nodes/{}/context", id(1), id(2))
        );
        assert_eq!(query["depth"], "1");
        assert_eq!(query["direction"], "both");
        assert!(["8", "2"].contains(&query["limit"].as_str()));
        assert!(["4096", "16384"].contains(&query["max_bytes"].as_str()));
        json!({"node":root,"nodes":[],"edges":[],"pinned_revisions":[],"truncated":mode==3})
    }
}
fn apply_edge_fixture(value: &mut Value, mode: u8) {
    let mut neighbor = node(id(1));
    neighbor["id"] = json!(id(3));
    let edge = json!({"id":id(4),"workspace_id":id(1),"source":id(2),"target":id(3),"edge_type":"blocks","source_revision":null,"target_revision":null,"version":1,"deleted":false,"created_at":"2026-01-01T00:00:00Z","description":null});
    value["nodes"] = json!([neighbor.clone()]);
    value["edges"] = json!([edge.clone()]);
    if mode == 12 {
        let mut second = neighbor.clone();
        second["id"] = json!(id(5));
        value["nodes"] = json!([neighbor.clone(), second]);
    }
    if mode == 13 {
        value["edges"] = json!(
            (0..9)
                .map(|n| {
                    let mut e = edge.clone();
                    e["id"] = json!(id(100 + n));
                    e
                })
                .collect::<Vec<_>>()
        );
    }
    if mode == 14 {
        value["edges"] = json!([edge.clone(), edge]);
    }
    if mode == 15 {
        value["nodes"] = json!([]);
    }
    if mode == 16 {
        value["edges"][0]["target_revision"] = 1.into();
    }
    if mode == 17 {
        value["edges"][0]["target_revision"] = 1.into();
        value["pinned_revisions"] = json!([neighbor]);
    }
}
fn graph_response(value: Value, mode: u8) -> Response {
    let raw = if mode == 6 {
        value
            .to_string()
            .replace("\"version\":1", "\"version\":1,\"version\":1")
    } else if mode == 7 {
        " ".repeat(1_048_577)
    } else {
        value.to_string()
    };
    Response::builder()
        .status(if mode == 8 {
            StatusCode::FORBIDDEN
        } else {
            StatusCode::OK
        })
        .header("X-AQ-Graph-API-Version", if mode == 4 { "2" } else { "1" })
        .header("content-type", "application/json")
        .body(Body::from(raw))
        .unwrap()
}
struct Fixture {
    runtime: Runtime,
    peer: Peer,
    server: tokio::task::JoinHandle<()>,
    _dir: tempfile::TempDir,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
async fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let key = dir.path().join("graph-key");
    std::fs::write(&key, b"synthetic-graph-service-key-000000000000").unwrap();
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let peer = Peer {
        mode: Arc::new(AtomicU8::new(0)),
        calls: Arc::new(AtomicUsize::new(0)),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let app = Router::new()
        .route("/{*path}", get(graph))
        .with_state(peer.clone());
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let runtime = Runtime::new(&AgentAccessConfig {
        graph_url: url,
        graph_token_file: key,
        mutation_actor_id: None,
        credential_ttl_ms: 1000,
    })
    .await
    .unwrap();
    Fixture {
        runtime,
        peer,
        server,
        _dir: dir,
    }
}
fn expected() -> TaskRevision {
    TaskRevision {
        workspace_id: id(1),
        task_id: id(2),
        version: 1,
        content_revision: 1,
    }
}
#[tokio::test]
async fn actual_fixed_public_routes_preserve_exact_revision_raw_artifact_and_partial_coverage() {
    let f = fixture().await;
    let page = f.runtime.scheduling_ready(id(1), 1, None).await.unwrap();
    assert_eq!(page.items[0].revision, expected());
    assert!(page.stamp.requested_at_ms <= page.stamp.received_at_ms);
    assert!(page.stamp.expires_at_ms <= page.stamp.requested_at_ms + 30_000);
    assert_eq!(page.items[0].node["metadata"]["example:weight"], 1.5);
    let context = f
        .runtime
        .scheduling_context(&expected(), 1, 8, 4096)
        .await
        .unwrap();
    assert!(!context.truncated);
    assert_eq!(context.context["node"]["version"], 1);
    assert_eq!(
        context.context,
        serde_json::from_str::<Value>(&context.raw_json).unwrap()
    );
    use sha2::Digest;
    assert_eq!(
        context.digest,
        hex::encode(sha2::Sha256::digest(context.raw_json.as_bytes()))
    );
    f.peer.mode.store(3, Ordering::SeqCst);
    assert!(
        f.runtime
            .scheduling_context(&expected(), 1, 8, 4096)
            .await
            .unwrap()
            .truncated
    );
}
#[tokio::test]
async fn foreign_stale_malformed_duplicate_wrong_version_and_oversize_refuse() {
    let f = fixture().await;
    for mode in [1, 2, 4, 6, 7, 9, 11] {
        f.peer.mode.store(mode, Ordering::SeqCst);
        assert!(
            f.runtime
                .scheduling_context(&expected(), 1, 8, 4096)
                .await
                .is_err(),
            "mode {mode}"
        );
    }
    f.peer.mode.store(2, Ordering::SeqCst);
    assert_eq!(
        f.runtime
            .scheduling_context(&expected(), 1, 8, 4096)
            .await
            .unwrap_err(),
        Error::StaleTask
    );
    for mode in [1, 4, 5, 6, 7, 11] {
        f.peer.mode.store(mode, Ordering::SeqCst);
        assert!(
            f.runtime.scheduling_ready(id(1), 1, None).await.is_err(),
            "mode {mode}"
        );
    }
    f.peer.mode.store(10, Ordering::SeqCst);
    assert!(
        f.runtime
            .scheduling_ready(id(1), 1, Some("same-page"))
            .await
            .is_err()
    );
    f.peer.mode.store(8, Ordering::SeqCst);
    assert_eq!(
        f.runtime
            .scheduling_ready(id(1), 1, None)
            .await
            .unwrap_err(),
        Error::Forbidden
    );
}
#[tokio::test]
async fn invalid_inputs_do_not_contact_graph_or_choose_other_routes() {
    let f = fixture().await;
    assert!(f.runtime.scheduling_ready(id(0), 1, None).await.is_err());
    assert!(f.runtime.scheduling_ready(id(1), 65, None).await.is_err());
    assert!(
        f.runtime
            .scheduling_ready(id(1), 1, Some("\n"))
            .await
            .is_err()
    );
    assert!(
        f.runtime
            .scheduling_ready(id(1), 1, Some(&"x".repeat(4097)))
            .await
            .is_err()
    );
    assert!(
        f.runtime
            .scheduling_context(&expected(), 6, 8, 4096)
            .await
            .is_err()
    );
    let mut nil = expected();
    nil.task_id = id(0);
    assert!(
        f.runtime
            .scheduling_context(&nil, 1, 8, 4096)
            .await
            .is_err()
    );
    assert_eq!(f.peer.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn context_bounds_and_edge_pin_association_follow_the_actual_request() {
    let f = fixture().await;
    for mode in 12..=16 {
        f.peer.mode.store(mode, Ordering::SeqCst);
        assert_eq!(
            f.runtime
                .scheduling_context(&expected(), 1, 2, 16384)
                .await
                .unwrap_err(),
            Error::InvalidResponse,
            "mode {mode}"
        );
    }
    f.peer.mode.store(17, Ordering::SeqCst);
    let context = f
        .runtime
        .scheduling_context(&expected(), 1, 2, 16384)
        .await
        .unwrap();
    assert_eq!(
        context.context["pinned_revisions"][0]["id"],
        id(3).to_string()
    );
}
