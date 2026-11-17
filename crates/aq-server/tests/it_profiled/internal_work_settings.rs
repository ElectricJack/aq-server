use super::profile_catalog::{call, setup};
use aq_internal_work::{Purpose, settings};
use axum::{Router, http::StatusCode};
use serde_json::{Value, json};
use uuid::Uuid;

const PATH: &str = "/api/settings/internal-work";

fn request(snapshot: &Value, default: Value, overrides: Value) -> Value {
    json!({"operation_id":Uuid::new_v4(),"expected_revision":snapshot["revision"],
        "default_profile_id":default,"overrides":overrides})
}
async fn fixture() -> (aq_server::test_support::TestDb, Router, sqlx::PgPool, Value) {
    // The store has the shipped roles and profiles active, as after start.
    let (db, app, pool) = setup().await;
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
        json!("standard-high"),
        json!({"playbook_call":"standard-high"}),
    );
    let (status, saved) = call(&app, PATH, Some(body.clone()), true).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["disposition"], "saved");
    assert_eq!(saved["revision"], "1");
    let (_, read) = call(&app, PATH, None, false).await;
    assert_eq!(read["default_profile_id"], "standard-high");
    assert_eq!(read["overrides"], json!({"playbook_call":"standard-high"}));
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
async fn stale_revision_conflicts() {
    let (_db, app, _pool, initial) = fixture().await;
    let ok = request(&initial, json!("standard-high"), json!({}));
    assert_eq!(call(&app, PATH, Some(ok), true).await.0, StatusCode::OK);
    let stale = request(&initial, json!(null), json!({}));
    assert_eq!(
        call(&app, PATH, Some(stale), true).await.0,
        StatusCode::CONFLICT
    );
}

/// A choice names a level of the active role snapshot (spec 0014 §5.2).
#[tokio::test]
async fn a_level_that_does_not_exist_is_refused_by_name() {
    let (_db, app, _pool, initial) = fixture().await;
    let unknown = request(&initial, json!("nope"), json!({}));
    let (status, body) = call(&app, PATH, Some(unknown), true).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(body.to_string().contains("nope"), "{body}");
    let overridden = request(&initial, json!(null), json!({"playbook_call":"nope"}));
    assert_eq!(
        call(&app, PATH, Some(overridden), true).await.0,
        StatusCode::BAD_REQUEST
    );
    // The removed catalog fence is an unknown field.
    let mut fenced = request(&initial, json!("standard-high"), json!({}));
    fenced["expected_catalog_revision"] = json!("0");
    assert_eq!(
        call(&app, PATH, Some(fenced), true).await.0,
        StatusCode::BAD_REQUEST
    );
    let (_, read) = call(&app, PATH, None, false).await;
    assert_eq!(read["revision"], "0");
    assert_eq!(read["missing"], json!([]));
    assert_eq!(read.get("catalog_revision"), None);
}

/// A level deleted after it was chosen: the read names it, and its purpose
/// runs on the default.
#[tokio::test]
async fn a_stored_level_that_no_longer_exists_is_reported_and_falls_back_to_the_default() {
    let (_db, app, pool, _) = fixture().await;
    sqlx::query("UPDATE internal_work_settings SET document=$1")
        .bind(json!({"default_profile_id":"standard-high",
            "overrides":{"playbook_call":"gone","decision_revision":"simple-low"}}))
        .execute(&pool)
        .await
        .unwrap();
    let (status, read) = call(&app, PATH, None, false).await;
    assert_eq!(status, StatusCode::OK, "{read}");
    assert_eq!(
        read["missing"],
        json!([{"purpose":"playbook_call","profile_id":"gone"}])
    );
    assert_eq!(read["overrides"]["playbook_call"], "gone");
    let active = aq_role_settings::active_snapshot(&pool).await.unwrap();
    let levels = (active.unwrap().snapshot.intelligence_profiles())
        .map(|p| p.id.clone())
        .collect();
    let document = settings::document(&pool).await.unwrap();
    let level = |purpose| settings::level_for(&document, purpose, &levels);
    assert_eq!(level(Purpose::PlaybookCall), Ok("standard-high".into()));
    assert_eq!(level(Purpose::DecisionRevision), Ok("simple-low".into()));

    // The default itself is gone: the read says so, and there is no level.
    sqlx::query("UPDATE internal_work_settings SET document=$1")
        .bind(json!({"default_profile_id":"gone","overrides":{}}))
        .execute(&pool)
        .await
        .unwrap();
    let (_, read) = call(&app, PATH, None, false).await;
    assert_eq!(
        read["missing"],
        json!([{"purpose":null,"profile_id":"gone"}])
    );
    let document = settings::document(&pool).await.unwrap();
    let reason = settings::level_for(&document, Purpose::PlaybookCall, &levels).unwrap_err();
    assert!(reason.contains("\"gone\""), "{reason}");
}

#[tokio::test]
async fn unknown_purpose_key_is_refused() {
    let (_db, app, _pool, initial) = fixture().await;
    let body = request(
        &initial,
        json!(null),
        json!({"not_a_purpose":"standard-high"}),
    );
    assert_eq!(
        call(&app, PATH, Some(body), true).await.0,
        StatusCode::BAD_REQUEST
    );
}

#[tokio::test]
async fn save_requires_owner_and_csrf_header() {
    let (_db, app, _pool, initial) = fixture().await;
    let body = request(&initial, json!("standard-high"), json!({}));
    assert_eq!(
        call(&app, PATH, Some(body), false).await.0,
        StatusCode::FORBIDDEN
    );
    let (_, read) = call(&app, PATH, None, false).await;
    assert_eq!(read["revision"], "0");
}

#[tokio::test]
async fn a_purpose_uses_its_override_then_the_default_then_none() {
    let (_db, app, pool, initial) = fixture().await;
    let active = aq_role_settings::active_snapshot(&pool).await.unwrap();
    let levels = (active.unwrap().snapshot.intelligence_profiles())
        .map(|p| p.id.clone())
        .collect();
    let document = settings::document(&pool).await.unwrap();
    assert_eq!(
        settings::level_for(&document, Purpose::PlaybookCall, &levels),
        Err("no internal work profile configured".into())
    );
    let body = request(
        &initial,
        json!("standard-high"),
        json!({"playbook_call":"simple-low"}),
    );
    assert_eq!(call(&app, PATH, Some(body), true).await.0, StatusCode::OK);
    let document = settings::document(&pool).await.unwrap();
    assert_eq!(
        settings::level_for(&document, Purpose::PlaybookCall, &levels),
        Ok("simple-low".into())
    );
    assert_eq!(
        settings::level_for(&document, Purpose::DecisionRevision, &levels),
        Ok("standard-high".into())
    );
}
