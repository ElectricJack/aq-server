//! Actual protected component owner + bounded human catalog routes. Synthetic
//! fixture transport and records; no workflow admission or provider execution.
use super::*;
use axum::{Router, body::Body, http::Request};
use http_body_util::BodyExt;
use tower::ServiceExt;
async fn read(app: &Router, email: &str, path: &str) -> (u16, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::get(path)
                .header("x-aq-dev-email", email)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.headers().get("cache-control").unwrap(), "no-store");
    let status = response.status().as_u16();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}
async fn supported(app: &Router, email: &str) -> bool {
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
    let body: Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    body["data"]["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|cap| cap["name"] == "playbooks.catalog.read")
        .unwrap()["enabled"]
        == true
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_current_catalog_tracks_later_install_and_activation_without_restamping_startup() {
    let mut s = setup().await;
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
    assert!(runtime.driver.catalog_supported());
    assert!(supported(&app, "owner@localhost").await);
    let (status, first) = read(
        &app,
        "owner@localhost",
        "/api/playbooks/inspection/catalog/definitions?limit=1",
    )
    .await;
    assert_eq!(status, 200, "{first}");
    let schema: Value = serde_json::from_str(include_str!(
        "../../../../protocol/playbooks-current-catalog/v1/schema.json"
    ))
    .unwrap();
    assert!(
        aq_agent_api::check(&schema, &schema, &first),
        "current definition page must match the frozen closed schema"
    );
    assert_eq!(first["api_version"], "playbooks-current-catalog-1");
    assert_eq!(first["component_api_version"], "playbooks-catalog-1");
    assert_eq!(first["source"], "current_namespace_state");
    assert_eq!(first["current_catalog"], true);
    assert_eq!(first["items"].as_array().unwrap().len(), 1);
    let original: Artifact =
        serde_json::from_slice(&std::fs::read(&s.artifact_path).unwrap()).unwrap();
    assert_eq!(first["items"][0]["key"], original.digest);
    let (status, bindings) = read(
        &app,
        "owner@localhost",
        "/api/playbooks/inspection/catalog/activations",
    )
    .await;
    assert_eq!(status, 200, "{bindings}");
    assert_eq!(
        bindings["items"][0]["record"]["metadata"]["binding"],
        "mail"
    );
    assert_eq!(
        bindings["items"][0]["record"]["metadata"]["artifact"],
        original.digest
    );
    assert_eq!(bindings["items"][0]["record"]["metadata"]["revision"], "1");
    let mut content = original.content.clone();
    content.playbook.purpose =
        "Actual later installation: current metadata, not startup capture".into();
    content.playbook.id = "n".repeat(128);
    content.source.header.id = content.playbook.id.clone();
    let entry = content.playbook.entries.remove("main").unwrap();
    content.playbook.entries.insert("e".repeat(128), entry);
    content.source.header.interfaces = BTreeSet::from(["e".repeat(128)]);
    content.dependencies.insert(
        "d".repeat(128),
        CallableDependency {
            binding: "mail".into(),
            selection: DependencySelection::Exact,
            artifact: original.digest.clone(),
            interfaces: original.content.playbook.entries.clone(),
            capabilities: original.content.playbook.required_capabilities.clone(),
            scope: original.content.playbook.scope.clone(),
            dependencies: BTreeSet::new(),
        },
    );
    let later = Artifact::seal(content).unwrap();
    runtime.driver.install(later.clone()).await.unwrap();
    runtime
        .driver
        .activate("mail".into(), later.digest.clone(), s.grant, 1)
        .await
        .unwrap();
    let (_, current) = read(
        &app,
        "owner@localhost",
        "/api/playbooks/inspection/catalog/definitions",
    )
    .await;
    assert_eq!(current["items"].as_array().unwrap().len(), 2);
    assert!(current.to_string().contains(&later.digest));
    let (_, active) = read(
        &app,
        "owner@localhost",
        "/api/playbooks/inspection/catalog/activations",
    )
    .await;
    assert_eq!(
        active["items"][0]["record"]["metadata"]["artifact"],
        later.digest
    );
    assert_eq!(active["items"][0]["record"]["metadata"]["revision"], "2");
    let (_, historical) = read(
        &app,
        "owner@localhost",
        "/api/playbooks/inspection/loaded-definitions",
    )
    .await;
    assert_eq!(historical["current_catalog"], false);
    assert_eq!(historical["items"].as_array().unwrap().len(), 1);
    assert_eq!(historical["items"][0]["artifact"], original.digest);
    assert!(!historical.to_string().contains(&later.digest));
    let stale = format!(
        "/api/playbooks/inspection/catalog/definitions?after={}&incarnation={}&metadata_revision={}",
        first["items"][0]["key"].as_str().unwrap(),
        first["incarnation"].as_str().unwrap(),
        first["metadata_revision"].as_str().unwrap()
    );
    assert_eq!(read(&app, "owner@localhost", &stale).await.0, 409);
    let exact = format!(
        "/api/playbooks/inspection/catalog/definitions?exact={}",
        later.digest
    );
    let (_, detail) = read(&app, "owner@localhost", &exact).await;
    assert_eq!(detail["items"].as_array().unwrap().len(), 1);
    assert_eq!(detail["items"][0]["key"], later.digest);
    for secret in [
        "registry",
        "source_map",
        "required_capabilities",
        "password=test",
        "captured_values",
    ] {
        assert!(!current.to_string().contains(secret));
        assert!(!active.to_string().contains(secret));
    }
    assert!(
        runtime
            .driver
            .inspect_runs(s.f.actor, Default::default())
            .await
            .unwrap()
            .items
            .is_empty()
    );
    optional_browser(&s, &runtime, &original.digest, &later.digest).await;
    runtime.shutdown().await.unwrap();
    assert!(
        !supported(&app, "owner@localhost").await,
        "same mounted router must observe acknowledged owner stop"
    );
    assert_eq!(
        read(
            &app,
            "owner@localhost",
            "/api/playbooks/inspection/catalog/definitions"
        )
        .await
        .0,
        503
    );
}
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn current_catalog_refuses_foreign_scope_roles_bad_query_and_unmounted_owner() {
    let mut s = setup().await;
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
    for (email, role, expected) in [
        ("catalog-viewer@localhost", "viewer", 403),
        ("catalog-foreign@localhost", "owner", 404),
    ] {
        sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,$2,$3,'dev',$2,now())").bind(Uuid::new_v4()).bind(email).bind(role).execute(&s.f.db.pool().await).await.unwrap();
        assert_eq!(
            read(&app, email, "/api/playbooks/inspection/catalog/definitions")
                .await
                .0,
            expected
        );
        assert!(!supported(&app, email).await);
    }
    for suffix in [
        "?limit=26",
        "?limit=0",
        "?namespace=foreign",
        "?incarnation=1",
        "?after=bad",
        "?incarnation=01&metadata_revision=1",
    ] {
        assert_eq!(
            read(
                &app,
                "owner@localhost",
                &format!("/api/playbooks/inspection/catalog/definitions{suffix}")
            )
            .await
            .0,
            400
        );
    }
    assert_eq!(
        read(
            &app,
            "owner@localhost",
            &format!(
                "/api/playbooks/inspection/catalog/definitions?project_id={}",
                Uuid::new_v4()
            )
        )
        .await
        .0,
        404
    );
    let ordinary = aq_server::build_admin_router(s.f.state.clone());
    assert!(!supported(&ordinary, "owner@localhost").await);
    let response = ordinary
        .oneshot(
            Request::get("/api/playbooks/inspection/catalog/definitions")
                .header("x-aq-dev-email", "owner@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(s.f.actor)
        .execute(&s.f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(
        read(
            &app,
            "owner@localhost",
            "/api/playbooks/inspection/catalog/activations"
        )
        .await
        .0,
        403
    );
    sqlx::query("UPDATE users SET role='owner' WHERE id=$1")
        .bind(s.f.actor)
        .execute(&s.f.db.pool().await)
        .await
        .unwrap();
    runtime.shutdown().await.unwrap();
}

/// Optional actual browser proof on these two installed definitions and current
/// activation; ordinary tests require no Node, browser or compiled assets.
async fn optional_browser(
    s: &Setup,
    runtime: &aq_server::playbooks::runtime::Runtime,
    original: &str,
    later: &str,
) {
    let Some(script) = std::env::var_os("AQ_TEST_CURRENT_CATALOG_BROWSER_SCRIPT") else {
        assert!(std::env::var_os("AQ_TEST_CURRENT_CATALOG_DASHBOARD_DIR").is_none());
        return;
    };
    let bundle = std::env::var_os("AQ_TEST_CURRENT_CATALOG_DASHBOARD_DIR")
        .expect("explicit compiled current catalog assets required");
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
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let mut browser = tokio::process::Command::new("node")
        .arg(script)
        .env("AQ_LIVE_URL", origin)
        .env("AQ_LIVE_CATALOG_ORIGINAL", original)
        .env("AQ_LIVE_CATALOG_LATER", later)
        .env("AQ_LIVE_BUNDLE_MANIFEST_SHA256", bundle_digest)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(45), browser.wait()).await;
    server.abort();
    assert!(
        result
            .expect("bounded current catalog browser probe")
            .unwrap()
            .success()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn held_catalog_read_pins_original_owner_binding_and_authorization_version() {
    let mut s = setup().await;
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
    let pool = s.f.db.pool().await;
    let engine_namespace = runtime.driver.service.namespace.engine_namespace();
    let pid: i32 = sqlx::query_scalar(
        "SELECT owner_backend_pid FROM aq_playbooks.namespaces WHERE namespace=$1",
    )
    .bind(engine_namespace)
    .fetch_one(&pool)
    .await
    .unwrap();
    for action in [
        "subject",
        "issuer",
        "role",
        "same-value-update",
        "engine-error",
    ] {
        let mut blocker = pool.begin().await.unwrap();
        sqlx::query("LOCK TABLE aq_playbooks.catalog_rows IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *blocker)
            .await
            .unwrap();
        let held_app = app.clone();
        let pending = tokio::spawn(async move {
            read(
                &held_app,
                "owner@localhost",
                "/api/playbooks/inspection/catalog/definitions",
            )
            .await
        });
        tokio::time::timeout(Duration::from_secs(1),async {loop {
            let waiting:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE pid=$1 AND wait_event_type='Lock' AND query LIKE '%FROM aq_playbooks.catalog_rows WHERE namespace=%')").bind(pid).fetch_one(&pool).await.unwrap();
            if waiting {break;}tokio::time::sleep(Duration::from_millis(5)).await;
        }}).await.expect("actual owner catalog read must enter controlled storage wait after preauthorization");
        let sql = match action {
            "subject" | "engine-error" => {
                "UPDATE users SET subject='rebound-owner-subject' WHERE id=$1"
            }
            "issuer" => "UPDATE users SET issuer='rebound-owner-issuer' WHERE id=$1",
            "role" => "UPDATE users SET role='viewer' WHERE id=$1",
            _ => "UPDATE users SET role='owner' WHERE id=$1",
        };
        sqlx::query(sql)
            .bind(s.f.actor)
            .execute(&pool)
            .await
            .unwrap();
        let deferred_blocker = if action == "engine-error" {
            Some(blocker)
        } else {
            blocker.rollback().await.unwrap();
            None
        };
        let (status, body) = tokio::time::timeout(Duration::from_secs(3), pending)
            .await
            .unwrap()
            .unwrap();
        if let Some(blocker) = deferred_blocker {
            blocker.rollback().await.unwrap();
        }
        assert_eq!(status, 403, "held {action}: {body}");
        assert!(body.get("items").is_none());
        assert!(!body.to_string().contains("current_namespace_state"));
        sqlx::query(
            "UPDATE users SET role='owner',issuer='dev',subject='owner@localhost' WHERE id=$1",
        )
        .bind(s.f.actor)
        .execute(&pool)
        .await
        .unwrap();
        assert_eq!(
            read(
                &app,
                "owner@localhost",
                "/api/playbooks/inspection/catalog/definitions"
            )
            .await
            .0,
            200,
            "fresh original binding reads after restoration"
        );
    }
    runtime.shutdown().await.unwrap();
    assert!(!supported(&app, "owner@localhost").await);
}

fn explicit_empty_deployments(text: &str) -> String {
    let (owner, _) = text.split_once("[[playbooks.deployments]]").unwrap();
    owner.replace("[playbooks]\n", "[playbooks]\ndeployments=[]\n")
}

async fn no_work_counts(pool: &sqlx::PgPool) -> Vec<i64> {
    let mut counts = vec![];
    for table in [
        "execution_attempts",
        "playbook_admissions",
        "playbook_effects",
        "playbook_cancellations",
        "playbook_outbox",
        "human_graph_operations",
        "agent_graph_operations",
        "agent_graph_outbox",
        "mailbox_messages",
        "mailbox_deliveries",
        "notification_messages",
        "notification_deliveries",
        "review_subjects",
        "human_decision_requests",
        "human_decision_finality",
        "review_owner_outbox",
        "observation_events",
    ] {
        counts.push(
            sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
                .fetch_one(pool)
                .await
                .unwrap(),
        );
    }
    counts
}

async fn idle_quanta(runtime: &aq_server::playbooks::runtime::Runtime) {
    for _ in 0..2 {
        let result = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                match runtime.driver.quantum().await {
                    Err(Error::Overloaded) => tokio::task::yield_now().await,
                    result => break result,
                }
            }
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result["admissions_checked"], 0);
        assert_eq!(result["events_checked"], 0);
        assert_eq!(result["tick"]["transitions"], 0);
    }
}

async fn no_startup_snapshot(app: &Router) {
    let (status, bootstrap) = read(app, "owner@localhost", "/api/v1/bootstrap").await;
    assert_eq!(status, 200);
    let capabilities = bootstrap["data"]["capabilities"].as_array().unwrap();
    for (name, expected) in [
        ("playbooks.catalog.read", true),
        ("playbooks.loaded-definitions.read", false),
        ("playbooks.inspect", true),
        ("decisions.read", true),
    ] {
        assert_eq!(
            capabilities.iter().find(|cap| cap["name"] == name).unwrap()["enabled"],
            expected,
            "unexpected availability for {name}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_explicit_empty_owner_has_current_catalog_without_grants_or_startup_snapshot() {
    let mut s = setup().await;
    s.text = explicit_empty_deployments(&s.text);
    // [] is an explicit operator choice. Omitting the field stays invalid.
    assert!(toml::from_str::<Config>(&s.text.replace("deployments=[]\n", "")).is_err());
    s.f.state.config = Arc::new(toml::from_str::<Config>(&s.text).unwrap());
    let pool = s.f.db.pool().await;
    let before = no_work_counts(&pool).await;
    let mut previous_incarnation = None;
    for _ in 0..2 {
        let runtime = aq_server::playbooks::runtime::Runtime::start(
            &s.f.state,
            s.f.state.config.playbooks.clone().unwrap(),
        )
        .await
        .unwrap();
        assert!(runtime.driver.catalog_supported());
        assert!(!runtime.driver.loaded_definitions_available());
        let app = aq_server::build_admin_router_with_policy_owners(
            s.f.state.clone(),
            Some(runtime.driver.clone()),
            Some(runtime.reviews.clone()),
        );
        no_startup_snapshot(&app).await;
        let mut current_incarnation = None;
        for kind in ["definitions", "activations"] {
            let (status, page) = read(
                &app,
                "owner@localhost",
                &format!("/api/playbooks/inspection/catalog/{kind}"),
            )
            .await;
            assert_eq!(status, 200);
            assert_eq!(page["source"], "current_namespace_state");
            assert_eq!(page["current_catalog"], true);
            assert_eq!(page["namespace"]["id"], json!(s.namespace));
            assert_eq!(page["items"], json!([]));
            assert_eq!(page["next_cursor"], Value::Null);
            let incarnation = page["incarnation"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap();
            assert!(current_incarnation.is_none_or(|value| value == incarnation));
            current_incarnation = Some(incarnation);
        }
        if let Some(previous) = previous_incarnation {
            assert_eq!(current_incarnation, Some(previous + 1));
        }
        previous_incarnation = current_incarnation;
        for table in [
            "playbook_grants",
            "playbook_admissions",
            "aq_playbooks.namespace_artifacts",
            "aq_playbooks.artifacts",
            "aq_playbooks.encrypted_artifacts",
            "aq_playbooks.catalog_rows",
        ] {
            assert_eq!(
                sqlx::query_scalar::<_, i64>(&format!("SELECT count(*) FROM {table}"))
                    .fetch_one(&pool)
                    .await
                    .unwrap(),
                0,
                "empty configuration created unexpected {table}"
            );
        }
        idle_quanta(&runtime).await;
        assert_eq!(no_work_counts(&pool).await, before);
        runtime.shutdown().await.unwrap();
        assert!(!supported(&app, "owner@localhost").await);
    }
    assert_eq!(no_work_counts(&pool).await, before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn actual_empty_startup_preserves_installed_definitions_and_activations() {
    let mut s = setup().await;
    s.f.state.config = Arc::new(toml::from_str::<Config>(&s.text).unwrap());
    let pool = s.f.db.pool().await;
    let before = no_work_counts(&pool).await;
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
    let mut retained = vec![];
    for kind in ["definitions", "activations"] {
        let (status, page) = read(
            &app,
            "owner@localhost",
            &format!("/api/playbooks/inspection/catalog/{kind}"),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(page["items"].as_array().unwrap().len(), 1);
        retained.push(page);
    }
    let grant: Value = sqlx::query_scalar("SELECT to_jsonb(g) FROM playbook_grants g")
        .fetch_one(&pool)
        .await
        .unwrap();
    runtime.shutdown().await.unwrap();

    // The old artifact file is deliberately absent: [] must neither reload it
    // nor interpret absence from configuration as an uninstall instruction.
    std::fs::remove_file(&s.artifact_path).unwrap();
    s.text = explicit_empty_deployments(&s.text);
    s.f.state.config = Arc::new(toml::from_str::<Config>(&s.text).unwrap());
    let runtime = aq_server::playbooks::runtime::Runtime::start(
        &s.f.state,
        s.f.state.config.playbooks.clone().unwrap(),
    )
    .await
    .unwrap();
    let app = aq_server::build_admin_router_with_policy_owners(
        s.f.state.clone(),
        Some(runtime.driver.clone()),
        Some(runtime.reviews.clone()),
    );
    no_startup_snapshot(&app).await;
    assert!(!runtime.driver.loaded_definitions_available());
    for (kind, previous) in ["definitions", "activations"].into_iter().zip(retained) {
        let (status, page) = read(
            &app,
            "owner@localhost",
            &format!("/api/playbooks/inspection/catalog/{kind}"),
        )
        .await;
        assert_eq!(status, 200);
        assert_eq!(page["items"], previous["items"]);
        assert_eq!(page["source"], "current_namespace_state");
        assert_eq!(page["current_catalog"], true);
        assert_eq!(
            page["incarnation"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap(),
            previous["incarnation"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap()
                + 1
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, Value>("SELECT to_jsonb(g) FROM playbook_grants g")
            .fetch_one(&pool)
            .await
            .unwrap(),
        grant
    );
    idle_quanta(&runtime).await;
    assert_eq!(no_work_counts(&pool).await, before);
    runtime.shutdown().await.unwrap();
    assert!(!supported(&app, "owner@localhost").await);
}
