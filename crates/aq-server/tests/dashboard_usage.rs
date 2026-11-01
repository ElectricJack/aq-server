//! Mounted dashboard usage capability and exact authorized persisted snapshots.
//! Synthetic SQL projections qualify inspection, never a provider invoice.
mod common;
use aq_accounting::projection::Projection;
use aq_server::dashboard_api::contract::SessionDetail;
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use common::profiled::{NOW, fixture, insert};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use tower::ServiceExt;
use uuid::Uuid;
async fn read(app: &Router, path: &str, email: &str) -> (StatusCode, Value) {
    let res = app
        .clone()
        .oneshot(
            Request::get(path)
                .header("x-aq-dev-email", email)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let code = res.status();
    if code.is_success() && path.ends_with("/usage") {
        assert_eq!(res.headers()["cache-control"], "no-store");
    }
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (code, serde_json::from_slice(&bytes).unwrap())
}
fn cap(body: &Value) -> bool {
    let v = body["data"]["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "session.usage.read")
        .unwrap();
    assert_eq!(v["version"], "1");
    v["enabled"].as_bool().unwrap()
}
async fn bootstrap(app: &Router, email: &str) -> bool {
    let (code, body) = read(app, "/api/v1/bootstrap", email).await;
    assert_eq!(code, StatusCode::OK, "{body}");
    cap(&body)
}
fn usage(body: &Value) -> aq_observation_api::SessionUsage {
    aq_observation_api::decode_usage(&serde_json::to_vec(body).unwrap()).unwrap()
}
async fn legacy(
    pool: &sqlx::PgPool,
    node: &aq_worker_protocol::NodeIdentity,
    project: Uuid,
) -> (Uuid, Uuid) {
    let attempt = Uuid::new_v4();
    let session = Uuid::new_v4();
    let task = Uuid::new_v4();
    let offer = Uuid::new_v4();
    let body = json!({"task_id":task,"offer":{"offer_id":offer,"identity":node,"attempt":{"attempt_id":attempt,"session_id":session,"generation":1},"spec":{"project_id":project,"harness":"deterministic-agent-v1","resolved_spec_digest":"legacy-inspection-fixture","fixture_output":"done","resources":{"slots":1,"cpu_millis":100,"memory_bytes":1024,"disk_bytes":1024},"required_capabilities":["deterministic-agent-v1"],"bindings":null},"offer_expires_at_ms":NOW+30000,"reservation_ttl_ms":10000},"status":"offered","grant":null,"grant_expires_at_ms":NOW+30000,"last_observation":null,"report":null,"disposition":null,"cancellation_id":null,"cancellation_reason":null,"stopped":false});
    let _: aq_server::execution::AttemptState = serde_json::from_value(body.clone()).unwrap();
    sqlx::query("INSERT INTO execution_attempts(attempt_id,task_id,node_id,offer_id,session_id,kind,state) VALUES($1,$2,$3,$4,$5,'legacy_fixture',$6)").bind(attempt).bind(task).bind(node.node_id).bind(offer).bind(session).bind(body).execute(pool).await.unwrap();
    // Keep explicit fixture provenance: this row only exercises historical reads.
    (attempt, session)
}
#[tokio::test]
async fn mounted_owner_and_exact_viewer_usage_preserve_unknowns_and_revocation() {
    let f = fixture().await;
    insert(&f).await;
    let pool = f.db.pool().await;
    let expected = aq_admission::transport::lifecycle_binding(&f.profiled.offer).unwrap();
    let attempt = expected.attempt_id;
    let aq_session_protocol::SessionIdentity::Remote { session_id, .. } = expected.session else {
        panic!("remote fixture")
    };
    let detail_path = format!("/api/v1/sessions/{session_id}");
    let usage_path = format!("/api/observations/sessions/{attempt}/usage");
    let app = aq_server::build_admin_router(f.state.clone());
    assert!(bootstrap(&app, "owner@localhost").await);
    let standalone = aq_server::dashboard_api::routes(f.state.clone()).with_state(f.state.clone());
    assert!(
        !bootstrap(&standalone, "owner@localhost").await,
        "configuration cannot imply an observation route"
    );
    let (code, detail) = read(&app, &detail_path, "owner@localhost").await;
    assert_eq!(code, StatusCode::OK, "{detail}");
    let typed: SessionDetail = serde_json::from_value(detail["data"].clone()).unwrap();
    assert_eq!(typed.observation_binding.as_ref(), Some(&expected));
    assert_eq!(
        detail["data"]["session_identity"]["assignment_generation"],
        "1"
    );
    assert_eq!(
        detail["data"]["observation_binding"]["session"]["assignment_generation"],
        1
    );
    let (code, empty) = read(&app, &usage_path, "owner@localhost").await;
    assert_eq!(code, StatusCode::OK, "{empty}");
    let empty = usage(&empty);
    assert_eq!(empty.binding, expected);
    assert_eq!(empty.revision, 0);
    assert_eq!(empty.last_recorded_at_ms, None);
    assert_eq!(empty.usage.ledger_revision.version.as_str(), "0");
    assert_eq!(empty.known_category_subtotals, [0; 5]);
    assert_eq!(empty.canonical_calls, 0);
    let not_reported = aq_lifecycle_api::Counter::Unknown {
        reason: aq_lifecycle_api::worker_lifecycle::UnknownReason::NotReported,
    };
    assert_eq!(empty.usage.counters.input, not_reported);
    assert_eq!(empty.usage.counters.output, not_reported);
    assert_eq!(empty.usage.counters.cache_read, not_reported);
    assert_eq!(empty.usage.counters.cache_write, not_reported);
    assert_eq!(empty.usage.counters.reasoning, not_reported);
    assert_eq!(
        empty.usage.costs,
        vec![aq_lifecycle_api::Cost::Unknown {
            reason: aq_lifecycle_api::worker_lifecycle::UnknownReason::NotReported
        }]
    );
    assert!(matches!(
        empty.usage.completeness,
        aq_lifecycle_api::AccountingCompleteness::Unknown
    ));
    // Persist a bounded incomplete projection, keeping categories separate.
    let projection = Projection {
        canonical_calls: 2,
        known_subtotals: [120, 9, 80, 0, 3],
        unknown_contributions: [0; 5],
        incomplete_calls: 1,
        unresolved_events: 1,
    };
    projection.validate().unwrap();
    sqlx::query("INSERT INTO observation_attempt_projections(attempt_id,revision,last_recorded_at_ms,projection) VALUES($1,3,$2,$3)").bind(attempt).bind(NOW as i64).bind(json!(projection)).execute(&pool).await.unwrap();
    let (code, persisted) = read(&app, &usage_path, "owner@localhost").await;
    assert_eq!(code, StatusCode::OK, "{persisted}");
    let persisted = usage(&persisted);
    assert_eq!(persisted.binding, expected);
    assert_eq!(persisted.revision, 3);
    assert_eq!(persisted.canonical_calls, 2);
    assert_eq!(persisted.known_category_subtotals, [120, 9, 80, 0, 3]);
    assert_eq!(persisted.incomplete_calls, 1);
    assert_eq!(persisted.unresolved_events, 1);
    assert_eq!(persisted.last_recorded_at_ms, Some(NOW));
    assert!(matches!(
        persisted.usage.counters.input,
        aq_lifecycle_api::Counter::Unknown {
            reason: aq_lifecycle_api::worker_lifecycle::UnknownReason::Incomplete
        }
    ));
    assert_eq!(persisted.usage.costs, empty.usage.costs);
    optional_browser(&f, &expected, session_id).await;
    let retained: Value = sqlx::query_scalar(
        "SELECT projection FROM observation_attempt_projections WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        retained,
        json!(projection),
        "inspection cannot change accounting facts"
    );
    let viewer = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'usage-viewer@localhost','viewer','dev','usage-viewer@localhost',now())").bind(viewer).execute(&pool).await.unwrap();
    // A capability advertises the interface, never blanket session permission.
    assert!(bootstrap(&app, "usage-viewer@localhost").await);
    assert_eq!(
        read(&app, &usage_path, "usage-viewer@localhost").await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        read(&app, &detail_path, "usage-viewer@localhost").await.0,
        StatusCode::FORBIDDEN
    );
    let key = aq_server::session_access::session_key(&expected.session).unwrap();
    sqlx::query("INSERT INTO session_inspection_acl(session_key,session_identity,principal_id,revision,can_inspect,expires_at_ms) VALUES($1,$2,$3,1,true,$4)").bind(&key).bind(json!(expected.session)).bind(viewer).bind((NOW+100)as i64).execute(&pool).await.unwrap();
    let (code, v) = read(&app, &usage_path, "usage-viewer@localhost").await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(usage(&v), persisted);
    let (code, v) = read(&app, &detail_path, "usage-viewer@localhost").await;
    assert_eq!(code, StatusCode::OK, "{v}");
    assert_eq!(v["data"]["observation_binding"], json!(expected));
    assert_eq!(
        read(
            &app,
            &format!("/api/observations/sessions/{session_id}/usage"),
            "usage-viewer@localhost"
        )
        .await
        .0,
        StatusCode::NOT_FOUND,
        "session UUID must not alias attempt UUID"
    );
    let (old_attempt, old_session) = legacy(&pool, &f.node, expected.project_id).await;
    let (code, old) = read(
        &app,
        &format!("/api/v1/sessions/{old_session}"),
        "owner@localhost",
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{old}");
    assert_eq!(old["data"]["source_kind"], "legacy_fixture");
    assert!(old["data"].get("observation_binding").is_none());
    assert_eq!(
        read(
            &app,
            &format!("/api/observations/sessions/{old_attempt}/usage"),
            "owner@localhost"
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    // Worker revocation does not erase authorized historical observation facts.
    sqlx::query("UPDATE nodes SET revoked_at=now() WHERE id=$1")
        .bind(f.node.node_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        read(&app, &usage_path, "owner@localhost").await.0,
        StatusCode::OK
    );
    assert_eq!(
        read(&app, &usage_path, "usage-viewer@localhost").await.0,
        StatusCode::OK
    );
    f.time.store((NOW + 101) as i64, Ordering::SeqCst);
    assert_eq!(
        read(&app, &usage_path, "usage-viewer@localhost").await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        read(&app, &detail_path, "usage-viewer@localhost").await.0,
        StatusCode::FORBIDDEN
    );
    f.time.store(NOW as i64, Ordering::SeqCst);
    sqlx::query("UPDATE session_inspection_acl SET revision=2,can_inspect=false WHERE session_key=$1 AND principal_id=$2").bind(key).bind(viewer).execute(&pool).await.unwrap();
    assert_eq!(
        read(&app, &usage_path, "usage-viewer@localhost").await.0,
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(f.actor)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        read(&app, &usage_path, "owner@localhost").await.0,
        StatusCode::FORBIDDEN
    );
}
#[tokio::test]
async fn excessive_visibility_disables_the_usage_dashboard_capability() {
    let f = fixture().await;
    insert(&f).await;
    let pool = f.db.pool().await;
    let viewer = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,'bounded-usage@localhost','viewer','dev','bounded-usage@localhost',now())").bind(viewer).execute(&pool).await.unwrap();
    let mut tx = pool.begin().await.unwrap();
    for _ in 0..257 {
        let mut session = f.profiled.offer.binding.session.clone();
        if let aq_session_protocol::SessionIdentity::Remote { session_id, .. } = &mut session {
            *session_id = Uuid::new_v4();
        }
        let key = aq_server::session_access::session_key(&session).unwrap();
        sqlx::query("INSERT INTO session_inspection_acl(session_key,session_identity,principal_id,revision,can_inspect,expires_at_ms) VALUES($1,$2,$3,1,true,$4)").bind(key).bind(json!(session)).bind(viewer).bind((NOW+1000)as i64).execute(&mut *tx).await.unwrap();
    }
    tx.commit().await.unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    assert!(!bootstrap(&app, "bounded-usage@localhost").await);
}

/// Optional real HTTP+bundle inspection of this disposable persisted SQL fixture.
/// It supplies synthetic incomplete facts, never a provider or billing claim.
async fn optional_browser(
    f: &common::profiled::Fixture,
    binding: &aq_lifecycle_api::Binding,
    session_id: Uuid,
) {
    let Some(script) = std::env::var_os("AQ_TEST_USAGE_BROWSER_SCRIPT") else {
        assert!(std::env::var_os("AQ_TEST_USAGE_DASHBOARD_DIR").is_none());
        return;
    };
    let bundle = std::env::var_os("AQ_TEST_USAGE_DASHBOARD_DIR")
        .expect("explicit usage probe requires a compiled dashboard bundle");
    let assets =
        aq_server::dashboard_assets::DashboardAssets::load(std::path::Path::new(&bundle)).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let origin = format!("http://{}", listener.local_addr().unwrap());
    let mut state = f.state.clone();
    let mut config = (*state.config).clone();
    config.server.public_origin = origin.clone();
    state.config = std::sync::Arc::new(config);
    let router = aq_server::build_admin_router_with_dashboard_and_policy_owners(
        state,
        None,
        None,
        std::sync::Arc::new(assets),
    );
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let mut probe = tokio::process::Command::new("node")
        .arg(script)
        .env("AQ_LIVE_URL", origin)
        .env("AQ_LIVE_SESSION_ID", session_id.to_string())
        .env("AQ_LIVE_ATTEMPT_ID", binding.attempt_id.to_string())
        .env("AQ_LIVE_PROJECT_ID", binding.project_id.to_string())
        .env(
            "AQ_LIVE_USAGE_BINDING",
            serde_json::to_string(binding).unwrap(),
        )
        .env("AQ_LIVE_USAGE_REVISION", "3")
        .env("AQ_LIVE_USAGE_SUBTOTALS", "[120,9,80,0,3]")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let result = tokio::time::timeout(std::time::Duration::from_secs(25), probe.wait()).await;
    server.abort();
    assert!(
        result
            .expect("bounded actual persisted usage browser probe")
            .unwrap()
            .success()
    );
}
