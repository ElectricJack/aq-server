//! Loopback browser pilot using disposable PostgreSQL and a local tmux peer.
use super::*;
use aq_server::{
    build_admin_router_with_dashboard, build_worker_router, dashboard_assets::DashboardAssets,
};
use serde_json::json;
use std::{io::Write, os::unix::fs::OpenOptionsExt, path::PathBuf, time::Duration};
use tokio::net::TcpListener;

#[tokio::test]
#[ignore = "manual browser pilot; run with scripts/task-terminal-pilot.py"]
async fn browser_pilot() {
    let runtime = PathBuf::from(std::env::var_os("AQ_TERMINAL_PILOT_DIR").unwrap());
    assert_eq!(
        std::fs::metadata(&runtime).unwrap().permissions().mode() & 0o777,
        0o700
    );
    let dist = PathBuf::from(std::env::var_os("AQ_TERMINAL_PILOT_DASHBOARD_DIR").unwrap());
    let dashboard = Arc::new(DashboardAssets::load(&dist).expect("verified dashboard assets"));
    let admin = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let worker = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", admin.local_addr().unwrap());
    let worker_address = worker.local_addr().unwrap();
    let uuid_env = |key| std::env::var(key).unwrap().parse::<Uuid>().unwrap();
    let project = uuid_env("AQ_TERMINAL_PILOT_HOME");
    let workspace = uuid_env("AQ_TERMINAL_PILOT_WORKSPACE");
    let task = uuid_env("AQ_TERMINAL_PILOT_TASK");
    let baseline = fixture().await;
    let mut fixture = common::profiled::fixture_on(
        baseline.db.clone(),
        Some((
            baseline.actor,
            aq_selection::TaskLineage {
                project_id: project,
                workspace_id: workspace,
                task_id: task,
            },
        )),
    )
    .await;
    let graph =
        aq_server::agent_access::Runtime::new(&aq_server::agent_access::AgentAccessConfig {
            graph_url: std::env::var("AQ_TERMINAL_PILOT_GRAPH_ORIGIN").unwrap(),
            graph_token_file: PathBuf::from(
                std::env::var_os("AQ_TERMINAL_PILOT_GRAPH_TOKEN").unwrap(),
            ),
            mutation_actor_id: Some("pilot-admin".into()),
            credential_ttl_ms: 120_000,
        })
        .await
        .unwrap();
    graph.verify_workspace(workspace).await.unwrap();
    fixture.state.agent_access = Some(Arc::new(graph));
    let offer = &mut fixture.profiled.offer;
    // The retained ticket concerns the live tmux path when finite input is off.
    offer.manifest.terminal.input_enabled = false;
    offer.binding.launch_manifest_digest =
        aq_session_protocol::canonical_digest(&offer.manifest).unwrap();
    let (mut f, started) = common::native_started::setup_fixture(fixture, true, true, None).await;
    let auth = common::native_started::context(&f).await;
    let attempt = started.binding.attempt_id;
    let aq_session_protocol::SessionIdentity::Remote { session_id, .. } =
        started.binding.session.clone()
    else {
        panic!("pilot requires a remote session");
    };
    aq_server::profiled_execution::started::record(&f.state, &auth, &started)
        .await
        .unwrap();
    let key = runtime.join("terminal.key");
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&key)
        .unwrap();
    file.write_all(Uuid::new_v4().as_bytes()).unwrap();
    file.write_all(Uuid::new_v4().as_bytes()).unwrap();
    let mut config = (*f.state.config).clone();
    config.terminals = Some(relay_config(key, false));
    config.server.public_origin.clone_from(&origin);
    f.state.config = Arc::new(config);
    aq_server::terminal::persistence::install(&f.state)
        .await
        .unwrap();
    let authority: i64 = sqlx::query_scalar(
        "SELECT authority_revision FROM profiled_authority_hot WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_one(&f.db.pool().await)
    .await
    .unwrap();
    let r = Register {
        version: 1,
        capability: Text::new(CAPABILITY).unwrap(),
        operation_id: Uuid::new_v4(),
        peer: Peer::Remote {
            node_id: f.node.node_id,
            boot_id: f.node.boot_id,
        },
        terminal: TerminalIdentity {
            session: started.binding.session,
            terminal_id: Uuid::new_v4(),
            epoch: 1,
        },
        expected_authority_revision: u64::try_from(authority).unwrap(),
        input_mode: InputMode::FiniteV1,
        advertised_capabilities: vec![Capability {
            id: Text::new(CAPABILITY).unwrap(),
            version: 1,
        }],
    };
    register(&f, &r).await;
    let published = service::inspect(&f.state, f.actor, r.terminal.terminal_id)
        .await
        .unwrap();
    assert!(!published.terminal_policy.input_enabled);
    let temporary = runtime.join("peer.json.tmp");
    let mut manifest = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&temporary)
        .unwrap();
    serde_json::to_writer(&mut manifest, &json!({
        "origin": origin,
        "worker_websocket": format!("ws://{worker_address}/v1/worker/attempt-terminals/{attempt}/terminal-stream"),
        "worker_token": f.credential,
        "task_id": f.selection.lineage.task_id,
        "attempt_id": attempt,
        "session_id": session_id,
        "terminal_id": r.terminal.terminal_id,
    }))
    .unwrap();
    std::fs::rename(&temporary, runtime.join("peer.json")).unwrap();
    let admin_app = build_admin_router_with_dashboard(f.state.clone(), dashboard);
    let worker_app = build_worker_router(f.state.clone());
    tokio::select! {
        result = axum::serve(admin, admin_app).into_future() => result.unwrap(),
        result = axum::serve(worker, worker_app).into_future() => result.unwrap(),
        () = async {
            while !runtime.join("stop").exists() {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        } => {}
    }
}
