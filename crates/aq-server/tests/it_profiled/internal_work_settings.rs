use super::profile_catalog::{call, settings_fixture};
use aq_internal_work::{Purpose, settings::profile_for};
use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use uuid::Uuid;

const PATH: &str = "/api/settings/internal-work";

fn request(snapshot: &Value, default: Value, overrides: Value) -> Value {
    json!({"operation_id":Uuid::new_v4(),"expected_revision":snapshot["revision"],
        "expected_catalog_revision":snapshot["catalog_revision"],
        "default_profile_id":default,"overrides":overrides})
}
async fn fixture() -> (aq_server::test_support::TestDb, Router, sqlx::PgPool, Value) {
    let (db, app, pool, _, _) = settings_fixture().await;
    let (status, snapshot) = call(&app, PATH, None, false).await;
    assert_eq!(status, StatusCode::OK, "{snapshot}");
    (db, app, pool, snapshot)
}

#[tokio::test]
async fn default_and_override_round_trip_and_replay_returns_the_same_receipt() {
    let (_db, app, _pool, initial) = fixture().await;
    assert_eq!(initial["revision"], "0");
    assert_eq!(initial["default_profile_id"], Value::Null);
    assert_eq!(initial["purposes"].as_array().unwrap().len(), 5);
    let body = request(
        &initial,
        json!("engineering"),
        json!({"playbook_call":"engineering"}),
    );
    let (status, saved) = call(&app, PATH, Some(body.clone()), true).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["disposition"], "saved");
    assert_eq!(saved["revision"], "1");
    let (_, read) = call(&app, PATH, None, false).await;
    assert_eq!(read["default_profile_id"], "engineering");
    assert_eq!(read["overrides"], json!({"playbook_call":"engineering"}));
    let replay = call(&app, PATH, Some(body.clone()), true).await;
    assert_eq!(replay, (StatusCode::OK, saved.clone()));
    let id = body["operation_id"].as_str().unwrap();
    let (status, receipt) = call(&app, &format!("{PATH}/operations/{id}"), None, false).await;
    assert_eq!((status, &receipt), (StatusCode::OK, &saved));
    let mut reused = body;
    reused["default_profile_id"] = Value::Null;
    assert_eq!(
        call(&app, PATH, Some(reused), true).await.0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn stale_revision_and_stale_catalog_revision_conflict() {
    let (_db, app, pool, initial) = fixture().await;
    let ok = request(&initial, json!("engineering"), json!({}));
    assert_eq!(call(&app, PATH, Some(ok), true).await.0, StatusCode::OK);
    let stale = request(&initial, json!(null), json!({}));
    assert_eq!(
        call(&app, PATH, Some(stale), true).await.0,
        StatusCode::CONFLICT
    );
    let (_, current) = call(&app, PATH, None, false).await;
    sqlx::query("UPDATE profile_catalog_registry SET revision=revision+1")
        .execute(&pool)
        .await
        .unwrap();
    let stale_catalog = request(&current, json!(null), json!({}));
    assert_eq!(
        call(&app, PATH, Some(stale_catalog), true).await.0,
        StatusCode::CONFLICT
    );
}

#[tokio::test]
async fn unknown_profile_and_automatic_router_are_refused() {
    let (_db, app, pool, initial) = fixture().await;
    let unknown = request(&initial, json!("nope"), json!({}));
    assert_eq!(
        call(&app, PATH, Some(unknown), true).await.0,
        StatusCode::BAD_REQUEST
    );
    let overridden = request(&initial, json!(null), json!({"playbook_call":"nope"}));
    assert_eq!(
        call(&app, PATH, Some(overridden), true).await.0,
        StatusCode::BAD_REQUEST
    );
    // Mark the active catalog's profile as the automatic router.
    let digest: String =
        sqlx::query_scalar("SELECT active_digest FROM profile_catalog_registry WHERE singleton")
            .fetch_one(&pool)
            .await
            .unwrap();
    let bytes: Vec<u8> =
        sqlx::query_scalar("SELECT artifact FROM profile_catalog_artifacts WHERE digest=$1")
            .bind(&digest)
            .fetch_one(&pool)
            .await
            .unwrap();
    let mut catalog = aq_profiles::ValidatedCatalog::from_artifact(&bytes)
        .unwrap()
        .artifact()
        .catalog
        .clone();
    let mut router = catalog.intelligence[0].clone();
    router.id = "router".into();
    router.automatic_routing = Some(true);
    catalog.intelligence.push(router);
    let instructions = aq_profiles::ValidatedCatalog::from_artifact(&bytes)
        .unwrap()
        .artifact()
        .instructions
        .clone();
    let catalog = aq_profiles::validate(catalog, instructions).unwrap();
    let revision: i64 =
        sqlx::query_scalar("SELECT revision FROM profile_catalog_registry WHERE singleton")
            .fetch_one(&pool)
            .await
            .unwrap();
    let activation = json!({"operation_id":Uuid::new_v4(),"expected_revision":revision,
        "artifact_json":String::from_utf8(catalog.canonical_bytes().unwrap()).unwrap()});
    let (status, body) = call(
        &app,
        "/api/profiles/catalogs/activate",
        Some(activation),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, current) = call(&app, PATH, None, false).await;
    let router = request(&current, json!("router"), json!({}));
    let (status, body) = call(&app, PATH, Some(router), true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
}

#[tokio::test]
async fn unknown_purpose_key_is_refused() {
    let (_db, app, _pool, initial) = fixture().await;
    let body = request(
        &initial,
        json!(null),
        json!({"not_a_purpose":"engineering"}),
    );
    assert_eq!(
        call(&app, PATH, Some(body), true).await.0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn save_requires_owner_and_csrf_header() {
    let (_db, app, _pool, initial) = fixture().await;
    let body = request(&initial, json!("engineering"), json!({}));
    assert_eq!(
        call(&app, PATH, Some(body), false).await.0,
        StatusCode::FORBIDDEN
    );
    let (_, read) = call(&app, PATH, None, false).await;
    assert_eq!(read["revision"], "0");
}

#[tokio::test]
async fn profile_for_uses_override_then_default_then_none() {
    let (_db, app, pool, initial) = fixture().await;
    assert_eq!(
        profile_for(&pool, Purpose::PlaybookCall).await.unwrap(),
        None
    );
    let body = request(&initial, json!("engineering"), json!({}));
    assert_eq!(call(&app, PATH, Some(body), true).await.0, StatusCode::OK);
    assert_eq!(
        profile_for(&pool, Purpose::PlaybookCall).await.unwrap(),
        Some("engineering".into())
    );
    sqlx::query("UPDATE internal_work_settings SET document=$1")
        .bind(json!({"default_profile_id":"engineering","overrides":{"playbook_call":"other"}}))
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        profile_for(&pool, Purpose::PlaybookCall).await.unwrap(),
        Some("other".into())
    );
    assert_eq!(
        profile_for(&pool, Purpose::DecisionRevision).await.unwrap(),
        Some("engineering".into())
    );
}
