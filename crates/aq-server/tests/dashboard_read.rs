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
    assert!(list["data"]["total"]["value"].is_null());
    let id = row["id"].as_str().unwrap();
    let (status, detail) = read(&app, &format!("/api/v1/sessions/{id}"), "owner@localhost").await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["data"]["id"], id);
    fixture_response("session", &detail);
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
    assert!(o["data"]["counts"]["running"]["value"].is_null());
    fixture_response("overview", &o);
    assert_eq!(
        read(&app, "/api/v1/sessions?limit=101", "owner@localhost")
            .await
            .0,
        StatusCode::BAD_REQUEST
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
            status == StatusCode::BAD_REQUEST || status == StatusCode::CONFLICT,
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
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
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
