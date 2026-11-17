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
async fn swarm_stopped_attempt_holding_its_task_is_listed_as_finishing_not_running() {
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
    let listed = listed_agents(&body);
    assert_eq!(listed.len(), 1, "{body}");
    assert_eq!(listed[0]["attempt_id"], json!(attempt));
    assert_eq!(listed[0]["phase"], "finishing", "{body}");
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
async fn slot_holders_are_capped_and_most_recent_first() {
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
    let query = |limit| aq_swarm::agents::Query {
        nodes: &ids,
        project: None,
        runtime: None,
        summaries: true,
        now_ms: common::profiled::NOW,
        limit,
    };
    let max = aq_swarm::assemble::MAX_CANDIDATES;
    let held = aq_swarm::agents::load(&mut c, query(max)).await.unwrap();
    assert_eq!(held.agents.len(), max);
    assert!(held.cut);
    let kept: Vec<i64> = sqlx::query_scalar("SELECT execution_deadline_ms FROM execution_attempts WHERE attempt_id=ANY($1) ORDER BY execution_deadline_ms")
        .bind(held.agents.iter().map(|(_, a)| a.attempt_id).collect::<Vec<_>>())
        .fetch_all(&pool)
        .await
        .unwrap();
    assert_eq!(kept[0], 1_003, "the three oldest are dropped");
    let held = aq_swarm::agents::load(&mut c, query(n)).await.unwrap();
    assert_eq!((held.agents.len(), held.cut), (n, false));
}

/// The offer and worker report rate limits were removed (owner, 2026-11-13): a node and
/// its project past the old offer limits, and a node past the old 720 reports an hour,
/// show no hold, and nothing reads `history_rate_limited`.
#[tokio::test]
async fn swarm_shows_no_rate_hold_past_the_old_offer_and_report_limits() {
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
    let project = f.selection.lineage.project_id;
    common::sealed::offers(&pool, f.node.node_id, project, now as i64 - 1_000, 31).await;
    common::sealed::spread_offers(&pool, Some(project), 210, now as i64 - 1_000).await;
    common::sealed::inventories(&pool, f.node.node_id, 721, now as i64 - 60_000).await;
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
        !node["holding"]
            .as_array()
            .unwrap()
            .iter()
            .any(|h| h["code"].as_str().unwrap().starts_with("history_")),
        "{node}"
    );
    let text = body.to_string();
    assert!(!text.contains("history_rate_limited"), "{text}");
    assert!(!text.contains("rate limit reached"), "{text}");
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

#[test]
fn internal_work_attention_is_an_additive_pinned_schema_kind() {
    let schema: Value = serde_json::from_slice(&aq_dashboard_contract::schema_bytes()).unwrap();
    let kinds = &schema["$defs"]["AttentionKind"]["enum"];
    assert_eq!(kinds.as_array().unwrap().last().unwrap(), "internal_work");
    // Overviews recorded before the kind existed still read.
    for fixture in [
        include_str!("../../../../protocol/dashboard-read/v1/fixtures/overview-nonempty.json"),
        include_str!("../../../../protocol/dashboard-read/v1/fixtures/overview.json"),
    ] {
        let fixture: Value = serde_json::from_str(fixture).unwrap();
        let _: aq_dashboard_contract::Overview =
            serde_json::from_value(fixture["data"].clone()).unwrap();
    }
}

/// Spec 0014 §8: placement needs no measurement, so the swarm read shows no
/// measurement hold: not for a node with a registered native runtime that
/// nothing ever measured.
#[tokio::test]
async fn a_node_nothing_measured_has_no_measurement_hold() {
    let now = common::profiled::NOW;
    let f = common::profiled::fixture().await;
    let pool = f.db.pool().await;
    let a = |id: &str| json!(common::profiled::artifact(id, id.as_bytes()));
    let (runtime, qualification) = (a("swarm-runtime"), a("swarm-runtime-qualification"));
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
    let nodes: Vec<&Value> = (body["data"]["machines"].as_array().unwrap().iter())
        .flat_map(|m| m["nodes"].as_array().unwrap())
        .collect();
    assert!(!nodes.is_empty(), "{body}");
    let measurement: Vec<&str> = (nodes.iter())
        .flat_map(|n| n["holding"].as_array().unwrap())
        .filter_map(|h| h["code"].as_str().filter(|c| c.starts_with("measurement")))
        .collect();
    assert!(measurement.is_empty(), "{body}");
}

/// An attempt row as admission leaves it, without the launch records around it:
/// the swarm read needs only its status, stop state, grant and manifest names.
struct Holder {
    node: Uuid,
    project: Uuid,
    status: &'static str,
    stop: &'static str,
    held: bool,
}
async fn hold(c: &mut sqlx::PgConnection, h: Holder) -> (Uuid, Uuid) {
    let now = common::profiled::NOW;
    let (attempt, task) = (Uuid::new_v4(), Uuid::new_v4());
    let grant = (h.status == "granted").then(|| json!({"expires_at_ms": now + 5_000}));
    let state = json!({"schema_version":1,"status":h.status,"stop":{"kind":h.stop},"revoked":false,
        "cancellation_id":null,"grant":grant,
        "offer":{"manifest":{"model":{"provider":"anthropic","model":"claude-sonnet-5"},
            "harness":{"harness":"claude-code"},"bindings":{"project_id":h.project,"task_revision":3}}}});
    sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,kind,state,holds_task,execution_deadline_ms) VALUES($1,$2,$3,$4,$5,'profiled_v1',$6,$7,$8)")
        .bind(attempt).bind(task).bind(h.node).bind(Uuid::new_v4()).bind(Uuid::new_v4())
        .bind(state).bind(h.held).bind(grant.is_some().then_some((now + 60_000) as i64))
        .execute(&mut *c).await.unwrap();
    (attempt, task)
}
/// The worker's started receipt, as the receipt table stores it.
async fn started(c: &mut sqlx::PgConnection, attempt: Uuid, node: Uuid, at_ms: u64) {
    let body = serde_json::to_vec(&json!({"recorded_at_ms": at_ms})).unwrap();
    sqlx::query("INSERT INTO profiled_started_receipts(attempt_id,node_id,operation_id,request_bytes,request_digest,receipt_bytes,before_record_revision) VALUES($1,$2,$3,$4,encode(sha256($4),'hex'),$4,1)")
        .bind(attempt).bind(node).bind(Uuid::new_v4()).bind(body)
        .execute(&mut *c).await.unwrap();
}
fn summary(role: &str) -> aq_role_settings::attempts::RanWith {
    aq_role_settings::attempts::RanWith {
        role: role.into(),
        level: "standard-high".into(),
        provider: "anthropic".into(),
        model: "claude-sonnet-5".into(),
        reasoning: "high".into(),
        harness: "claude-code".into(),
        execution_profile: "sandbox".into(),
        snapshot_digest: None,
    }
}

/// Every attempt that holds a slot is listed under its own node with its
/// phase, title, times and `ran_with`; the rows of a node equal its occupied
/// slots; a settled attempt is not listed.
#[tokio::test]
async fn swarm_lists_every_slot_holding_attempt_with_its_phase() {
    let now = common::profiled::NOW;
    let mut f = common::profiled::fixture().await;
    make_running(&mut f);
    // One real launch: offered nine seconds ago, started by the fixture's grant.
    let real = common::profiled::insert_offered(&f, now - 9_000).await;
    let pool = f.db.pool().await;
    let (first, project) = (f.node.node_id, f.selection.lineage.project_id);
    let second = add_node(&pool, "second-machine", Some(now)).await;
    sqlx::query(
        "UPDATE nodes SET last_seen_at=to_timestamp($2::double precision/1000) WHERE id=$1",
    )
    .bind(first)
    .bind(now as i64)
    .execute(&pool)
    .await
    .unwrap();
    let mut report = windows_report();
    report.machine.key = Some("second-box".into());
    report.machine.label = Some("second-box".into());
    aq_swarm::machine::record(&pool, second, &report, now)
        .await
        .unwrap();
    // The receipt and terminal tables hang off launch records these rows do not have.
    let mut c = pool.acquire().await.unwrap().detach();
    sqlx::query("SET session_replication_role=replica")
        .execute(&mut c)
        .await
        .unwrap();
    let row = |node, status, stop, held| Holder {
        node,
        project,
        status,
        stop,
        held,
    };
    let (preparing, preparing_task) = hold(&mut c, row(first, "preparing", "unknown", true)).await;
    let (granted, _) = hold(&mut c, row(first, "granted", "unknown", true)).await;
    let (running, running_task) = hold(&mut c, row(first, "granted", "running", true)).await;
    let (stopped, _) = hold(&mut c, row(first, "finished", "stopped", true)).await;
    let (elsewhere, _) = hold(&mut c, row(second, "granted", "running", true)).await;
    let (settled, _) = hold(&mut c, row(first, "finished", "stopped", false)).await;
    for (attempt, node, at) in [
        (running, first, now - 4_000),
        (stopped, first, now - 8_000),
        (elsewhere, second, now - 2_000),
    ] {
        started(&mut c, attempt, node, at).await;
    }
    let runtime = Uuid::new_v4();
    sqlx::query("INSERT INTO terminal_sessions(terminal_id,attempt_id,node_id,boot_id,session_id,assignment_generation,terminal_epoch,identity,authority_revision,execution_spec_digest,launch_manifest_digest,input_mode,ledger_revision,runtime_id,protection_version,key_id,nonce,ciphertext,controller_generation,pending_bytes,operation_count,observed_at_ms,last_output_at_ms) SELECT $1,attempt_id,node_id,$1,session_id,1,2,'{}',1,repeat('a',64),repeat('a',64),'finite_v1',0,$3,1,'k',decode(repeat('00',12),'hex'),decode(repeat('00',16),'hex'),0,0,0,$4,$5 FROM execution_attempts WHERE attempt_id=$2")
        .bind(Uuid::new_v4()).bind(running).bind(runtime).bind(now as i64).bind((now - 1_000) as i64)
        .execute(&mut c).await.unwrap();
    for (attempt, role) in [(real, "developer"), (running, "reviewer")] {
        aq_role_settings::attempts::record(&mut c, attempt, &summary(role))
            .await
            .unwrap();
    }
    // Titles come from the graph; this fixture has none, so two are already known.
    aq_swarm::agents::remember(running_task, Some("Fix the flaky checkout test".into()));
    aq_swarm::agents::remember(preparing_task, Some("Add retry backoff".into()));

    let app = aq_server::build_admin_router(f.state.clone());
    let (status, body) = read(&app, "/api/v1/swarm").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let nodes: Vec<&Value> = (body["data"]["machines"].as_array().unwrap().iter())
        .flat_map(|m| m["nodes"].as_array().unwrap())
        .collect();
    let on = |node: Uuid| {
        *nodes
            .iter()
            .find(|n| n["node"]["id"] == json!(node))
            .unwrap()
    };
    let find = |node: Uuid, attempt: Uuid| {
        (on(node)["agents"].as_array().unwrap().iter())
            .find(|a| a["attempt_id"] == json!(attempt))
            .unwrap_or_else(|| panic!("{attempt} missing: {body}"))
    };
    assert_eq!(on(first)["slots"]["occupied"], 5, "{body}");
    assert_eq!(on(first)["agents"].as_array().unwrap().len(), 5, "{body}");
    assert_eq!(on(second)["slots"]["occupied"], 1);
    assert_eq!(on(second)["agents"].as_array().unwrap().len(), 1);
    assert!(
        listed_agents(&body)
            .iter()
            .all(|a| a["attempt_id"] != json!(settled))
    );
    let phases = [
        (preparing, "starting"),
        (granted, "starting"),
        (running, "running"),
        (real, "running"),
        (stopped, "finishing"),
    ];
    for (attempt, phase) in phases {
        assert_eq!(find(first, attempt)["phase"], phase, "{attempt}");
    }
    assert_eq!(find(second, elsewhere)["phase"], "running");
    assert_eq!(body["data"]["server"]["running_agents"], 3, "{body}");
    let at = |ms: u64| json!(aq_swarm::timestamp(ms).unwrap());
    // Titles, falling back to the task id; project id and name; ids.
    let a = find(first, running);
    assert_eq!(a["task"]["title"], "Fix the flaky checkout test");
    assert_eq!(a["task"]["id"], json!(running_task));
    assert_eq!(
        a["project"],
        json!({"id": project, "label": "Synthetic authority fixture"})
    );
    assert_eq!(find(first, preparing)["task"]["title"], "Add retry backoff");
    let untitled = find(first, granted);
    assert_eq!(untitled["task"]["title"], untitled["task"]["id"]);
    // Since-times: the agent's start once it started, else when the slot was taken.
    assert_eq!(a["started_at"], at(now - 4_000));
    assert_eq!(find(first, stopped)["started_at"], at(now - 8_000));
    assert_eq!(find(second, elsewhere)["started_at"], at(now - 2_000));
    let launched = find(first, real);
    assert_eq!(launched["held_since"], at(now - 9_000));
    assert_eq!(
        launched["started_at"],
        at(now - 9_000),
        "no start receipt yet"
    );
    assert!(find(first, preparing)["started_at"].is_null());
    // `ran_with` where the attempt has a summary, null where it has none.
    assert_eq!(
        a["ran_with"],
        serde_json::to_value(summary("reviewer")).unwrap()
    );
    assert_eq!(launched["ran_with"]["role"], "developer");
    for attempt in [preparing, granted, stopped] {
        let row = find(first, attempt);
        assert!(
            row["ran_with"].is_null() && row.get("ran_with").is_some(),
            "{row}"
        );
    }
    // This server has no terminal runtime, so it shows no terminal or output time.
    assert!(a["last_output_at"].is_null() && a["terminal"].is_null());

    // The batch read itself: one call for both nodes, whatever the attempt count,
    // and the output time of a terminal this runtime registered.
    let ids = [first, second];
    let query = aq_swarm::agents::Query {
        nodes: &ids,
        project: None,
        runtime: Some(runtime),
        summaries: true,
        now_ms: now,
        limit: aq_swarm::assemble::MAX_CANDIDATES,
    };
    let held = aq_swarm::agents::load(&mut c, query).await.unwrap();
    assert_eq!((held.agents.len(), held.running, held.cut), (6, 3, false));
    let (node, row) = (held.agents.iter())
        .find(|(_, a)| a.attempt_id == running)
        .unwrap();
    assert_eq!(*node, first);
    assert_eq!(json!(row.last_output_at), at(now - 1_000));
    assert_eq!(json!(row.terminal.as_ref().unwrap().revision), json!("2"));
    assert!(
        held.agents
            .iter()
            .filter(|(_, a)| a.attempt_id != running)
            .all(|(_, a)| a.last_output_at.is_none())
    );
    // A project scope lists only that project's attempts.
    let other = aq_swarm::agents::Query {
        project: Some(Uuid::new_v4()),
        runtime: None,
        nodes: &ids,
        summaries: true,
        now_ms: now,
        limit: 8,
    };
    assert!(
        aq_swarm::agents::load(&mut c, other)
            .await
            .unwrap()
            .agents
            .is_empty()
    );
}
