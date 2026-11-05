//! Immutable catalog activation through the real owner router and PostgreSQL.
use aq_profiles::{ValidatedCatalog, load, validate};
use aq_server::{AppState, config::Config, identity::verifier_from_config, test_support::TestDb};
use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use chrono::Utc;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::{path::Path, sync::Arc};
use tower::ServiceExt;
use uuid::Uuid;

fn seed() -> ValidatedCatalog {
    load(&Path::new(env!("CARGO_MANIFEST_DIR")).join("../aq-profiles/examples/catalog")).unwrap()
}
async fn setup() -> (TestDb, Router, sqlx::PgPool) {
    setup_with("").await
}
async fn setup_with(extra: &str) -> (TestDb, Router, sqlx::PgPool) {
    let db = TestDb::create().await;
    let store = db.store().await;
    store
        .bootstrap_owner("owner@localhost", "dev", "owner@localhost", Utc::now())
        .await
        .unwrap()
        .unwrap();
    let config: Config = toml::from_str(&format!(
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
{extra}"#
    ))
    .unwrap();
    let state = AppState {
        identity: verifier_from_config(&config.auth, reqwest::Client::new()),
        config: Arc::new(config),
        store,
        clock: Arc::new(Utc::now),
        agent_access: None,
        source_retention: None,
    };
    let pool = db.pool().await;
    (db, aq_server::build_admin_router(state), pool)
}
async fn call(app: &Router, path: &str, body: Option<Value>, csrf: bool) -> (StatusCode, Value) {
    let mut req = Request::builder()
        .uri(path)
        .method(if body.is_some() { "POST" } else { "GET" })
        .header("content-type", "application/json");
    if csrf {
        req = req.header("X-AQ-Request", "1");
    }
    let response = app
        .clone()
        .oneshot(
            req.body(Body::from(body.map(|v| v.to_string()).unwrap_or_default()))
                .unwrap(),
        )
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
fn activation(catalog: &ValidatedCatalog, revision: u64) -> Value {
    json!({"operation_id":Uuid::new_v4(),"expected_revision":revision,"artifact_json":String::from_utf8(catalog.canonical_bytes().unwrap()).unwrap()})
}
const ACTIVATE: &str = "/api/profiles/catalogs/activate";
const STATE: &str = "/api/profiles/catalogs";

#[tokio::test]
async fn missing_registry_is_an_explicit_conflict() {
    let (_db, app, pool) = setup().await;
    sqlx::query("DELETE FROM profile_catalog_registry")
        .execute(&pool)
        .await
        .unwrap();
    let (status, body) = call(&app, STATE, None, false).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert!(
        body["message"]
            .as_str()
            .unwrap()
            .contains("registry has not been installed")
    );
}

#[tokio::test]
async fn exact_retry_survives_reactivation_and_changed_payload_conflicts() {
    let (_db, app, pool) = setup().await;
    let catalog = seed();
    assert_eq!(
        call(&app, STATE, None, false).await.1,
        json!({"revision":0,"active_digest":null})
    );
    let request = activation(&catalog, 0);
    let first = call(&app, ACTIVATE, Some(request.clone()), true).await;
    assert_eq!(first.0, StatusCode::OK, "{}", first.1);
    assert_eq!(first.1["revision"], 1);
    assert_eq!(
        call(&app, ACTIVATE, Some(activation(&catalog, 1)), true)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, ACTIVATE, Some(request.clone()), true).await,
        first
    );
    let mut changed = request;
    changed["artifact_json"] = json!(format!("{}\n", changed["artifact_json"].as_str().unwrap()));
    assert_eq!(
        call(&app, ACTIVATE, Some(changed), true).await.0,
        StatusCode::CONFLICT
    );
    assert_eq!(call(&app, STATE, None, false).await.1["revision"], 2);
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profile_catalog_artifacts")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profile_catalog_operations")
            .fetch_one(&pool)
            .await
            .unwrap(),
        2
    );
}

#[tokio::test]
async fn activation_pins_environment_template_or_refuses() {
    let dir = tempfile::tempdir().unwrap();
    let digest = |c: char| std::iter::repeat_n(c, 64).collect::<String>();
    let write = |name: &str, entries: Value| {
        let path = dir.path().join(name);
        std::fs::write(
            &path,
            json!({"schema_version":1,"entries":entries}).to_string(),
        )
        .unwrap();
        path
    };
    let entry = |name: &str, c: char| json!({"name":name,"version":"1","image_digest":format!("sha256:{}",digest(c))});
    let catalog = seed();
    let pinned = write(
        "pinned.json",
        json!([entry("confined-worktree", 'a'), entry("other", 'b')]),
    );
    let (_db, app, pool) = setup_with(&format!(
        "[templates]\nallowlist={:?}\n",
        pinned.display().to_string()
    ))
    .await;
    let ok = call(&app, ACTIVATE, Some(activation(&catalog, 0)), true).await;
    assert_eq!(ok.0, StatusCode::OK, "{}", ok.1);
    assert_eq!(
        ok.1["environment_templates"],
        json!([{"environment":"confined-worktree","template_digest":digest('a')}])
    );
    let stored: Vec<(String, String)> = sqlx::query_as(
        "SELECT environment,template_digest FROM profile_catalog_environment_templates",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(stored, [("confined-worktree".into(), digest('a'))]);
    for (name, entries, reason) in [
        (
            "missing.json",
            json!([entry("other", 'b')]),
            "environment_template_missing",
        ),
        (
            "ambiguous.json",
            json!([
                entry("confined-worktree", 'a'),
                entry("confined-worktree", 'c')
            ]),
            "environment_template_ambiguous",
        ),
    ] {
        let path = write(name, entries);
        let (_db, app, pool) = setup_with(&format!(
            "[templates]\nallowlist={:?}\n",
            path.display().to_string()
        ))
        .await;
        let refused = call(&app, ACTIVATE, Some(activation(&catalog, 0)), true).await;
        assert_eq!(refused.0, StatusCode::CONFLICT, "{}", refused.1);
        assert!(refused.1.to_string().contains(reason), "{}", refused.1);
        assert_eq!(call(&app, STATE, None, false).await.1["revision"], 0);
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT count(*) FROM profile_catalog_environment_templates"
            )
            .fetch_one(&pool)
            .await
            .unwrap(),
            0
        );
    }
}

#[tokio::test]
async fn concurrent_activation_has_one_revision_winner() {
    let (_db, app, _pool) = setup().await;
    let catalog = seed();
    let (a, b) = tokio::join!(
        call(&app, ACTIVATE, Some(activation(&catalog, 0)), true),
        call(&app, ACTIVATE, Some(activation(&catalog, 0)), true)
    );
    assert!(
        (a.0 == StatusCode::OK && b.0 == StatusCode::CONFLICT)
            || (b.0 == StatusCode::OK && a.0 == StatusCode::CONFLICT)
    );
    assert_eq!(call(&app, STATE, None, false).await.1["revision"], 1);
}

#[tokio::test]
async fn invalid_edits_preserve_last_good_and_rollback_retains_exact_artifacts() {
    let (_db, app, pool) = setup().await;
    let catalog = seed();
    assert_eq!(
        call(&app, ACTIVATE, Some(activation(&catalog, 0)), true)
            .await
            .0,
        StatusCode::OK
    );
    let mut malformed = activation(&catalog, 1);
    malformed["artifact_json"] = json!("{broken");
    assert_eq!(
        call(&app, ACTIVATE, Some(malformed), true).await.0,
        StatusCode::BAD_REQUEST
    );
    let mut texts = catalog.artifact().instructions.clone();
    texts.insert(
        "engineering@1".into(),
        "Changed instructions under existing identity.".into(),
    );
    let changed = validate(catalog.artifact().catalog.clone(), texts).unwrap();
    assert_eq!(
        call(&app, ACTIVATE, Some(activation(&changed, 1)), true)
            .await
            .0,
        StatusCode::CONFLICT
    );
    assert_eq!(
        call(&app, STATE, None, false).await.1["active_digest"],
        catalog.digest()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profile_catalog_artifacts")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    // A new identity can activate; old bytes remain readable and rollback is CAS.
    let mut next_catalog = catalog.artifact().catalog.clone();
    let mut new_module = next_catalog.modules[0].clone();
    new_module.id = "new-policy".into();
    new_module.instruction = "instructions/new-policy.md".into();
    next_catalog.modules.push(new_module);
    let mut texts = catalog.artifact().instructions.clone();
    texts.insert(
        "new-policy@1".into(),
        "New policy for future compositions.".into(),
    );
    let next = validate(next_catalog, texts).unwrap();
    assert_eq!(
        call(&app, ACTIVATE, Some(activation(&next, 1)), true)
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        call(&app, ACTIVATE, Some(activation(&catalog, 2)), true)
            .await
            .0,
        StatusCode::OK
    );
    for c in [&catalog, &next] {
        let fetched = call(&app, &format!("{STATE}/{}", c.digest()), None, false).await;
        assert_eq!(fetched.0, StatusCode::OK);
        let restored =
            ValidatedCatalog::from_artifact(&serde_json::to_vec(&fetched.1["artifact"]).unwrap())
                .unwrap();
        assert_eq!(
            restored.canonical_bytes().unwrap(),
            c.canonical_bytes().unwrap()
        );
    }
    // Identity history outlives rollback.
    let mut texts = next.artifact().instructions.clone();
    texts.insert(
        "new-policy@1".into(),
        "Reused version with different bytes.".into(),
    );
    let invalid = validate(next.artifact().catalog.clone(), texts).unwrap();
    assert_eq!(
        call(&app, ACTIVATE, Some(activation(&invalid, 3)), true)
            .await
            .0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn catalogs_require_owner_and_csrf_and_reject_bad_ids() {
    let (_db, app, pool) = setup().await;
    let catalog = seed();
    assert_eq!(
        call(&app, ACTIVATE, Some(activation(&catalog, 0)), false)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    let mut nil = activation(&catalog, 0);
    nil["operation_id"] = json!(Uuid::nil());
    assert_eq!(
        call(&app, ACTIVATE, Some(nil), true).await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(&app, &format!("{STATE}/not-a-digest"), None, false)
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    sqlx::query("UPDATE users SET role='viewer'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&app, STATE, None, false).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&app, ACTIVATE, Some(activation(&catalog, 0)), true)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&app, &format!("{STATE}/{}", catalog.digest()), None, false)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT revision FROM profile_catalog_registry")
            .fetch_one(&pool)
            .await
            .unwrap(),
        0
    );
}
