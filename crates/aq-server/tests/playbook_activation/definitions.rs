//! Actual configured startup metadata; deliberately not an engine catalog cache.
use super::*;
use axum::{Router, body::Body, http::Request};
use http_body_util::BodyExt;
use tower::ServiceExt;

async fn read(app: &Router, email: &str, query: &str) -> (u16, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::get(format!(
                "/api/playbooks/inspection/loaded-definitions{query}"
            ))
            .header("x-aq-dev-email", email)
            .body(Body::empty())
            .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

async fn capability(app: &Router, email: &str) -> bool {
    let response = app
        .clone()
        .oneshot(
            Request::get("/api/v1/bootstrap")
                .header("x-aq-dev-email", email)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    body["data"]["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|cap| cap["name"] == "playbooks.loaded-definitions.read")
        .unwrap()["enabled"]
        == true
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_startup_definitions_are_redacted_historical_and_do_not_read_engine_snapshot() {
    let mut s = setup().await;
    let original: Artifact =
        serde_json::from_slice(&std::fs::read(&s.artifact_path).unwrap()).unwrap();
    let mut content = original.content;
    content.playbook.purpose = "é".repeat(3000);
    let original = Artifact::seal(content).unwrap();
    private(&s.artifact_path, serde_json::to_vec(&original).unwrap());
    s.f.state.config = Arc::new(toml::from_str::<Config>(&s.text).unwrap());
    let runtime = aq_server::playbooks::runtime::Runtime::start(
        &s.f.state,
        s.f.state.config.playbooks.clone().unwrap(),
    )
    .await
    .unwrap();
    assert!(runtime.driver.loaded_definitions_available());
    let app = aq_server::build_admin_router_with_playbooks(
        s.f.state.clone(),
        Some(runtime.driver.clone()),
    );
    assert!(capability(&app, "owner@localhost").await);
    let (status, page) = read(&app, "owner@localhost", "").await;
    assert_eq!(status, 200, "{page}");
    let schema: Value = serde_json::from_str(include_str!(
        "../../../../protocol/playbooks-loaded-definitions/v1/schema.json"
    ))
    .unwrap();
    assert!(
        aq_agent_api::check(&schema, &schema, &page),
        "public loaded definition DTO schema mismatch"
    );
    assert_eq!(page["api_version"], "playbooks-loaded-definitions-1");
    assert_eq!(page["namespace"]["id"], json!(s.namespace));
    assert_eq!(page["source"], "runtime_startup_configuration");
    assert_eq!(page["current_catalog"], false);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    let item = &page["items"][0];
    assert_eq!(item["artifact"], original.digest);
    assert_eq!(
        item["executable_fingerprint"],
        original.executable_fingerprint
    );
    assert_eq!(item["source_digest"], original.content.source.digest);
    assert_eq!(item["name"], original.content.playbook.id);
    assert_eq!(item["entries"], json!(["main"]));
    assert_eq!(item["dependency_pins"], json!({}));
    assert_eq!(item["purpose_truncated"], true);
    assert_eq!(item["purpose"].as_str().unwrap().len(), 4096);
    assert_eq!(item["purpose"], "é".repeat(2048));
    for private in [
        "synthetic optional-owner mailbox delivery",
        "source_map",
        "registry",
        "artifact_file",
        "password=test",
        "required_capabilities",
    ] {
        assert!(!page.to_string().contains(private), "leaked {private}");
    }
    // A real later install does not make startup configuration a current catalog.
    let mut later = original.content.clone();
    later.playbook.purpose = "Later installation, not loaded startup configuration".into();
    let later = Artifact::seal(later).unwrap();
    runtime.driver.install(later.clone()).await.unwrap();
    let (_, after) = read(&app, "owner@localhost", "").await;
    assert_eq!(after["items"], page["items"]);
    assert!(!after.to_string().contains(&later.digest));
    // Block actual component snapshot reads. Metadata only authorizes against
    // AQ's existing public namespace/user owner, then copies the <=4 startup rows.
    let pool = s.f.db.pool().await;
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE aq_playbooks.namespaces IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let (status, unlocked_read) =
        tokio::time::timeout(Duration::from_secs(1), read(&app, "owner@localhost", ""))
            .await
            .expect("startup metadata must not wait for the engine snapshot table");
    assert_eq!(status, 200);
    assert_eq!(unlocked_read["items"], page["items"]);
    lock.rollback().await.unwrap();
    runtime.shutdown().await.unwrap();
}

#[tokio::test]
async fn loaded_definitions_refuse_foreign_scope_roles_unknown_query_and_absent_runtime() {
    let mut s = setup().await;
    let original: Artifact =
        serde_json::from_slice(&std::fs::read(&s.artifact_path).unwrap()).unwrap();
    let mut content = original.content.clone();
    // Legal maximum-length declarations also exercise actual 320px reflow in
    // the optional browser path; these are not mock responses or DOM overrides.
    content.playbook.id = "n".repeat(128);
    content.source.header.id = content.playbook.id.clone();
    let entry = content.playbook.entries.remove("main").unwrap();
    content
        .playbook
        .entries
        .insert("e".repeat(128), entry.clone());
    content.source.header.interfaces = BTreeSet::from(["e".repeat(128)]);
    content.dependencies.insert(
        "d".repeat(128),
        CallableDependency {
            binding: "fixture-child".into(),
            selection: DependencySelection::Exact,
            artifact: original.digest,
            interfaces: BTreeMap::from([("main".into(), entry)]),
            capabilities: content.playbook.required_capabilities.clone(),
            scope: "project".into(),
            dependencies: BTreeSet::new(),
        },
    );
    let artifact = Artifact::seal(content).unwrap();
    artifact.verify().unwrap();
    private(&s.artifact_path, serde_json::to_vec(&artifact).unwrap());
    s.f.state.config = Arc::new(toml::from_str::<Config>(&s.text).unwrap());
    let runtime = aq_server::playbooks::runtime::Runtime::start(
        &s.f.state,
        s.f.state.config.playbooks.clone().unwrap(),
    )
    .await
    .unwrap();
    let app = aq_server::build_admin_router_with_playbooks(
        s.f.state.clone(),
        Some(runtime.driver.clone()),
    );
    optional_browser(&s, &runtime).await;
    let pool = s.f.db.pool().await;
    for (email, role, expected) in [
        ("definitions-viewer@localhost", "viewer", 403),
        ("definitions-owner@localhost", "owner", 404),
    ] {
        sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,$2,$3,'dev',$2,now())")
            .bind(Uuid::new_v4()).bind(email).bind(role).execute(&pool).await.unwrap();
        assert_eq!(read(&app, email, "").await.0, expected);
        assert!(!capability(&app, email).await);
    }
    assert_eq!(
        read(
            &app,
            "owner@localhost",
            &format!("?project_id={}", Uuid::new_v4())
        )
        .await
        .0,
        404
    );
    // Axum's malformed-query rejection is not a success/empty definition set.
    let response = app
        .clone()
        .oneshot(
            Request::get("/api/playbooks/inspection/loaded-definitions?limit=25")
                .header("x-aq-dev-email", "owner@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(s.f.actor)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(read(&app, "owner@localhost", "").await.0, 403);
    sqlx::query("UPDATE users SET role='owner' WHERE id=$1")
        .bind(s.f.actor)
        .execute(&pool)
        .await
        .unwrap();
    runtime.shutdown().await.unwrap();
    let plain = aq_server::build_admin_router(s.f.state.clone());
    assert!(!capability(&plain, "owner@localhost").await);
    let response = plain
        .oneshot(
            Request::get("/api/playbooks/inspection/loaded-definitions")
                .header("x-aq-dev-email", "owner@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
}

/// Optional actual compiled-browser path. It reuses this configured protected
/// fixture and never requires a sibling UI, Node or browser in ordinary tests.
async fn optional_browser(s: &Setup, runtime: &aq_server::playbooks::runtime::Runtime) {
    let Some(script) = std::env::var_os("AQ_TEST_LOADED_DEFINITIONS_BROWSER_SCRIPT") else {
        assert!(std::env::var_os("AQ_TEST_LOADED_DEFINITIONS_DASHBOARD_DIR").is_none());
        return;
    };
    let bundle = std::env::var_os("AQ_TEST_LOADED_DEFINITIONS_DASHBOARD_DIR")
        .expect("loaded definition browser requires explicit compiled assets");
    let assets = aq_server::dashboard_assets::DashboardAssets::load(Path::new(&bundle)).unwrap();
    let bundle_digest = assets.digest().to_owned();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let mut state = s.f.state.clone();
    let mut config = (*state.config).clone();
    config.server.public_origin = origin.clone();
    state.config = Arc::new(config);
    let app = aq_server::build_admin_router_with_dashboard_and_policy_owners(
        state,
        Some(runtime.driver.clone()),
        Some(runtime.reviews.clone()),
        Arc::new(assets),
    );
    let artifact: Artifact =
        serde_json::from_slice(&std::fs::read(&s.artifact_path).unwrap()).unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut browser = tokio::process::Command::new("node")
        .arg(script)
        .env("AQ_LIVE_URL", origin)
        .env("AQ_LIVE_LOADED_ARTIFACT", artifact.digest)
        .env("AQ_LIVE_BUNDLE_MANIFEST_SHA256", bundle_digest)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(30), browser.wait()).await;
    server.abort();
    assert!(
        result
            .expect("bounded loaded definition browser probe")
            .unwrap()
            .success()
    );
}
