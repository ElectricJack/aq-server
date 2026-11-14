//! Read-only owner inspections through the real authenticated/CSRF router.
use aq_profiles::{CompileRequest, ValidatedCatalog};
use aq_server::{AppState, config::Config, identity::verifier_from_config, test_support::TestDb};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use chrono::{TimeZone, Utc};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tower::ServiceExt;
fn catalog() -> ValidatedCatalog {
    aq_profiles::load(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../aq-profiles/examples/catalog"),
    )
    .unwrap()
}
fn context(id: &str, body: &str) -> Value {
    json!({"source":{"identity":id,"digest":format!("{:x}",Sha256::digest(body.as_bytes())),"revision":"1"},"text":body,"delivery":"startup","required":true,"omission_priority":0})
}
fn composition(c: &ValidatedCatalog) -> CompileRequest {
    let p = &c.artifact().catalog.execution[0].permissions;
    serde_json::from_value(json!({
        "profile":{"catalog_digest":c.digest(),"agent_profile":"implementer@1","project_binding_id":"11111111-1111-4111-8111-111111111111","expected_project_revision":1,"task_context_revision":"task:1"},
        "ceilings":{"installation":p,"project":p,"execution":p,"caller":p},
        "available_tools":["task.read","knowledge.read","source.write"],"available_adapters":["codex"],"harness":"codex","environment":"confined-worktree",
        "platform":context("platform:1","Platform rules"),"project":context("project:1","Project rules"),"assignment":context("assignment:1","Exact assignment"),"context":[],"overrides":[]
    })).unwrap()
}
async fn setup() -> (TestDb, sqlx::PgPool, Router) {
    let db = TestDb::create().await;
    let store = db.store().await;
    let pool = db.pool().await;
    let now = Utc.timestamp_millis_opt(1_000_000).unwrap();
    store
        .bootstrap_owner("owner@localhost", "dev", "owner@localhost", now)
        .await
        .unwrap();
    store
        .invite_user("viewer@localhost", aq_server::store::Role::Viewer, now)
        .await
        .unwrap();
    let config: Config = toml::from_str(
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
"#,
    )
    .unwrap();
    let state = AppState {
        internal_work: Default::default(),
        identity: verifier_from_config(&config.auth, reqwest::Client::new()),
        config: Arc::new(config),
        store,
        clock: Arc::new(|| Utc.timestamp_millis_opt(1_000_000).unwrap()),
        agent_access: None,
        source_retention: None,
    };
    (db, pool, aq_server::build_admin_router(state))
}
async fn post(
    app: &Router,
    path: &str,
    body: Value,
    email: &str,
    csrf: bool,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method("POST")
        .uri(path)
        .header("content-type", "application/json")
        .header("x-aq-dev-email", email);
    if csrf {
        request = request.header("X-AQ-Request", "1");
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"raw":String::from_utf8_lossy(&bytes)})),
    )
}
#[tokio::test]
async fn owner_inspection_is_typed_scoped_and_never_activates_or_resolves_models() {
    let (_db, pool, app) = setup().await;
    let c = catalog();
    let artifact = String::from_utf8(c.canonical_bytes().unwrap()).unwrap();
    let base = json!({"artifact_json":artifact});
    assert_eq!(
        post(
            &app,
            "/api/profiles/validate",
            base.clone(),
            "owner@localhost",
            false
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(
            &app,
            "/api/profiles/validate",
            base.clone(),
            "stranger@localhost",
            true
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        post(
            &app,
            "/api/profiles/validate",
            base.clone(),
            "viewer@localhost",
            true
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
    let (status, out) = post(
        &app,
        "/api/profiles/validate",
        base.clone(),
        "owner@localhost",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(out["catalog_valid"], true);
    assert_eq!(out["authority_admission"], false);
    assert_eq!(
        post(
            &app,
            "/api/profiles/explain",
            base.clone(),
            "owner@localhost",
            true
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let mut full = base.clone();
    full["composition"] = serde_json::to_value(composition(&c)).unwrap();
    full["graph_access_profile_digest"] = "a".repeat(64).into();
    let (status, out) = post(
        &app,
        "/api/profiles/explain",
        full.clone(),
        "owner@localhost",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(out["compilation"]["composition_valid"], true);
    assert_eq!(out["compilation"]["explanation"]["valid"], false);
    assert!(out["compilation"]["explanation"]["selection"]["selected_candidate_id"].is_null());
    full["composition"]["ceilings"]["caller"] = json!([]);
    let (status, out) = post(
        &app,
        "/api/profiles/explain",
        full.clone(),
        "owner@localhost",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(out["compilation"]["composition_valid"], false);
    assert!(out["compilation"]["composition"].is_null());
    full["principal_id"] = "chosen".into();
    assert_eq!(
        post(&app, "/api/profiles/explain", full, "owner@localhost", true)
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let (revision, active): (i64, Option<String>) = sqlx::query_as(
        "SELECT revision,active_digest FROM profile_catalog_registry WHERE singleton",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!((revision, active), (0, None));
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM profile_catalog_artifacts")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
}
#[tokio::test]
async fn malformed_inspection_and_unknown_scope_fields_refuse() {
    let (_db, _pool, app) = setup().await;
    let c = catalog();
    let mut full = json!({"artifact_json":String::from_utf8(c.canonical_bytes().unwrap()).unwrap(),"composition":composition(&c),"graph_access_profile_digest":"a".repeat(64)});
    full["composition"]["ceilings"]["tenant_override"] = json!(["*"]);
    assert_eq!(
        post(&app, "/api/profiles/explain", full, "owner@localhost", true)
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let (status, _) = post(
        &app,
        "/api/profiles/validate",
        json!({"artifact_json":"{}"}),
        "owner@localhost",
        true,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}
