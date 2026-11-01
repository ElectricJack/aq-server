//! Opt-in real protected engine + owner + compiled dashboard browser fixture.
//! The labelled development workflow is admitted through normal typed owners.
use super::*;

fn parent(child: &Artifact) -> Artifact {
    let mut content = child.content.clone();
    content.source.header.id = "dev-playbook-inspection".into();
    content.playbook.id = "dev-playbook-inspection".into();
    content.playbook.purpose =
        "Labelled development browser workflow with an actual child run".into();
    content.playbook.entries.get_mut("main").unwrap().start = "call".into();
    content.playbook.steps.remove("send");
    content.playbook.steps.insert(
        "call".into(),
        Step {
            op: Operation::Call {
                dependency: "mail-child".into(),
                entry: "main".into(),
                input: Expr::literal(Value::Null),
                mode: CallMode::Wait,
                bind: "child_result".into(),
            },
            edges: BTreeMap::from([("ok".into(), "done".into())]),
            on_error: None,
        },
    );
    content.dependencies.insert(
        "mail-child".into(),
        CallableDependency {
            binding: "mail".into(),
            selection: DependencySelection::Exact,
            artifact: child.digest.clone(),
            interfaces: child.content.playbook.entries.clone(),
            capabilities: child.content.playbook.required_capabilities.clone(),
            scope: child.content.playbook.scope.clone(),
            dependencies: BTreeSet::new(),
        },
    );
    Artifact::seal(content).unwrap()
}

#[tokio::test]
#[ignore = "requires explicit compiled dashboard and actual browser probe; no provider"]
async fn actual_protected_run_and_child_reach_compiled_playbooks_page() {
    let script =
        std::env::var_os("AQ_TEST_PLAYBOOK_BROWSER_SCRIPT").expect("explicit browser probe");
    let bundle =
        std::env::var_os("AQ_TEST_PLAYBOOK_DASHBOARD_DIR").expect("explicit compiled bundle");
    let mut s = setup().await;
    s.f.state.config = Arc::new(toml::from_str::<Config>(&s.text).unwrap());
    let runtime = aq_server::playbooks::runtime::Runtime::start(
        &s.f.state,
        s.f.state.config.playbooks.clone().unwrap(),
    )
    .await
    .unwrap();
    let child: Artifact =
        serde_json::from_slice(&std::fs::read(&s.artifact_path).unwrap()).unwrap();
    let caller = parent(&child);
    runtime.driver.install(caller.clone()).await.unwrap();
    let active = runtime
        .driver
        .activate("dev-inspection".into(), caller.digest.clone(), s.grant, 1)
        .await
        .unwrap();
    let request = PrepareRequest {
        request_id: Uuid::new_v4(),
        grant_id: s.grant,
        grant_revision: 1,
        binding: "dev-inspection".into(),
        expected_activation_revision: active["revision"].as_u64().unwrap(),
        entry: "main".into(),
        input: Value::Null,
    };
    runtime
        .driver
        .service
        .prepare(s.f.actor, request.clone())
        .await
        .unwrap();
    let mut admitted = None;
    for _ in 0..100 {
        let status = runtime
            .driver
            .service
            .admission(s.f.actor, request.request_id)
            .await
            .unwrap();
        if let Some(receipt) = status.status.receipt {
            let detail = runtime
                .driver
                .inspect_run_timeline(s.f.actor, receipt.run_id.clone(), Default::default())
                .await
                .unwrap();
            if detail.run.status == "completed" && !detail.run.children.is_empty() {
                admitted = Some(detail);
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let actual = admitted.expect("actual completed parent and child required");
    assert_eq!(actual.run.artifact, caller.digest);
    assert_eq!(actual.run.children.len(), 1);
    let actual_child = runtime
        .driver
        .inspect_run_timeline(
            s.f.actor,
            actual.run.children[0].clone(),
            Default::default(),
        )
        .await
        .unwrap();
    assert_eq!(actual_child.run.artifact, child.digest);
    assert_eq!(actual_child.run.status, "completed");
    assert!(!actual.receipts.items.is_empty());
    let assets =
        aq_server::dashboard_assets::DashboardAssets::load(std::path::Path::new(&bundle)).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let mut state = s.f.state.clone();
    let mut config = (*state.config).clone();
    config.server.public_origin = origin.clone();
    state.config = Arc::new(config);
    let router = aq_server::build_admin_router_with_dashboard_and_policy_owners(
        state,
        Some(runtime.driver.clone()),
        Some(runtime.reviews.clone()),
        Arc::new(assets),
    );
    let server = tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    let mut browser = tokio::process::Command::new("node")
        .arg(script)
        .env("AQ_LIVE_URL", &origin)
        .env("AQ_LIVE_RUN_ID", &actual.run.id)
        .env("AQ_LIVE_CHILD_RUN_ID", &actual.run.children[0])
        .env("AQ_LIVE_PLAYBOOK_ARTIFACT", &caller.digest)
        .env("AQ_LIVE_CHILD_ARTIFACT", &child.digest)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(45), browser.wait()).await;
    server.abort();
    runtime.shutdown().await.unwrap();
    assert!(result.expect("bounded browser probe").unwrap().success());
    eprintln!(
        "actual Playbooks browser: protected engine admission, pinned parent/child, public receipts and compiled dashboard; no interception/provider"
    );
}
