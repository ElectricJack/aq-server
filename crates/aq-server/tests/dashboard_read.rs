//! Synthetic owned SQL fixtures; no provider process or live graph is claimed.
mod common;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use common::profiled::{NOW, fixture, insert};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;
async fn read(app: &Router, path: &str, email: &str) -> (StatusCode, Value) {
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
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"unexpected_body":String::from_utf8_lossy(&bytes)})),
    )
}
fn fixture_response(name: &str, value: &Value) {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../protocol/dashboard-read/v1/fixtures")
        .join(format!("{name}.json"));
    if std::env::var_os("AQ_DASHBOARD_WRITE_FIXTURES").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            path,
            format!("{}\n", serde_json::to_string_pretty(value).unwrap()),
        )
        .unwrap();
    }
}
#[tokio::test]
async fn live_subset_bootstrap_and_exact_session_scope_are_truthful() {
    let f = fixture().await;
    insert(&f).await;
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, b) = read(&app, "/api/v1/bootstrap", "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{b}");
    assert_eq!(b["api_version"], "dashboard-read-1");
    fixture_response("bootstrap", &b);
    assert!(b["data"]["product"]["dashboard_digest"].is_null());
    for c in b["data"]["capabilities"].as_array().unwrap() {
        assert_eq!(c["version"], "1");
    }
    let (status, list) = read(&app, "/api/v1/sessions?limit=1", "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(list["data"]["items"].as_array().unwrap().len(), 1);
    fixture_response("sessions", &list);
    let row = &list["data"]["items"][0];
    assert_eq!(row["observation"]["state"], "unknown");
    assert_eq!(list["data"]["total"], json!({"value":1,"accuracy":"exact"}));
    let id = row["id"].as_str().unwrap();
    let (status, detail) = read(&app, &format!("/api/v1/sessions/{id}"), "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["data"]["id"], id);
    fixture_response("session", &detail);
    assert_eq!(row["last_output_at"], detail["data"]["last_activity_at"]);
    // No launch binding: inspect only, no session message slot.
    assert_eq!(detail["data"]["permissions"], json!(["session.inspect"]));
    assert_eq!(
        read(
            &app,
            &format!("/api/v1/sessions/{id}?scope=project:{}", Uuid::new_v4()),
            "owner@localhost"
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let serialized = serde_json::to_string(&detail).unwrap();
    for secret in [
        "credential_hash",
        "secret_lease",
        "graph_service_token",
        "launch_manifest",
    ] {
        assert!(!serialized.contains(secret));
    }
    let (status, p) = read(&app, "/api/v1/projects", "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{p}");
    fixture_response("projects", &p);
    assert_eq!(p["data"]["items"].as_array().unwrap().len(), 1);
    let (_, o) = read(&app, "/api/v1/overview", "owner@localhost").await;
    assert_eq!(o["data"]["counts"]["running"]["value"], 0);
    assert_eq!(o["data"]["counts"]["running"]["accuracy"], "exact");
    fixture_response("overview", &o);
    assert_eq!(
        read(&app, "/api/v1/sessions?limit=101", "owner@localhost")
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}
#[tokio::test]
async fn viewer_requires_current_exact_acl_and_never_gets_project_catalog() {
    let f = fixture().await;
    insert(&f).await;
    let pool = f.db.pool().await;
    let id = Uuid::new_v4();
    let email = "viewer@localhost";
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,$2,'viewer','dev',$2,now())").bind(id).bind(email).execute(&pool).await.unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let session = match f.profiled.offer.binding.session {
        aq_session_protocol::SessionIdentity::Remote { session_id, .. } => session_id,
        _ => unreachable!(),
    };
    assert_eq!(
        read(&app, &format!("/api/v1/sessions/{session}"), email)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let key = aq_server::session_access::session_key(&f.profiled.offer.binding.session).unwrap();
    sqlx::query("INSERT INTO session_inspection_acl(session_key,session_identity,principal_id,revision,can_inspect,expires_at_ms) VALUES($1,$2,$3,1,true,$4)").bind(&key).bind(json!(f.profiled.offer.binding.session)).bind(id).bind((NOW+1000)as i64).execute(&pool).await.unwrap();
    let (status, body) = read(&app, &format!("/api/v1/sessions/{session}"), email).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        read(&app, "/api/v1/projects", email).await.0,
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE session_inspection_acl SET can_inspect=false,revision=2 WHERE session_key=$1 AND principal_id=$2").bind(&key).bind(id).execute(&pool).await.unwrap();
    assert_eq!(
        read(&app, &format!("/api/v1/sessions/{session}"), email)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
}
#[tokio::test]
async fn malformed_inputs_have_dashboard_error_envelopes() {
    let f = fixture().await;
    let app = aq_server::build_admin_router(f.state.clone());
    for path in [
        "/api/v1/sessions/not-a-uuid",
        "/api/v1/sessions?limit=wrong",
        "/api/v1/sessions?unexpected=1",
        "/api/v1/sessions?cursor=bad",
    ] {
        let (status, body) = read(&app, path, "owner@localhost").await;
        assert!(
            status == StatusCode::UNPROCESSABLE_ENTITY || status == StatusCode::GONE,
            "{body}"
        );
        assert!(body["code"].is_string(), "{body}");
        assert!(body["request_id"].is_string());
    }
    let (status, body) = read(&app, "/api/v1/bootstrap", "unknown@localhost").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert!(body["code"].is_string(), "{body}");
}
#[tokio::test]
async fn acl_expiry_during_database_wait_refuses_read() {
    use std::sync::atomic::Ordering;
    let f = fixture().await;
    insert(&f).await;
    let pool = f.db.pool().await;
    let actor = Uuid::new_v4();
    let email = "expiry@localhost";
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,$2,'viewer','dev',$2,now())").bind(actor).bind(email).execute(&pool).await.unwrap();
    let key = aq_server::session_access::session_key(&f.profiled.offer.binding.session).unwrap();
    sqlx::query("INSERT INTO session_inspection_acl(session_key,session_identity,principal_id,revision,can_inspect,expires_at_ms) VALUES($1,$2,$3,1,true,$4)").bind(key).bind(json!(f.profiled.offer.binding.session)).bind(actor).bind((NOW+100)as i64).execute(&pool).await.unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let mut lock = pool.begin().await.unwrap();
    sqlx::query("LOCK TABLE session_inspection_acl IN ACCESS EXCLUSIVE MODE")
        .execute(&mut *lock)
        .await
        .unwrap();
    let request = tokio::spawn(async move { read(&app, "/api/v1/bootstrap", email).await });
    // Advance the fake clock only after auth sampled the pre-expiry time and
    // its ACL query is actually blocked. A fixed delay races pool startup.
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            let waiting: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND application_name='aq-dashboard-read' AND wait_event_type='Lock' AND query LIKE 'SELECT session_key,session_identity,revision,expires_at_ms FROM session_inspection_acl%')")
                .fetch_one(&pool).await.unwrap();
            if waiting {
                break;
            }
            assert!(!request.is_finished(), "request completed before ACL lock wait");
            tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        }
    }).await.expect("dashboard auth must wait for the held ACL lock");
    assert!(!request.is_finished());
    f.time.store((NOW + 100) as i64, Ordering::SeqCst);
    lock.commit().await.unwrap();
    let (status, body) = request.await.unwrap();
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "authorization_changed");
}
#[tokio::test]
async fn overflow_visibility_refuses_detail_and_wrong_boot_acl_does_not_list_session() {
    let f = fixture().await;
    insert(&f).await;
    let pool = f.db.pool().await;
    let actor = Uuid::new_v4();
    let email = "bounded@localhost";
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,$2,'viewer','dev',$2,now())").bind(actor).bind(email).execute(&pool).await.unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let mut wrong = f.profiled.offer.binding.session.clone();
    if let aq_session_protocol::SessionIdentity::Remote { boot_id, .. } = &mut wrong {
        *boot_id = Uuid::new_v4();
    }
    let key = aq_server::session_access::session_key(&wrong).unwrap();
    sqlx::query("INSERT INTO session_inspection_acl(session_key,session_identity,principal_id,revision,can_inspect,expires_at_ms) VALUES($1,$2,$3,1,true,$4)").bind(key).bind(json!(wrong)).bind(actor).bind((NOW+10000)as i64).execute(&pool).await.unwrap();
    let (status, list) = read(&app, "/api/v1/sessions", email).await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert!(list["data"]["items"].as_array().unwrap().is_empty());
    let mut tx = pool.begin().await.unwrap();
    for _ in 0..256 {
        let mut identity = f.profiled.offer.binding.session.clone();
        if let aq_session_protocol::SessionIdentity::Remote { session_id, .. } = &mut identity {
            *session_id = Uuid::new_v4();
        }
        let key = aq_server::session_access::session_key(&identity).unwrap();
        sqlx::query("INSERT INTO session_inspection_acl(session_key,session_identity,principal_id,revision,can_inspect,expires_at_ms) VALUES($1,$2,$3,1,true,$4)").bind(key).bind(json!(identity)).bind(actor).bind((NOW+10000)as i64).execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    let (_, bootstrap) = read(&app, "/api/v1/bootstrap", email).await;
    assert!(
        bootstrap["data"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["name"] == "sessions.read" && c["enabled"] == false)
    );
    let id = match f.profiled.offer.binding.session {
        aq_session_protocol::SessionIdentity::Remote { session_id, .. } => session_id,
        _ => unreachable!(),
    };
    assert_eq!(
        read(&app, &format!("/api/v1/sessions/{id}"), email).await.0,
        StatusCode::FORBIDDEN
    );
}
#[tokio::test]
async fn session_lifecycle_reports_are_historical_acl_scoped_and_never_live_observation() {
    use aq_server::profiled_execution::{started, stopped};
    use std::sync::atomic::Ordering;
    // Actual receipt owners over synthetic host observations; no physical/kernel
    // or provider claim follows from this read-projection fixture.
    let (f, q) = common::native_started::setup(true).await;
    let auth = common::native_started::context(&f).await;
    let aq_session_protocol::SessionIdentity::Remote { session_id, .. } = q.binding.session else {
        unreachable!()
    };
    let path = format!("/api/v1/sessions/{session_id}");
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, before) = read(&app, &path, "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{before}");
    assert_eq!(
        before["data"]["lifecycle"],
        json!({"started_at":null,"stopped_at":null})
    );
    assert_eq!(before["data"]["observation"]["state"], "unknown");
    let start = started::record(&f.state, &auth, &q).await.unwrap();
    let (status, running) = read(&app, &path, "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{running}");
    assert!(running["data"]["lifecycle"]["started_at"].is_string());
    assert!(running["data"]["lifecycle"]["stopped_at"].is_null());
    assert_eq!(running["data"]["execution_authority"], "granted");
    assert_eq!(running["data"]["observation"]["state"], "unknown");
    f.time.store((NOW + 2000) as i64, Ordering::SeqCst);
    let stop = stopped::record(&f.state, &auth, &common::native_started::stop_request(&q))
        .await
        .unwrap();
    let (status, after) = read(&app, &path, "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{after}");
    assert_eq!(after["data"]["execution_authority"], "finished");
    assert_eq!(after["data"]["observation"]["state"], "unknown");
    assert!(after["data"]["terminal"].is_null());
    for (key, ms) in [
        ("started_at", start.recorded_at_ms),
        ("stopped_at", stop.recorded_at_ms),
    ] {
        assert_eq!(
            after["data"]["lifecycle"][key],
            chrono::DateTime::from_timestamp_millis(ms as i64)
                .unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
        );
    }
    let (status, list) = read(&app, "/api/v1/sessions?limit=1", "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{list}");
    assert_eq!(
        list["data"]["items"][0]["lifecycle"],
        after["data"]["lifecycle"]
    );
    // Lightweight warmed sample, not a benchmark/scale acceptance claim.
    let mut warm_ms = Vec::new();
    for _ in 0..5 {
        let t = std::time::Instant::now();
        let (status, _) = read(&app, &path, "owner@localhost").await;
        assert_eq!(status, StatusCode::OK);
        warm_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let query = "SELECT e.state->'offer' AS offer,s.request_bytes AS started_request,s.request_digest AS started_digest,s.receipt_bytes AS started_receipt,z.request_bytes AS stopped_request,z.request_digest AS stopped_digest,z.receipt_bytes AS stopped_receipt FROM execution_attempts e LEFT JOIN profiled_started_receipts s USING(attempt_id) LEFT JOIN profiled_stopped_receipts z USING(attempt_id) WHERE e.attempt_id=$1";
    assert!(include_str!("../src/dashboard_api/service.rs").contains(query));
    let plan: Value = sqlx::query_scalar(&format!("EXPLAIN (ANALYZE,BUFFERS,FORMAT JSON) {query}"))
        .bind(q.binding.attempt_id)
        .fetch_one(&f.db.pool().await)
        .await
        .unwrap();
    eprintln!(
        "dashboard lifecycle tiny fixture sample: {}",
        json!({"sessions":1,"warmed_detail_ms":warm_ms,"new_projection_roundtrips":0,"existing_list_still_per_session":true,"plan":plan})
    );
    let serialized = serde_json::to_string(&after).unwrap();
    for excluded in [
        "request_bytes",
        "proof_bytes",
        "kernel_wait_echild",
        "guardian",
        "credential_hash",
    ] {
        assert!(!serialized.contains(excluded));
    }
    let pool = f.db.pool().await;
    let actor = Uuid::new_v4();
    let email = "lifecycle-viewer@localhost";
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,$2,'viewer','dev',$2,now())").bind(actor).bind(email).execute(&pool).await.unwrap();
    assert_eq!(read(&app, &path, email).await.0, StatusCode::FORBIDDEN);
    let key = aq_server::session_access::session_key(&q.binding.session).unwrap();
    sqlx::query("INSERT INTO session_inspection_acl(session_key,session_identity,principal_id,revision,can_inspect,expires_at_ms) VALUES($1,$2,$3,1,true,$4)").bind(&key).bind(json!(q.binding.session)).bind(actor).bind((NOW+3000)as i64).execute(&pool).await.unwrap();
    let (status, visible) = read(&app, &path, email).await;
    assert_eq!(status, StatusCode::OK, "{visible}");
    assert_eq!(visible["data"]["lifecycle"], after["data"]["lifecycle"]);
    f.time.store((NOW + 3000) as i64, Ordering::SeqCst);
    assert_eq!(read(&app, &path, email).await.0, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn search_validates_queries_and_returns_all_groups() {
    let f = fixture().await;
    insert(&f).await;
    let app = aq_server::build_admin_router(f.state.clone());
    let (status, body) = read(&app, "/api/v1/search?q=Remote&limit=1", "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let groups = body["data"]["groups"].as_array().unwrap();
    assert_eq!(groups.len(), 5);
    assert!(
        groups
            .iter()
            .all(|g| g["hits"].is_array() && g["has_more"].is_boolean())
    );
    let sessions = groups.iter().find(|g| g["type"] == "session").unwrap();
    assert_eq!(sessions["hits"].as_array().unwrap().len(), 1);
    assert!(!body.to_string().contains("private-search-canary"));
    for suffix in [
        "q=",
        "q=%0A",
        "q=Remote&limit=0",
        "q=Remote&limit=101",
        "q=Remote&types=invalid",
    ] {
        assert_eq!(
            read(&app, &format!("/api/v1/search?{suffix}"), "owner@localhost")
                .await
                .0,
            StatusCode::UNPROCESSABLE_ENTITY
        );
    }
    assert_eq!(
        read(
            &app,
            &format!("/api/v1/search?q={}", "a".repeat(257)),
            "owner@localhost"
        )
        .await
        .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}
