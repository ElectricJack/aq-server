//! Actual ordered PostgreSQL + daemon HTTP + protected engine. The Unix peer is
//! a synthetic local forwarding transport, not a new service/provider or native CLI.
mod common;
use aq_playbook_host::model::*;
use aq_server::{
    AppState,
    config::Config,
    playbooks::{domain::*, service::PrepareRequest},
};
use common::profiled::{Fixture, NOW, fixture, insert};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    net::{TcpListener, TcpStream, UnixListener},
    sync::oneshot,
    task::JoinHandle,
};
use uuid::Uuid;
struct Peer {
    task: JoinHandle<()>,
    connections: Arc<Mutex<Vec<JoinHandle<()>>>>,
}
impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
        for task in self.connections.lock().unwrap().drain(..) {
            task.abort();
        }
    }
}
async fn peer(path: &Path) -> Peer {
    // Forward to whichever disposable loopback PostgreSQL DATABASE_URL names:
    // scripts/test-db (127.0.0.1:55432) locally, localhost:5432 in CI.
    let env = reqwest::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
    let host = env.host_str().unwrap().to_owned();
    assert!(
        matches!(host.as_str(), "127.0.0.1" | "localhost"),
        "the Unix peer only forwards to a loopback test database, got {host}"
    );
    let port = env.port().unwrap_or(5432);
    let listener = UnixListener::bind(path.join(".s.PGSQL.5432")).unwrap();
    let connections = Arc::new(Mutex::new(vec![]));
    let saved = connections.clone();
    let task = tokio::spawn(async move {
        loop {
            let (mut unix, _) = listener.accept().await.unwrap();
            let host = host.clone();
            let task = tokio::spawn(async move {
                let mut tcp = TcpStream::connect((host.as_str(), port)).await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut unix, &mut tcp).await;
            });
            saved.lock().unwrap().push(task);
        }
    });
    Peer { task, connections }
}
fn private(path: &Path, bytes: impl AsRef<[u8]>) {
    std::fs::write(path, bytes).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}
fn artifact(recipient: Recipient) -> Artifact {
    let command = message_contract();
    let mut content = ArtifactContent {
        format_version: 1,
        compiler: "synthetic sealed AQ activation fixture".into(),
        source: SourceBinding {
            digest: hash_bytes(b"synthetic source"),
            header: SourceHeader {
                format_version: 1,
                id: "local-mail".into(),
                scope: "project".into(),
                interfaces: BTreeSet::from(["main".into()]),
                definitions: BTreeSet::new(),
            },
        },
        playbook: Playbook {
            id: "local-mail".into(),
            purpose: "actual owned mailbox via bounded optional runtime; no harness delivery"
                .into(),
            scope: "project".into(),
            entries: BTreeMap::from([(
                "main".into(),
                EntryPoint {
                    input: Schema::Null,
                    outcomes: BTreeMap::from([("ok".into(), Schema::Null)]),
                    start: "send".into(),
                },
            )]),
            steps: BTreeMap::new(),
            triggers: vec![],
            limits: Limits::default(),
            required_capabilities: BTreeSet::from(["message.send".into()]),
        },
        registry: registry(),
        dependencies: BTreeMap::new(),
        definitions: BTreeMap::new(),
        source_map: BTreeMap::new(),
    };
    content.playbook.steps.insert(
        "send".into(),
        Step {
            op: Operation::Invoke {
                command: ContractRef {
                    name: command.name.clone(),
                    fingerprint: command.fingerprint().unwrap(),
                },
                input: Expr::literal(json!(MessageInput {
                    recipient,
                    thread_id: Uuid::from_u128(42),
                    body: "synthetic optional-owner mailbox delivery".into()
                })),
                bind: "sent".into(),
                retry: None,
            },
            edges: BTreeMap::from([("queued".into(), "done".into())]),
            on_error: Some("done".into()),
        },
    );
    content.playbook.steps.insert(
        "done".into(),
        Step {
            op: Operation::Return {
                outcome: "ok".into(),
                output: Expr::literal(Value::Null),
            },
            edges: BTreeMap::new(),
            on_error: None,
        },
    );
    Artifact::seal(content).unwrap()
}
struct Setup {
    f: Fixture,
    _dir: tempfile::TempDir,
    _peer: Peer,
    text: String,
    grant: Uuid,
    namespace: Uuid,
    artifact_path: PathBuf,
}
async fn setup() -> Setup {
    let f = fixture().await;
    insert(&f).await;
    // Explicit synthetic known-task access binding, as required by the actual
    // AQ11 owner service. This does not issue agent credentials or launch.
    let binding = &f.profiled.offer.binding;
    let aq_session_protocol::SessionIdentity::Remote {
        session_id,
        assignment_generation,
        ..
    } = binding.session
    else {
        unreachable!()
    };
    let access = json!({"workspace_id":f.selection.lineage.workspace_id,"project_id":f.selection.lineage.project_id,"task_id":f.selection.lineage.task_id,"session_id":session_id,"attempt_id":binding.attempt_id,"generation":assignment_generation});
    sqlx::query("INSERT INTO agent_launch_bindings(attempt_id,node_id,binding,profile,context) VALUES($1,$2,$3,'{}','{}')")
        .bind(binding.attempt_id).bind(f.node.node_id).bind(access).execute(&f.db.pool().await).await.unwrap();
    let dir = tempfile::tempdir().unwrap();
    let peer = peer(dir.path()).await;
    private(
        &dir.path().join("dsn"),
        format!(
            "host='{}' port=5432 user=postgres password=test dbname={} sslmode=disable",
            dir.path().display(),
            f.db.options().get_database().unwrap()
        ),
    );
    private(&dir.path().join("bridge-key"), [9; 32]);
    private(
        &dir.path().join("engine-keys"),
        serde_json::to_vec(&json!({"active_key_id":"test","keys":{"test":hex::encode([11;32])}}))
            .unwrap(),
    );
    let recipient = Recipient::Task {
        workspace_id: f.selection.lineage.workspace_id,
        project_id: f.selection.lineage.project_id,
        task_id: f.selection.lineage.task_id,
    };
    let artifact_path = dir.path().join("artifact");
    private(
        &artifact_path,
        serde_json::to_vec(&artifact(recipient.clone())).unwrap(),
    );
    let namespace = Uuid::new_v4();
    let grant = Uuid::new_v4();
    let text = format!(
        r#"format_version=1
environment="development"
[server]
admin_bind="127.0.0.1:0"
worker_bind="127.0.0.1:0"
public_origin="http://127.0.0.1:8080"
[auth]
mode="dev"
dev_email="owner@localhost"
[bootstrap]
owner_email="owner@localhost"
enabled=true
[playbooks]
enabled=true
profile="human_review_v2"
engine_connection_file="{root}/dsn"
socket_owner_uid={uid}
engine_keyring_file="{root}/engine-keys"
drive_interval_ms=250
[playbooks.namespace]
namespace_id="{namespace}"
generation=1
name="activation-{namespace}"
workspace_id="{workspace}"
project_id="{project}"
owner_user_id="{actor}"
[playbooks.protection]
active_key_id="test"
[[playbooks.protection.keys]]
id="test"
file="{root}/bridge-key"
[[playbooks.deployments]]
artifact_file="{root}/artifact"
binding="mail"
grant_id="{grant}"
expires_at_ms={expires}
[playbooks.deployments.scope]
workspace_id="{workspace}"
project_id="{project}"
capabilities=["message.send"]
[[playbooks.deployments.scope.recipients]]
kind="task"
workspace_id="{workspace}"
project_id="{project}"
task_id="{task}"
"#,
        root = dir.path().display(),
        uid = nix::unistd::geteuid().as_raw(),
        workspace = f.selection.lineage.workspace_id,
        project = f.selection.lineage.project_id,
        actor = f.actor,
        task = f.selection.lineage.task_id,
        expires = NOW + 10000
    );
    Setup {
        f,
        _dir: dir,
        _peer: peer,
        text,
        grant,
        namespace,
        artifact_path,
    }
}
async fn launch(state: AppState) -> (String, oneshot::Sender<()>, JoinHandle<anyhow::Result<()>>) {
    let admin = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let worker = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", admin.local_addr().unwrap());
    let (tx, rx) = oneshot::channel();
    let task = tokio::spawn(aq_server::run_with_listeners(
        state,
        admin,
        worker,
        async move {
            let _ = rx.await;
        },
    ));
    (base, tx, task)
}
fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap()
}
async fn ready(base: &str, task: &JoinHandle<anyhow::Result<()>>) {
    let c = client();
    for _ in 0..100 {
        if let Ok(r) = c
            .get(format!("{base}/api/playbooks/status"))
            .header("x-aq-dev-email", "owner@localhost")
            .send()
            .await
            && r.status().is_success()
        {
            return;
        }
        assert!(!task.is_finished(), "server exited before owner readiness");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    panic!("owner startup timeout");
}
async fn get(base: &str, path: &str) -> Value {
    // Inspection reads queue on the single owner thread behind a drive quantum
    // (up to 8 transitions, ~1-2s in unoptimized test builds) and report 503
    // playbook_storage_unknown when that outlasts the 2s inspect deadline.
    // Re-reading is idempotent; every other status still fails immediately.
    for attempt in 1.. {
        let r = client()
            .get(format!("{base}{path}"))
            .header("x-aq-dev-email", "owner@localhost")
            .send()
            .await
            .unwrap();
        let status = r.status();
        let body: Value = r.json().await.unwrap();
        if status == 503 && body["code"] == "playbook_storage_unknown" && attempt < 5 {
            continue;
        }
        assert!(status.is_success(), "{status}: {body}");
        return body;
    }
    unreachable!()
}
#[tokio::test]
async fn explicit_unix_owner_starts_dispatches_restarts_without_regrant_and_drains() {
    let mut s = setup().await;
    let config: Config = toml::from_str(&s.text).unwrap();
    assert!(config.playbooks.is_some());
    assert!(!format!("{config:?}").contains("password=test"));
    s.f.state.config = Arc::new(config);
    let (base, stop, task) = launch(s.f.state.clone()).await;
    ready(&base, &task).await;
    // A competing startup can neither replace the held protected owner nor
    // consume a new central runtime revision.
    let pool = s.f.db.pool().await;
    let before: i64 = sqlx::query_scalar(
        "SELECT runtime_revision FROM playbook_namespaces WHERE namespace_id=$1",
    )
    .bind(s.namespace)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        aq_server::playbooks::runtime::Runtime::start(
            &s.f.state,
            s.f.state.config.playbooks.clone().unwrap()
        )
        .await
        .is_err()
    );
    let after: i64 = sqlx::query_scalar(
        "SELECT runtime_revision FROM playbook_namespaces WHERE namespace_id=$1",
    )
    .bind(s.namespace)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(before, after);
    let status = get(&base, "/api/playbooks/status").await;
    assert_eq!(status["automatic_driver"], true);
    assert_eq!(status["automatic_driver_state"], "active");
    assert!(
        status["registered_commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "decision.request")
    );
    let binding = get(&base, "/api/playbooks/bindings/mail").await;
    let revision = binding["revision"].as_u64().unwrap();
    let request = PrepareRequest {
        request_id: Uuid::new_v4(),
        grant_id: s.grant,
        grant_revision: 1,
        binding: "mail".into(),
        expected_activation_revision: revision,
        entry: "main".into(),
        input: Value::Null,
    };
    let r = client()
        .post(format!("{base}/api/playbooks/admissions"))
        .header("x-aq-dev-email", "owner@localhost")
        .header("x-aq-request", "1")
        .json(&request)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), reqwest::StatusCode::ACCEPTED);
    let pool = s.f.db.pool().await;
    let mut delivered = false;
    for _ in 0..100 {
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mailbox_messages")
            .fetch_one(&pool)
            .await
            .unwrap();
        if count == 1 {
            delivered = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    assert!(
        delivered,
        "status: {:?}; admission: {:?}",
        get(&base, "/api/playbooks/status").await,
        get(
            &base,
            &format!("/api/playbooks/admissions/{}", request.request_id)
        )
        .await
    );
    let no_csrf = client()
        .post(format!("{base}/api/playbooks/drive"))
        .header("x-aq-dev-email", "owner@localhost")
        .json(&json!({}))
        .send()
        .await
        .unwrap();
    assert_eq!(no_csrf.status(), reqwest::StatusCode::FORBIDDEN);
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let current: Option<Uuid> = sqlx::query_scalar(
        "SELECT runtime_incarnation FROM playbook_namespaces WHERE namespace_id=$1",
    )
    .bind(s.namespace)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(current.is_none());
    sqlx::query("UPDATE playbook_grants SET revoked_at_ms=$2 WHERE grant_id=$1")
        .bind(s.grant)
        .bind(NOW as i64)
        .execute(&pool)
        .await
        .unwrap();
    let (base, stop, task) = launch(s.f.state.clone()).await;
    ready(&base, &task).await;
    assert_eq!(
        get(&base, "/api/playbooks/bindings/mail").await["revision"],
        revision
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM mailbox_messages")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 1);
    let revoked: Option<i64> =
        sqlx::query_scalar("SELECT revoked_at_ms FROM playbook_grants WHERE grant_id=$1")
            .bind(s.grant)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(revoked.is_some());
    let mut fresh = request.clone();
    fresh.request_id = Uuid::new_v4();
    let refused = client()
        .post(format!("{base}/api/playbooks/admissions"))
        .header("x-aq-dev-email", "owner@localhost")
        .header("x-aq-request", "1")
        .json(&fresh)
        .send()
        .await
        .unwrap();
    assert_eq!(refused.status(), reqwest::StatusCode::NOT_FOUND);
    let old: Value = client()
        .get(format!(
            "{base}/api/playbooks/admissions/{}",
            request.request_id
        ))
        .header("x-aq-dev-email", "owner@localhost")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(old["request_id"], request.request_id.to_string());
    // Even a revoked configured user's fresh business authority is unnecessary
    // to close its already-held runtime at process shutdown.
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(s.f.actor)
        .execute(&pool)
        .await
        .unwrap();
    stop.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(15), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
#[tokio::test]
async fn config_refuses_disabled_tcp_unsafe_files_unknown_fields_and_changed_socket_directory() {
    let s = setup().await;
    for text in [
        s.text
            .replace("enabled=true\nprofile=", "enabled=false\nprofile="),
        s.text
            .replace("drive_interval_ms=250", "drive_interval_ms=0"),
        s.text
            .replace("profile=\"human_review_v2\"", "profile=\"future\""),
        s.text.replace(
            "drive_interval_ms=250",
            "drive_interval_ms=250\nprovider_token=\"not-allowed\"",
        ),
    ] {
        assert!(toml::from_str::<Config>(&text).is_err());
    }
    let dsn = s._dir.path().join("dsn");
    private(
        &dsn,
        format!(
            "host=127.0.0.1 port=55432 user=postgres password=canary-secret dbname={} sslmode=disable",
            s.f.db.options().get_database().unwrap()
        ),
    );
    let error = toml::from_str::<Config>(&s.text).unwrap_err().to_string();
    assert!(!error.contains("canary-secret"));
    private(
        &dsn,
        format!(
            "host='{}' port=5432 user=postgres password=test dbname={} sslmode=disable",
            s._dir.path().display(),
            s.f.db.options().get_database().unwrap()
        ),
    );
    std::fs::set_permissions(&s.artifact_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(toml::from_str::<Config>(&s.text).is_err());
    std::fs::set_permissions(&s.artifact_path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let alias = s._dir.path().join("dsn-alias");
    std::fs::hard_link(&dsn, &alias).unwrap();
    assert!(
        toml::from_str::<Config>(&s.text)
            .unwrap_err()
            .to_string()
            .contains("unsafe playbook private file")
    );
    std::fs::remove_file(alias).unwrap();
    let config: Config = toml::from_str(&s.text).unwrap();
    let moved = s._dir.path().with_extension("moved");
    std::fs::rename(s._dir.path(), &moved).unwrap();
    std::fs::create_dir(s._dir.path()).unwrap();
    std::fs::set_permissions(s._dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut state = s.f.state.clone();
    state.config = Arc::new(config);
    let result = aq_server::playbooks::runtime::Runtime::start(
        &state,
        state.config.playbooks.clone().unwrap(),
    )
    .await;
    assert!(result.is_err());
    std::fs::remove_dir(s._dir.path()).unwrap();
    std::fs::rename(moved, s._dir.path()).unwrap();
}
#[tokio::test]
async fn startup_missing_registered_schema_refuses_before_engine_creation() {
    let s = setup().await;
    let config: Config = toml::from_str(&s.text).unwrap();
    let mut state = s.f.state.clone();
    state.config = Arc::new(config);
    let pool = s.f.db.pool().await;
    sqlx::query("DELETE FROM schema_migrations WHERE version=26")
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        aq_server::playbooks::runtime::Runtime::start(
            &state,
            state.config.playbooks.clone().unwrap()
        )
        .await
        .is_err()
    );
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM playbook_namespaces")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    let schema: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pg_namespace WHERE nspname='aq_playbooks')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!schema);
}
#[tokio::test]
async fn changed_bootstrap_binding_is_refused_and_successful_stop_replay_is_exact() {
    let s = setup().await;
    let config: Config = toml::from_str(&s.text).unwrap();
    let mut state = s.f.state.clone();
    state.config = Arc::new(config);
    let loaded = state.config.playbooks.clone().unwrap();
    let runtime = aq_server::playbooks::runtime::Runtime::start(&state, loaded.clone())
        .await
        .unwrap();
    let binding = runtime.driver.inspect_binding("mail".into()).await.unwrap();
    runtime.shutdown().await.unwrap();
    let mut changed: Artifact =
        serde_json::from_slice(&std::fs::read(&s.artifact_path).unwrap()).unwrap();
    changed.content.playbook.purpose = "changed operator declaration".into();
    let changed = Artifact::seal(changed.content).unwrap();
    private(&s.artifact_path, serde_json::to_vec(&changed).unwrap());
    let new_config: Config = toml::from_str(&s.text).unwrap();
    assert!(
        aq_server::playbooks::runtime::Runtime::start(&state, new_config.playbooks.unwrap())
            .await
            .is_err()
    );
    let runtime = aq_server::playbooks::runtime::Runtime::start(&state, loaded)
        .await
        .unwrap();
    assert_eq!(
        runtime.driver.inspect_binding("mail".into()).await.unwrap(),
        binding
    );
    runtime.driver.stop().await.unwrap();
    runtime.shutdown().await.unwrap();
}
#[tokio::test]
async fn lost_engine_connection_does_not_fabricate_shutdown_release_ack() {
    let s = setup().await;
    let config: Config = toml::from_str(&s.text).unwrap();
    let mut state = s.f.state.clone();
    state.config = Arc::new(config);
    let runtime = aq_server::playbooks::runtime::Runtime::start(
        &state,
        state.config.playbooks.clone().unwrap(),
    )
    .await
    .unwrap();
    for task in s._peer.connections.lock().unwrap().drain(..) {
        task.abort();
    }
    assert!(runtime.shutdown().await.is_err());
    let pool = s.f.db.pool().await;
    let incarnation: Option<Uuid> = sqlx::query_scalar(
        "SELECT runtime_incarnation FROM playbook_namespaces WHERE namespace_id=$1",
    )
    .bind(s.namespace)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(incarnation.is_none());
}

#[tokio::test]
async fn inspection_discovers_actual_admissions_runs_and_public_timeline() {
    let mut s = setup().await;
    s.f.state.config = Arc::new(toml::from_str::<Config>(&s.text).unwrap());
    let (base, stop, task) = launch(s.f.state.clone()).await;
    ready(&base, &task).await;
    let bootstrap = get(&base, "/api/v1/bootstrap").await;
    let enabled = |body: &Value| {
        body["data"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|v| v["name"] == "playbooks.inspect")
            .unwrap()["enabled"]
            == true
    };
    assert!(
        enabled(&bootstrap),
        "actual injected runtime namespace owner sees discovery"
    );
    for (email, role) in [
        ("other-owner@localhost", "owner"),
        ("viewer@localhost", "viewer"),
    ] {
        sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,$2,$3,'dev',$2,now())").bind(Uuid::new_v4()).bind(email).bind(role).execute(&s.f.db.pool().await).await.unwrap();
        let response = client()
            .get(format!("{base}/api/v1/bootstrap"))
            .header("x-aq-dev-email", email)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 200);
        let body: Value = response.json().await.unwrap();
        assert!(
            !enabled(&body),
            "only exact runtime namespace owner may inspect"
        );
    }
    let first = get(&base, "/api/playbooks/inspection/runs?limit=1").await;
    assert_eq!(first["api_version"], "playbooks-inspection-1");
    assert_eq!(first["namespace"]["id"], json!(s.namespace));
    assert_eq!(first["items"], json!([]));
    let binding = get(&base, "/api/playbooks/bindings/mail").await;
    let mut requests = vec![];
    for _ in 0..2 {
        let request = PrepareRequest {
            request_id: Uuid::new_v4(),
            grant_id: s.grant,
            grant_revision: 1,
            binding: "mail".into(),
            expected_activation_revision: binding["revision"].as_u64().unwrap(),
            entry: "main".into(),
            input: Value::Null,
        };
        let response = client()
            .post(format!("{base}/api/playbooks/admissions"))
            .header("x-aq-dev-email", "owner@localhost")
            .header("x-aq-request", "1")
            .json(&request)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 202);
        requests.push(request.request_id);
    }
    let admission = get(&base, "/api/playbooks/inspection/admissions?limit=1").await;
    assert_eq!(admission["items"].as_array().unwrap().len(), 1);
    assert!(
        admission["revision"].is_null(),
        "AQ admissions have no engine snapshot revision"
    );
    let cursor = admission["next_cursor"].as_str().unwrap();
    let next = get(
        &base,
        &format!("/api/playbooks/inspection/admissions?limit=1&after={cursor}"),
    )
    .await;
    assert_eq!(next["items"].as_array().unwrap().len(), 1);
    assert!(next["next_cursor"].is_null());
    assert_ne!(
        admission["items"][0]["request_id"],
        next["items"][0]["request_id"]
    );
    let mut page = Value::Null;
    for _ in 0..100 {
        page = get(&base, "/api/playbooks/inspection/runs?limit=1").await;
        if page["items"].as_array().unwrap().len() == 1 && page["next_cursor"].is_string() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    let run = page["items"][0]["id"]
        .as_str()
        .expect("actual admitted run");
    let detail = get(
        &base,
        &format!("/api/playbooks/inspection/runs/{run}?limit=1"),
    )
    .await;
    assert_eq!(detail["run"]["id"], run);
    assert_eq!(detail["run"]["artifact"], binding["artifact"]);
    // The pin strip reads the binding and activation revision the run was admitted on.
    assert_eq!(detail["run"]["binding"], "mail");
    assert_eq!(
        detail["run"]["activation_revision"],
        binding["revision"].as_u64().unwrap().to_string()
    );
    assert_eq!(page["items"][0]["binding"], "mail");
    assert_eq!(detail["observation_is_atomic"], false);
    // G11 X30/X31: definition name, AQ install ordinal and active comparison.
    let definition = &detail["run"]["definition"];
    assert_eq!(definition["version"], "1", "{definition}");
    assert!(definition["name"].as_str().is_some_and(|n| !n.is_empty()));
    assert_eq!(definition["pinned_is_active"], true);
    assert_eq!(definition["active"]["binding"], "mail");
    assert_eq!(definition["active"]["artifact"], binding["artifact"]);
    assert_eq!(definition["active"]["version"], "1");
    assert_eq!(page["items"][0]["definition"], *definition);
    // X32: ordered step graph from the run's entry.
    assert_eq!(detail["steps"]["entry"], "main");
    let steps = detail["steps"]["steps"].as_array().unwrap();
    assert_eq!(steps[0]["id"], detail["steps"]["start"]);
    assert!(steps.iter().any(|step| step["id"] == "send"), "{steps:?}");
    // X33: no human decision is pending in this fixture.
    assert!(detail["run"]["waiting"].is_null());
    let parallel_detail_path = format!("/api/playbooks/inspection/runs/{run}?limit=1");
    let (parallel_list, parallel_detail) = tokio::join!(
        get(&base, "/api/playbooks/inspection/runs?limit=1"),
        get(&base, &parallel_detail_path)
    );
    assert_eq!(parallel_list["api_version"], "playbooks-inspection-1");
    assert_eq!(parallel_detail["run"]["id"], run);
    assert!(detail["receipts"]["revision"].is_string());
    assert!(detail["receipts"]["items"].as_array().unwrap().len() <= 1);
    let text = serde_json::to_string(&detail).unwrap();
    assert!(!text.contains("synthetic optional-owner mailbox delivery"));
    assert!(!text.contains("password=test"));
    assert!(!text.contains("\"variables\"") && !text.contains("\"input\""));
    let after = page["next_cursor"].as_str().unwrap();
    let second = get(
        &base,
        &format!("/api/playbooks/inspection/runs?limit=1&after={after}"),
    )
    .await;
    assert_eq!(second["items"].as_array().unwrap().len(), 1);
    assert_ne!(second["items"][0]["id"], json!(run));
    // X34: admission time range filter over the same runs.
    let admitted = page["items"][0]["admitted_at_ms"].as_i64().unwrap();
    let within = get(
        &base,
        &format!(
            "/api/playbooks/inspection/runs?since_ms={admitted}&until_ms={}",
            admitted + 1
        ),
    )
    .await;
    assert!(
        within["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["id"] == run && item["admitted_at_ms"] == admitted)
    );
    let future = get(
        &base,
        &format!(
            "/api/playbooks/inspection/runs?since_ms={}",
            admitted + 86_400_000
        ),
    )
    .await;
    assert_eq!(future["items"], json!([]));
    assert!(future["next_cursor"].is_null());
    for path in [
        "/api/playbooks/inspection/runs?limit=26".to_string(),
        "/api/playbooks/inspection/runs?since_ms=10&until_ms=10".to_string(),
        "/api/playbooks/inspection/runs?since_ms=-1".to_string(),
        format!(
            "/api/playbooks/inspection/runs?project_id={}",
            Uuid::new_v4()
        ),
    ] {
        let response = client()
            .get(format!("{base}{path}"))
            .header("x-aq-dev-email", "owner@localhost")
            .send()
            .await
            .unwrap();
        assert!(matches!(response.status().as_u16(), 400 | 404));
    }
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn dashboard_playbook_actions_start_cancel_and_refuse_stale_activation() {
    let mut s = setup().await;
    s.f.state.config = Arc::new(toml::from_str::<Config>(&s.text).unwrap());
    let (base, stop, task) = launch(s.f.state.clone()).await;
    ready(&base, &task).await;
    let project =
        get(&base, "/api/playbooks/inspection/runs?limit=1").await["namespace"]["project_id"]
            .clone();
    let binding = get(&base, "/api/playbooks/bindings/mail").await;
    let revision = binding["revision"].as_u64().unwrap().to_string();
    let post = |body: Value| {
        let base = base.clone();
        async move {
            let response = client()
                .post(format!("{base}/api/v1/actions"))
                .header("x-aq-dev-email", "owner@localhost")
                .header("x-aq-request", "1")
                .json(&body)
                .send()
                .await
                .unwrap();
            let status = response.status().as_u16();
            (status, response.json::<Value>().await.unwrap())
        }
    };
    let versioned = |kind: &str, revision: &str, extra: Value| {
        let id = Uuid::new_v4().to_string();
        let mut payload = json!({"operation_id":id,"project_id":project,"expected_version":revision,
            "binding":"mail","artifact":binding["artifact"]});
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        json!({"operation_id":id,"kind":kind,
            "subject":{"type":"playbook_binding","id":"mail","revision":revision},
            "expected_versions":{"playbook_binding":revision},"payload":payload})
    };
    let start = versioned(
        "playbook.run.start",
        &revision,
        json!({"entry":"main","inputs":{}}),
    );
    let (status, started) = post(start.clone()).await;
    assert_eq!(status, 200, "{started}");
    assert_eq!(started["data"]["disposition"], "confirmed", "{started}");
    let affected = &started["data"]["affected_entities"][0];
    assert_eq!(affected["type"], "playbook_run", "{started}");
    let run = affected["id"].as_str().unwrap().to_string();
    let (status, replayed) = post(start).await;
    assert_eq!(status, 200, "{replayed}");
    assert_eq!(replayed["data"]["disposition"], "confirmed", "{replayed}");
    let id = Uuid::new_v4().to_string();
    let (status, cancelled) = post(json!({"operation_id":id,"kind":"playbook.run.cancel",
        "subject":{"type":"playbook_run","id":run},"expected_versions":{},"reason":"Stop it",
        "payload":{"operation_id":id,"project_id":project,"run_id":run}}))
    .await;
    assert_eq!(status, 200, "{cancelled}");
    assert_eq!(cancelled["data"]["disposition"], "confirmed", "{cancelled}");
    let stale = (binding["revision"].as_u64().unwrap() + 1).to_string();
    let (status, conflict) =
        post(versioned("playbook.activation.rollback", &stale, json!({}))).await;
    assert_eq!(status, 200, "{conflict}");
    assert_eq!(conflict["data"]["disposition"], "conflict", "{conflict}");
    assert_eq!(
        conflict["data"]["current_versions"]["playbook_binding"],
        revision
    );
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn inspection_refuses_other_owner_viewer_and_absent_runtime() {
    let mut s = setup().await;
    s.f.state.config = Arc::new(toml::from_str::<Config>(&s.text).unwrap());
    let (base, stop, task) = launch(s.f.state.clone()).await;
    ready(&base, &task).await;
    let pool = s.f.db.pool().await;
    for (email, role, expected) in [
        ("inspection-viewer@localhost", "viewer", 403),
        ("inspection-other-owner@localhost", "owner", 404),
    ] {
        sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,$2,$3,'dev',$2,now())").bind(Uuid::new_v4()).bind(email).bind(role).execute(&pool).await.unwrap();
        let response = client()
            .get(format!("{base}/api/playbooks/inspection/runs"))
            .header("x-aq-dev-email", email)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), expected);
    }
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(s.f.actor)
        .execute(&pool)
        .await
        .unwrap();
    let response = client()
        .get(format!("{base}/api/playbooks/inspection/admissions"))
        .header("x-aq-dev-email", "owner@localhost")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 403);
    // Restore only the fixture user so the actual configured runtime can drain.
    sqlx::query("UPDATE users SET role='owner' WHERE id=$1")
        .bind(s.f.actor)
        .execute(&pool)
        .await
        .unwrap();
    stop.send(()).unwrap();
    task.await.unwrap().unwrap();
    let app = aq_server::build_admin_router(s.f.state.clone());
    use tower::ServiceExt;
    let response = app
        .oneshot(
            axum::http::Request::builder()
                .uri("/api/playbooks/inspection/runs")
                .header("x-aq-dev-email", "owner@localhost")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        404,
        "absent Driver is not an empty run list"
    );
}

#[path = "playbook_activation/browser.rs"]
mod browser;

#[path = "playbook_activation/definitions.rs"]
mod definitions;

#[path = "playbook_activation/catalog.rs"]
mod catalog;

/// Production Host7 construction: the configured owner binds the gateway
/// work graph and leaves the check executor and judgment provider unbound,
/// so both commands refuse with typed reasons and record no evidence.
#[tokio::test]
async fn host7_profile_starts_on_gateway_graph_with_unbound_check_and_judgment_ports() {
    use aq_playbook_host::runner::{Intent, IntentStatus};
    use aq_server::playbooks::review_domain::{CatalogProfile, HOST7_CAPABILITIES};
    let s = setup().await;
    let (head, _) = s.text.split_once("[[playbooks.deployments]]").unwrap();
    let text = head.replace(
        "profile=\"human_review_v2\"",
        "profile=\"host_v7\"\ndeployments=[]",
    );
    let config: Config = toml::from_str(&text).unwrap();
    let mut state = s.f.state.clone();
    state.config = Arc::new(config);
    let loaded = state.config.playbooks.clone().unwrap();
    // Host7 needs the production work graph; without it startup refuses.
    assert!(state.agent_access.is_none());
    assert!(
        aq_server::playbooks::runtime::Runtime::start(&state, loaded.clone())
            .await
            .is_err()
    );
    let token = s._dir.path().join("graph-token");
    private(&token, "g".repeat(48));
    state.agent_access = Some(Arc::new(
        aq_server::agent_access::Runtime::new(&aq_server::agent_access::AgentAccessConfig {
            graph_url: "http://127.0.0.1:9/".into(),
            graph_token_file: token,
            credential_ttl_ms: 10000,
            mutation_actor_id: None,
        })
        .await
        .unwrap(),
    ));
    let runtime = aq_server::playbooks::runtime::Runtime::start(&state, loaded)
        .await
        .unwrap();
    let service = runtime.driver.service.clone();
    runtime.shutdown().await.unwrap();

    // Drive the production-constructed service directly under a fresh owner.
    let pool = s.f.db.pool().await;
    let revision: i64 = sqlx::query_scalar(
        "SELECT runtime_revision FROM playbook_namespaces WHERE namespace_id=$1",
    )
    .bind(s.namespace)
    .fetch_one(&pool)
    .await
    .unwrap();
    let owner = service
        .adopt_runtime(Uuid::new_v4(), revision)
        .await
        .unwrap();
    let l = &s.f.selection.lineage;
    let scope = GrantScope {
        workspace_id: l.workspace_id,
        project_id: l.project_id,
        recipients: vec![],
        capabilities: BTreeSet::from(HOST7_CAPABILITIES.map(String::from)),
    };
    let now = (state.clock)().timestamp_millis();
    service
        .grant(s.f.actor, s.grant, scope.clone(), now + 10_000)
        .await
        .unwrap();
    let intent = |id: &str, name: &str, input: Value| Intent {
        id: id.into(),
        run: "synthetic-run".into(),
        step: name.replace('.', "-"),
        invocation: 1,
        deadline_at_ms: now + 5000,
        command: CatalogProfile::Host7.registry().commands[name].clone(),
        input_digest: aq_server::playbooks::domain::digest(&input).unwrap(),
        input,
        authority: CatalogProfile::Host7.authority(s.grant, 1, &scope).unwrap(),
        status: IntentStatus::Dispatched,
        cancel_requested: false,
        cancel_dispatched: false,
        handle: None,
        result: None,
        reason: None,
        late_results: vec![],
        completion_event: None,
    };
    // Subjects are not graph node ids, so no graph call is made.
    let subject = json!({"id": "work-1", "revision": 1});
    let check = json!({"check": {"id": "unit", "validator": {"id": "aq.check.cargo-test",
            "version": "1.0.0", "digest": format!("sha256:{}", "1".repeat(64))}},
        "source": {"format": "sha1", "hex": "0123456789abcdef0123456789abcdef01234567"},
        "subject": subject});
    let r = service
        .host_dispatch(
            &owner,
            s.grant,
            1,
            &intent("check-1", "check.request", check),
        )
        .await
        .unwrap();
    assert_eq!(
        (
            r.outcome.as_str(),
            &r.output["reason"],
            &r.output["disposition"]
        ),
        ("refused", &json!("unavailable"), &Value::Null),
        "{r:?}"
    );
    let judgment = json!({"budget": {"deadline_ms": 60000, "max_cost_microunits": 50000,
            "max_input_tokens": 8192, "max_output_tokens": 512},
        "context": {"artifacts": [{"digest": format!("sha256:{}", "a".repeat(64)),
            "id": "snapshot-1", "version": "1"}], "text": "Work item summary."},
        "output": {"choices": ["permitted-default", "isolated"], "rationale_max_bytes": 2048},
        "question": "fits?", "subject": subject});
    let r = service
        .host_dispatch(
            &owner,
            s.grant,
            1,
            &intent("judge-1", "judgment.request", judgment),
        )
        .await
        .unwrap();
    assert_eq!(
        (
            r.outcome.as_str(),
            &r.output["reason"],
            &r.output["provenance"]
        ),
        ("refused", &json!("no_permitted_profile"), &Value::Null),
        "{r:?}"
    );
    // Refusals fabricate no check evidence.
    let evidence: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_check_evidence")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(evidence, 0);
    service.retire_runtime(&owner).await.unwrap();
}

/// One aq-git exchange on `path`: replies `ok` with `result(request)`.
async fn git_exchange(
    path: &Path,
    result: impl FnOnce(&Value) -> Value + Send + 'static,
) -> JoinHandle<Value> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path).unwrap();
    tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        let req: Value = serde_json::from_str(&line).unwrap();
        let reply = json!({"ok": true, "result": result(&req), "protocol_version": 1,
            "request_id": req["request_id"], "operation_id": req["operation_id"]});
        reader
            .get_mut()
            .write_all(format!("{reply}\n").as_bytes())
            .await
            .unwrap();
        req
    })
}

/// D7 follow-up acceptance: production startup binds the D1 DeliveryService
/// as Host7's integration port when `[playbooks.integration]` is configured
/// and source retention (aq-git) is present; candidate.prepare and
/// candidate.publish then reach a test aq-git peer through D1 unchanged.
#[tokio::test]
async fn host7_production_startup_prepares_and_publishes_through_delivery_service() {
    use aq_playbook_host::runner::{Intent, IntentStatus};
    use aq_server::playbooks::review_domain::{CatalogProfile, HOST7_CAPABILITIES};
    use aq_worker_protocol::{ObjectAlgorithm, ObjectId, SourceRole, TransferManifest};
    let s = setup().await;
    let (head, _) = s.text.split_once("[[playbooks.deployments]]").unwrap();
    let text = head.replace(
        "profile=\"human_review_v2\"",
        "profile=\"host_v7\"\ndeployments=[]",
    ) + "[playbooks.integration]\ncommit_name=\"AQ\"\ncommit_email=\"aq@example.test\"\nchecks=[\"unit\"]\n";
    let config: Config = toml::from_str(&text).unwrap();
    let mut state = s.f.state.clone();
    state.config = Arc::new(config);
    let loaded = state.config.playbooks.clone().unwrap();
    let token = s._dir.path().join("graph-token");
    private(&token, "g".repeat(48));
    state.agent_access = Some(Arc::new(
        aq_server::agent_access::Runtime::new(&aq_server::agent_access::AgentAccessConfig {
            graph_url: "http://127.0.0.1:9/".into(),
            graph_token_file: token,
            credential_ttl_ms: 10000,
            mutation_actor_id: None,
        })
        .await
        .unwrap(),
    ));
    // A configured integration policy without aq-git refuses startup.
    assert!(state.source_retention.is_none());
    assert!(
        aq_server::playbooks::runtime::Runtime::start(&state, loaded.clone())
            .await
            .is_err()
    );
    let socket = s._dir.path().join("git.sock");
    state.source_retention = Some(Arc::new(
        aq_server::source_retention::Runtime::new(
            aq_server::source_retention::SourceRetentionConfig {
                keep_per_task: 2,
                max_age: 30 * 86400,
                staging_root: s._dir.path().join("staging"),
                git_root: s._dir.path().join("git"),
                git_socket: socket.clone(),
                max_staging_bytes: 2097152,
                max_bundle_bytes: 4096,
                bulk_concurrency: 2,
                incomplete_ttl_secs: 60,
            },
        )
        .unwrap(),
    ));
    let runtime = aq_server::playbooks::runtime::Runtime::start(&state, loaded)
        .await
        .unwrap();
    let service = runtime.driver.service.clone();
    runtime.shutdown().await.unwrap();

    // D2 publication policy on the project's repository binding, and one
    // retained checkpoint of that repository (the D1 source receipt).
    let pool = s.f.db.pool().await;
    let l = &s.f.selection.lineage;
    sqlx::query("UPDATE project_execution_bindings SET binding = binding || '{\"publication\":{\"mode\":\"fast_forward\",\"target\":\"refs/heads/main\"}}' WHERE project_id=$1")
        .bind(l.project_id).execute(&pool).await.unwrap();
    let source = s.f.profiled.source.clone();
    let checkpoint = ObjectId {
        algorithm: ObjectAlgorithm::Sha1,
        hex: "b".repeat(40),
    };
    let binding = &s.f.profiled.offer.binding;
    let aq_session_protocol::SessionIdentity::Remote { session_id, .. } = binding.session else {
        unreachable!()
    };
    let m = TransferManifest {
        transfer_id: Uuid::new_v4(),
        role: SourceRole::Checkpoint,
        checkpoint: checkpoint.clone(),
        attempt: aq_worker_protocol::AttemptRef {
            attempt_id: binding.attempt_id,
            session_id,
            generation: 1,
        },
        resolved_spec_digest: "c".repeat(64),
        source: source.clone(),
        artifact_id: Uuid::new_v4(),
        manifest_digest: "d".repeat(64),
        bundle_digest: None,
        manifest_bytes: 1,
        bundle_bytes: 0,
    };
    let receipt_id = Uuid::new_v4();
    let receipt = json!({"receipt_id":receipt_id,"transfer_id":m.transfer_id,"repository_id":source.repository_id,"binding_generation":1,"source":checkpoint,"retention_handle":m.artifact_id,"manifest_digest":m.manifest_digest});
    let ack = json!({"receiver_installation_id":Uuid::new_v4(),"repository_id":source.repository_id,"binding_generation":1,"transfer_id":m.artifact_id,"source":checkpoint,"retention_handle":m.artifact_id,"manifest_digest":m.manifest_digest,"retention_state":"active","historical":false});
    sqlx::query("INSERT INTO source_transfers(transfer_id,attempt_id,node_id,repository_id,artifact_id,import_operation_id,manifest,state,receipt,central_receipt) VALUES($1,$2,$3,$4,$5,$6,$7,'retained',$8,$9)")
        .bind(m.transfer_id).bind(binding.attempt_id).bind(s.f.node.node_id).bind(source.repository_id).bind(m.artifact_id).bind(receipt_id).bind(json!(m)).bind(receipt).bind(ack).execute(&pool).await.unwrap();

    let revision: i64 = sqlx::query_scalar(
        "SELECT runtime_revision FROM playbook_namespaces WHERE namespace_id=$1",
    )
    .bind(s.namespace)
    .fetch_one(&pool)
    .await
    .unwrap();
    let owner = service
        .adopt_runtime(Uuid::new_v4(), revision)
        .await
        .unwrap();
    let scope = GrantScope {
        workspace_id: l.workspace_id,
        project_id: l.project_id,
        recipients: vec![],
        capabilities: BTreeSet::from(HOST7_CAPABILITIES.map(String::from)),
    };
    let now = (state.clock)().timestamp_millis();
    service
        .grant(s.f.actor, s.grant, scope.clone(), now + 10_000)
        .await
        .unwrap();
    let intent = |id: &str, name: &str, input: Value| Intent {
        id: id.into(),
        run: "synthetic-run".into(),
        step: name.replace('.', "-"),
        invocation: 1,
        deadline_at_ms: now + 5000,
        command: CatalogProfile::Host7.registry().commands[name].clone(),
        input_digest: aq_server::playbooks::domain::digest(&input).unwrap(),
        input,
        authority: CatalogProfile::Host7.authority(s.grant, 1, &scope).unwrap(),
        status: IntentStatus::Dispatched,
        cancel_requested: false,
        cancel_dispatched: false,
        handle: None,
        result: None,
        reason: None,
        late_results: vec![],
        completion_event: None,
    };

    // candidate.prepare -> D1 create_candidate -> aq-git candidate.create.
    let ready = checkpoint.clone();
    let peer = git_exchange(&socket, move |req| {
        json!({"candidate_id":req["operation_id"],"state":"ready","candidate":ready,
            "clean_prefix":ready,"retention_handle":req["operation_id"],"conflict_step":null,
            "conflicting_paths":[],"conflict_path_count":0,"conflict_paths_truncated":false})
    })
    .await;
    let base = format!("refs/heads/main@sha1:{}", source.base.hex);
    let prepare = json!({"base": {"id": base, "revision": 0},
        "sources": [{"id": format!("aq.source/{receipt_id}"), "revision": 1}]});
    let r = service
        .host_dispatch(
            &owner,
            s.grant,
            1,
            &intent("prepare-1", "candidate.prepare", prepare),
        )
        .await
        .unwrap();
    assert_eq!(r.outcome, "prepared", "{r:?}");
    let req = peer.await.unwrap();
    assert_eq!(req["operation"], "candidate.create", "{req}");
    let candidate = r.output["reference"].as_str().unwrap().to_string();

    // Passed check evidence for the candidate. Production has no check
    // executor bound yet, so the evidence row is seeded in its owning store.
    let check_op = Uuid::new_v4();
    sqlx::query("INSERT INTO playbook_host_operations(operation_id,namespace_id,generation,intent_id,command_name,subject_id,subject_revision,check_id,request,status,output,created_at_ms,completed_at_ms) VALUES($1,$2,1,'check-1','check.request',$3,1,'unit',$4,'passed','{}',$5,$5)")
        .bind(check_op).bind(s.namespace).bind(&candidate)
        .bind(json!({"execution_profile":"aq.check.sandbox/v1","subject":{"id":candidate,"revision":1},
            "source":{"format":"sha1","hex":checkpoint.hex}}))
        .bind(now).execute(&pool).await.unwrap();

    // candidate.publish -> D1 deliver (fast-forward policy) -> aq-git publish.
    let (old, new) = (source.base.clone(), checkpoint.clone());
    let peer = git_exchange(&socket, move |req| {
        json!({"outcome":"applied","target":req["params"]["target"],"old":old,"new":new,
            "acknowledged_at":"2026-11-02T00:00:00Z"})
    })
    .await;
    let publish = json!({"candidate": {"id": candidate, "revision": 1},
        "destination": {"id": base, "revision": 0},
        "evidence": [{"id": format!("check-receipt:{check_op}"), "revision": 1}]});
    let r = service
        .host_dispatch(
            &owner,
            s.grant,
            1,
            &intent("publish-1", "candidate.publish", publish),
        )
        .await
        .unwrap();
    assert_eq!(r.outcome, "published", "{r:?}");
    let req = peer.await.unwrap();
    assert_eq!(req["operation"], "publish", "{req}");
    assert_eq!(req["params"]["target"], "refs/heads/main", "{req}");
    service.retire_runtime(&owner).await.unwrap();
}
