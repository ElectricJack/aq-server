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
    let env = reqwest::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
    assert_eq!(env.host_str(), Some("127.0.0.1"));
    assert_eq!(env.port(), Some(55432));
    let listener = UnixListener::bind(path.join(".s.PGSQL.5432")).unwrap();
    let connections = Arc::new(Mutex::new(vec![]));
    let saved = connections.clone();
    let task = tokio::spawn(async move {
        loop {
            let (mut unix, _) = listener.accept().await.unwrap();
            let task = tokio::spawn(async move {
                let mut tcp = TcpStream::connect("127.0.0.1:55432").await.unwrap();
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
    let r = client()
        .get(format!("{base}{path}"))
        .header("x-aq-dev-email", "owner@localhost")
        .send()
        .await
        .unwrap();
    let status = r.status();
    let body: Value = r.json().await.unwrap();
    assert!(status.is_success(), "{status}: {body}");
    body
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
    assert_eq!(detail["observation_is_atomic"], false);
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
    for path in [
        "/api/playbooks/inspection/runs?limit=26".to_string(),
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
