//! Swarm read (spec 0007 phase 2): machine grouping, load, agents, states.
use crate::common;
use aq_server::execution::profiled::{Status, Stop};
use aq_session_protocol::ProfiledGrant;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

async fn read(app: &Router, path: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::get(path)
                .header("x-aq-dev-email", "owner@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

fn swarm_fixture(value: &Value) {
    if std::env::var_os("AQ_DASHBOARD_WRITE_FIXTURES").is_some() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../protocol/dashboard-read/v1/fixtures/swarm.json");
        std::fs::write(
            path,
            format!("{}\n", serde_json::to_string_pretty(value).unwrap()),
        )
        .unwrap();
    }
}

/// Makes the fixture's attempt a running one (granted, stop `Running`).
fn make_running(f: &mut common::profiled::Fixture) {
    let now = common::profiled::NOW;
    f.profiled.status = Status::Granted;
    f.profiled.execution_deadline_ms = Some(now + 60_000);
    f.profiled.grant = Some(ProfiledGrant {
        binding: f.profiled.offer.binding.clone(),
        grant_id: Uuid::new_v4(),
        expires_at_ms: now + 5_000,
    });
    f.profiled.stop = Stop::Running;
}

async fn overview_running(app: &Router) -> Value {
    read(app, "/api/v1/overview").await.1["data"]["counts"]["running"]["value"].clone()
}

fn listed_agents(body: &Value) -> Vec<&Value> {
    body["data"]["machines"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["nodes"].as_array().unwrap())
        .flat_map(|n| n["agents"].as_array().unwrap())
        .collect()
}

async fn add_node(pool: &sqlx::PgPool, name: &str, seen_ms: Option<u64>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at,last_seen_at) VALUES($1,$2,$3,'t',1,now(),to_timestamp($4::double precision/1000))")
        .bind(id).bind(name).bind(id.to_string()).bind(seen_ms.map(|s| s as f64)).execute(pool).await.unwrap();
    id
}

fn windows_report() -> aq_swarm::machine::MachineReport {
    serde_json::from_str(include_str!(
        "../../../../protocol/worker-machine/v1/fixtures/report-windows-host.json"
    ))
    .unwrap()
}

#[tokio::test]
async fn swarm_requires_owner() {
    let f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'swarm-viewer@localhost','viewer','dev','swarm-viewer@localhost',now())")
        .bind(Uuid::new_v4()).execute(&pool).await.unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let viewer = app
        .oneshot(
            Request::get("/api/v1/swarm")
                .header("x-aq-dev-email", "swarm-viewer@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(viewer.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn swarm_groups_nodes_reports_load_and_lists_running_agents() {
    let now = common::profiled::NOW;
    let mut f = common::profiled::fixture().await;
    make_running(&mut f);
    let attempt = common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    sqlx::query(
        "UPDATE nodes SET last_seen_at=to_timestamp($2::double precision/1000) WHERE id=$1",
    )
    .bind(f.node.node_id)
    .bind(now as i64)
    .execute(&pool)
    .await
    .unwrap();
    let second = add_node(&pool, "local-2", Some(now)).await;
    let registration = json!({"backend":{"kind":"docker-local","capabilities":[],"max_concurrent_sandboxes":2,"spend_ceiling":null,"templates":[]}});
    sqlx::query("INSERT INTO delegated_node_boots(node_id,boot_id,registration) VALUES($1,$2,$3)")
        .bind(second)
        .bind(Uuid::new_v4())
        .bind(registration)
        .execute(&pool)
        .await
        .unwrap();
    for node in [f.node.node_id, second] {
        aq_swarm::machine::record(&pool, node, &windows_report(), now)
            .await
            .unwrap();
    }
    // A machine whose report has no sample in the window: averages are null.
    let quiet = add_node(&pool, "quiet", Some(now)).await;
    let mut report = windows_report();
    report.machine.key = Some("quiet-box".into());
    report.machine.label = Some("quiet-box".into());
    sqlx::query("INSERT INTO node_machine_reports(node_id,report,received_at_ms) VALUES($1,$2,$3)")
        .bind(quiet)
        .bind(serde_json::to_value(&report).unwrap())
        .bind(now as i64)
        .execute(&pool)
        .await
        .unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, body) = read(&app, "/api/v1/swarm").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let machines = body["data"]["machines"].as_array().unwrap();
    assert_eq!(machines.len(), 2, "{body}");
    let q = &machines[1];
    assert_eq!(q["label"], "quiet-box");
    assert_eq!(q["load"]["samples_15m"], 0);
    assert!(q["load"]["cpu_avg_15m_bps"].is_null());
    assert!(q["load"]["memory_avg_15m_bps"].is_null());
    let machine = &machines[0];
    assert_eq!(machine["label"], "desktop");
    assert_eq!(machine["nodes"].as_array().unwrap().len(), 2);
    assert_eq!(machine["load"]["source"], "windows_host");
    assert_eq!(machine["load"]["fresh"], true);

    let (_, list) = read(&app, "/api/v1/sessions").await;
    let session = list["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["attempt_id"] == json!(attempt))
        .unwrap()["id"]
        .clone();
    let (_, detail) = read(
        &app,
        &format!("/api/v1/sessions/{}", session.as_str().unwrap()),
    )
    .await;
    let agent = machine["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|n| n["agents"].as_array().unwrap())
        .find(|a| a["attempt_id"] == json!(attempt))
        .unwrap_or_else(|| panic!("agent missing: {body}"));
    assert_eq!(agent["provider"], detail["data"]["profile"]["provider"]);
    assert_eq!(agent["model"], detail["data"]["profile"]["model"]);
    assert_eq!(body["data"]["server"]["running_agents"], 1, "{body}");
    assert_eq!(listed_agents(&body).len(), 1);
    assert_eq!(overview_running(&app).await, json!(1));
    swarm_fixture(&body);
}

#[tokio::test]
async fn swarm_stopped_attempt_holding_its_task_is_not_a_running_agent() {
    use aq_server::execution::authority::hot::{HotState, LaunchOwner};
    let now = common::profiled::NOW;
    let f = common::profiled::fixture().await;
    let attempt = common::profiled::insert(&f).await;
    let pool = f.db.pool().await;
    sqlx::query(
        "UPDATE nodes SET last_seen_at=to_timestamp($2::double precision/1000) WHERE id=$1",
    )
    .bind(f.node.node_id)
    .bind(now as i64)
    .execute(&pool)
    .await
    .unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let mut hot = HotState::from_compat(&f.profiled).unwrap();
    sqlx::query("INSERT INTO profiled_authority_hot(attempt_id,state,record_revision,authority_revision,grant_revision) VALUES($1,$2,1,1,0)")
        .bind(attempt)
        .bind(json!(hot))
        .execute(&pool)
        .await
        .unwrap();
    hot.status = Status::Finished;
    hot.owner = Some(LaunchOwner {
        guardian_id: Uuid::new_v4(),
        launch_id: Uuid::new_v4(),
        reservation_id: Uuid::new_v4(),
    });
    hot.stop = Stop::Stopped {
        receipt: common::profiled::artifact("stop", b"stop"),
    };
    sqlx::query("UPDATE profiled_authority_hot SET state=$2 WHERE attempt_id=$1")
        .bind(attempt)
        .bind(json!(hot))
        .execute(&pool)
        .await
        .unwrap();
    let held: bool =
        sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(held, "the stopped attempt must still hold its task");
    let (status, body) = read(&app, "/api/v1/swarm").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["data"]["server"]["running_agents"], 0, "{body}");
    assert!(listed_agents(&body).is_empty(), "{body}");
    assert_eq!(overview_running(&app).await, json!(0));
}

#[tokio::test]
async fn swarm_offline_node_without_report_is_its_own_offline_machine() {
    let f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    let cold = add_node(&pool, "cold", None).await;
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, body) = read(&app, "/api/v1/swarm").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let machine = body["data"]["machines"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == json!(format!("node:{cold}")))
        .unwrap();
    assert_eq!(machine["state"], "offline");
    assert!(machine["load"].is_null());
}

#[tokio::test]
async fn swarm_waiting_reason_when_no_machine_online() {
    let f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    add_node(&pool, "cold", None).await;
    sqlx::query("UPDATE nodes SET last_seen_at=NULL")
        .execute(&pool)
        .await
        .unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, body) = read(&app, "/api/v1/swarm").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        body["data"]["waiting"]["reasons"][0]["code"],
        "no_machine_online"
    );
}

#[tokio::test]
async fn bootstrap_advertises_swarm_read_for_owner() {
    let f = common::profiled::fixture().await;
    let app = aq_server::build_admin_router(f.state.clone());
    let (_, bootstrap) = read(&app, "/api/v1/bootstrap").await;
    assert!(
        bootstrap["data"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["name"] == "swarm.read" && v["version"] == "1" && v["enabled"] == true)
    );
}

#[tokio::test]
async fn service_machine_reports_control_and_service_state() {
    use aq_machine_control as mc;
    let now = common::profiled::NOW;
    let f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    sqlx::query(
        "UPDATE nodes SET last_seen_at=to_timestamp($2::double precision/1000) WHERE id=$1",
    )
    .bind(f.node.node_id)
    .bind(now as i64)
    .execute(&pool)
    .await
    .unwrap();
    let mut c = pool.acquire().await.unwrap();
    let caller = mc::claim_key(&mut c, f.node.node_id, "desk").await.unwrap();
    let hashes = ["a", "b"].map(|s| format!("{s}-{}", Uuid::new_v4()));
    let new = mc::NewSlot {
        base_name: "profiled-fixture",
        worker_version: "0.1.0",
        protocol_version: 1,
    };
    let at = chrono::Utc::now();
    let minted = mc::mint_slots(&mut c, &caller, 3, &new, &hashes, at)
        .await
        .unwrap();
    for m in &minted {
        sqlx::query(
            "UPDATE nodes SET last_seen_at=to_timestamp($2::double precision/1000) WHERE id=$1",
        )
        .bind(m.node_id)
        .bind(now as i64)
        .execute(&pool)
        .await
        .unwrap();
    }
    let owner: Uuid = sqlx::query_scalar("SELECT id FROM users LIMIT 1")
        .fetch_one(&pool)
        .await
        .unwrap();
    mc::set_desired(
        &mut c,
        "desk",
        mc::DesiredState::Draining,
        1,
        owner,
        Uuid::new_v4(),
        at,
    )
    .await
    .unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, body) = read(&app, "/api/v1/swarm").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let machines = body["data"]["machines"].as_array().unwrap();
    assert_eq!(machines.len(), 1, "{body}");
    let m = &machines[0];
    assert_eq!(m["id"], "desk");
    assert_eq!(m["control"]["desired"], "draining");
    assert_eq!(m["control"]["generation"], "2");
    let nodes = m["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 3);
    for n in nodes {
        assert_eq!(n["service"]["state"], "draining", "{body}");
        let codes: Vec<&Value> = n["holding"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| &h["code"])
            .collect();
        assert!(codes.contains(&&json!("worker_draining")), "{body}");
    }
}

#[tokio::test]
async fn holding_candidates_are_capped_and_most_recent_first() {
    let f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    let n = aq_swarm::assemble::MAX_CANDIDATES + 3;
    for i in 0..n {
        sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,state,execution_deadline_ms) VALUES($1,$2,$3,$4,$5,'{}',$6)")
            .bind(Uuid::new_v4())
            .bind(Uuid::new_v4())
            .bind(f.node.node_id)
            .bind(Uuid::new_v4())
            .bind(Uuid::new_v4())
            .bind(1_000 + i as i64)
            .execute(&pool)
            .await
            .unwrap();
    }
    let mut c = pool.acquire().await.unwrap();
    let ids = [f.node.node_id];
    let (rows, cut) = aq_swarm::assemble::holding(&mut c, &ids, aq_swarm::assemble::MAX_CANDIDATES)
        .await
        .unwrap();
    assert_eq!(rows.len(), aq_swarm::assemble::MAX_CANDIDATES);
    assert!(cut);
    let kept: Vec<i64> = sqlx::query_scalar("SELECT execution_deadline_ms FROM execution_attempts WHERE attempt_id=ANY($1) ORDER BY execution_deadline_ms")
        .bind(rows.iter().map(|r| r.0).collect::<Vec<_>>())
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(kept[0], 1_003, "the three oldest are dropped");
    let (rows, cut) = aq_swarm::assemble::holding(&mut c, &ids, n).await.unwrap();
    assert_eq!((rows.len(), cut), (n, false));
}

fn measurement_holds(body: &Value) -> Vec<String> {
    body["data"]["machines"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["nodes"].as_array().unwrap())
        .flat_map(|n| n["holding"].as_array().unwrap())
        .filter_map(|h| h["code"].as_str().filter(|c| c.starts_with("measurement")))
        .map(str::to_owned)
        .collect()
}

/// Spec 0007 §7.3: the swarm read uses renewed context facts, so a node whose
/// configured measurement expired but was renewed shows no measurement hold.
#[tokio::test]
async fn renewed_node_has_no_measurement_hold() {
    use aq_server::profiled_execution::{config::ProfiledConfig, context_budget::Qualification};
    let now = common::profiled::NOW;
    let mut f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    let a = |id: &str| json!(common::profiled::artifact(id, id.as_bytes()));
    let (runtime, qualification) = (a("swarm-runtime"), a("swarm-runtime-qualification"));
    let configured: Qualification = serde_json::from_value(json!({"schema_version":1,
        "subject":{"runtime":runtime,"runtime_qualification":qualification,"harness":"h","cli_version":"1",
            "adapter":a("adapter"),"environment":a("environment"),"harness_qualification":a("harness-q"),
            "provider":"p","model":"m","native_reasoning":"high","reasoning_configuration_reference":"r@1",
            "credential_profile":a("credential"),"credential_revision":1,"account_reference":"acct","endpoint_reference":"ep"},
        "rendering":"utf8_double_newline_v1","maximum_tokens_per_utf8_byte":1,"fixed_overhead_tokens":37,
        "counting_evidence":a("counting"),"capacity_kind":"qualified_model_window","capacity_tokens":65536,
        "capacity_evidence":a("capacity"),"observed_at_ms":now - 120_000,"expires_at_ms":now - 1}))
    .unwrap();
    let mut config = (*f.state.config).clone();
    config.profiled_execution = Some(std::sync::Arc::new(ProfiledConfig {
        revision: 1,
        qualified_runtimes: vec![],
        projects: vec![],
        context_qualifications: vec![configured.clone()],
        definition_bodies: vec![],
    }));
    f.state.config = std::sync::Arc::new(config);
    sqlx::query(
        "UPDATE nodes SET last_seen_at=to_timestamp($2::double precision/1000) WHERE id=$1",
    )
    .bind(f.node.node_id)
    .bind(now as i64)
    .execute(&pool)
    .await
    .unwrap();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL session_replication_role=replica")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO execution_nodes(node_id,state) VALUES($1,$2) ON CONFLICT(node_id) DO UPDATE SET state=execution_nodes.state||excluded.state")
        .bind(f.node.node_id)
        .bind(json!({"native":{"request":{"runtime":runtime},"qualification":qualification}}))
        .execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, body) = read(&app, "/api/v1/swarm").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(measurement_holds(&body), ["measurement_expired"], "{body}");
    let mut renewed = configured.clone();
    renewed.observed_at_ms = now - 60_000;
    renewed.expires_at_ms = now - 60_000 + 86_400_000;
    sqlx::query("INSERT INTO context_qualification_renewals(renderer_digest,base_renderer_digest,fact,observed_at_ms,expires_at_ms,node_id,submission,created_at) VALUES($1,$2,$3,$4,$5,$6,'{}',now())")
        .bind(renewed.artifact().unwrap().digest.as_str())
        .bind(configured.artifact().unwrap().digest.as_str())
        .bind(json!(renewed))
        .bind(renewed.observed_at_ms as i64)
        .bind(renewed.expires_at_ms as i64)
        .bind(f.node.node_id)
        .execute(&pool).await.unwrap();
    let (status, body) = read(&app, "/api/v1/swarm").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(measurement_holds(&body).is_empty(), "{body}");
}

#[tokio::test]
async fn swarm_shows_the_offer_rate_hold_on_the_node() {
    let now = common::profiled::NOW;
    let f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    sqlx::query(
        "UPDATE nodes SET last_seen_at=to_timestamp($2::double precision/1000) WHERE id=$1",
    )
    .bind(f.node.node_id)
    .bind(now as i64)
    .execute(&pool)
    .await
    .unwrap();
    common::sealed::offers(
        &pool,
        f.node.node_id,
        f.selection.lineage.project_id,
        now as i64 - 1_000,
        30,
    )
    .await;
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, body) = read(&app, "/api/v1/swarm").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let node = body["data"]["machines"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|m| m["nodes"].as_array().unwrap())
        .find(|n| n["node"]["id"] == json!(f.node.node_id.to_string()))
        .unwrap();
    assert!(
        node["holding"].as_array().unwrap().contains(&json!({
            "code": "history_rate_limited",
            "message": "Offer rate limit reached: 30 offers in the last hour (limit 30)",
            "remedy": "Wait for the hour to pass; if this repeats, look for a task that fails in a loop"
        })),
        "{node}"
    );
}

#[test]
fn swarm_routing_profile_is_an_additive_pinned_schema_field() {
    let generated = aq_dashboard_contract::schema_bytes();
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../protocol/dashboard-read/v1/schema.json");
    if std::env::var_os("AQ_DASHBOARD_WRITE_FIXTURES").is_some() {
        std::fs::write(&path, &generated).unwrap();
    }
    assert_eq!(std::fs::read(path).unwrap(), generated);
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../protocol/dashboard-read/v1/fixtures/swarm.json"
    ))
    .unwrap();
    let legacy: aq_dashboard_contract::Swarm =
        serde_json::from_value(fixture["data"].clone()).unwrap();
    assert!(
        legacy
            .machines
            .iter()
            .flat_map(|m| &m.nodes)
            .flat_map(|n| &n.agents)
            .all(|a| a.routing_profile.is_none())
    );
}
