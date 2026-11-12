use super::*;
const PROVIDERS: &str = "/api/settings/providers";
async fn configured() -> (TestDb, Router, sqlx::PgPool) {
    setup_with(&format!(
        r#"
[intelligence]
revision=1
[[intelligence.credentials]]
allowed_node_ids=[]
[intelligence.credentials.credential]
profile={{id="claude-account",version="1",digest="{digest}"}}
provider="anthropic"
account_reference="team"
endpoint_reference="default"
revision=1
enabled=true
provisionable=false
[[intelligence.credentials]]
allowed_node_ids=[]
[intelligence.credentials.credential]
profile={{id="codex-account",version="1",digest="{digest}"}}
provider="openai"
account_reference="work"
endpoint_reference="default"
revision=1
enabled=true
provisionable=false
"#,
        digest = "a".repeat(64)
    ))
    .await
}
fn request(snapshot: &Value, providers: Value) -> Value {
    json!({"operation_id":Uuid::new_v4(),"expected_revision":snapshot["revision"],"expected_available_revision":snapshot["available_revision"],"providers":providers})
}
async fn allowed(pool: &sqlx::PgPool, provider: &str) -> Option<&'static str> {
    let mut tx = pool.begin().await.unwrap();
    aq_swarm_settings::admission::check(
        &mut tx,
        Some(&aq_swarm_settings::admission::Cell {
            provider: provider.into(),
            intelligence_class: "standard".into(),
        }),
    )
    .await
    .unwrap()
}
#[tokio::test]
async fn providers_save_remove_restore_empty_and_replay() {
    let (_db, app, pool) = configured().await;
    let (status, initial) = call(&app, PROVIDERS, None, false).await;
    assert_eq!(status, StatusCode::OK, "{initial}");
    assert_eq!(initial["providers"], json!(["anthropic", "openai"]));
    assert_eq!(initial["available"][0]["credential_status"], "missing");
    assert_eq!(allowed(&pool, "anthropic").await, None);
    let remove = request(&initial, json!(["openai"]));
    let (status, saved) = call(&app, PROVIDERS, Some(remove.clone()), true).await;
    assert_eq!(status, StatusCode::OK, "{saved}");
    assert_eq!(saved["revision"], "1");
    assert_eq!(allowed(&pool, "anthropic").await, Some("provider_removed"));
    assert_eq!(allowed(&pool, "openai").await, None);
    let (status, _) = call(&app, PROVIDERS, Some(request(&initial, json!([]))), true).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, restored) = call(
        &app,
        PROVIDERS,
        Some(request(&saved, json!(["anthropic", "openai"]))),
        true,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{restored}");
    assert_eq!(allowed(&pool, "anthropic").await, None);
    let (status, replay) = call(&app, PROVIDERS, Some(remove.clone()), true).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(replay, saved);
    let path = format!(
        "{PROVIDERS}/operations/{}",
        remove["operation_id"].as_str().unwrap()
    );
    let (status, receipt) = call(&app, &path, None, false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(receipt, saved);
    let mut mismatch = remove;
    mismatch["providers"] = json!([]);
    assert_eq!(
        call(&app, PROVIDERS, Some(mismatch), true).await.0,
        StatusCode::CONFLICT
    );
    let (status, empty) = call(&app, PROVIDERS, Some(request(&restored, json!([]))), true).await;
    assert_eq!(status, StatusCode::OK, "{empty}");
    assert_eq!(allowed(&pool, "openai").await, Some("provider_removed"));
    let mut tx = pool.begin().await.unwrap();
    assert_eq!(
        aq_swarm_settings::admission::check(&mut tx, None)
            .await
            .unwrap(),
        Some(aq_swarm_settings::admission::UNCLASSIFIED)
    );
    let (status, persisted) = call(&app, PROVIDERS, None, false).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(persisted["providers"], json!([]));
}
#[tokio::test]
async fn providers_validate_inventory_and_require_owner_and_csrf() {
    let (_db, app, pool) = configured().await;
    let (_, initial) = call(&app, PROVIDERS, None, false).await;
    assert_eq!(
        call(&app, PROVIDERS, Some(request(&initial, json!([]))), false)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    for ids in [json!(["unconfigured"]), json!(["openai", "openai"])] {
        assert_eq!(
            call(&app, PROVIDERS, Some(request(&initial, ids)), true)
                .await
                .0,
            StatusCode::BAD_REQUEST
        );
    }
    let mut stale = request(&initial, json!([]));
    stale["expected_available_revision"] = json!("0".repeat(64));
    assert_eq!(
        call(&app, PROVIDERS, Some(stale), true).await.0,
        StatusCode::CONFLICT
    );
    sqlx::query("UPDATE users SET role='viewer'")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&app, PROVIDERS, None, false).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(&app, PROVIDERS, Some(request(&initial, json!([]))), true)
            .await
            .0,
        StatusCode::FORBIDDEN
    );
}

fn credential(provider: &str) -> aq_selection::AuthorizedCredential {
    serde_json::from_value(json!({"profile":{"id":provider,"version":"1","digest":"a".repeat(64)},"provider":provider,"account_reference":"test","endpoint_reference":"default","revision":1,"enabled":true,"provisionable":false})).unwrap()
}
#[tokio::test]
async fn provider_membership_masks_fresh_selection_but_preserves_reserved_attempts() {
    let (_db, app, pool) = configured().await;
    let (_, initial) = call(&app, PROVIDERS, None, false).await;
    assert_eq!(
        call(
            &app,
            PROVIDERS,
            Some(request(&initial, json!(["openai"]))),
            true
        )
        .await
        .0,
        StatusCode::OK
    );
    let original = vec![credential("anthropic"), credential("openai")];
    let mut fresh = original.clone();
    let mut tx = pool.begin().await.unwrap();
    aq_swarm_settings::providers::mask(&mut tx, &mut fresh, None)
        .await
        .unwrap();
    assert!(!fresh[0].enabled);
    assert!(fresh[1].enabled);
    let mut reserved = original.clone();
    aq_swarm_settings::providers::mask(&mut tx, &mut reserved, Some(&original))
        .await
        .unwrap();
    assert!(reserved.iter().all(|c| c.enabled));
    reserved[0].enabled = false;
    aq_swarm_settings::providers::mask(&mut tx, &mut reserved, Some(&original))
        .await
        .unwrap();
    assert!(
        !reserved[0].enabled,
        "frozen membership cannot override a startup credential revocation"
    );
}
#[tokio::test]
async fn provider_concurrent_saves_have_one_winner_and_failed_receipts_roll_back() {
    let (_db, app, pool) = configured().await;
    let (_, initial) = call(&app, PROVIDERS, None, false).await;
    let (a, b) = tokio::join!(
        call(
            &app,
            PROVIDERS,
            Some(request(&initial, json!(["openai"]))),
            true
        ),
        call(
            &app,
            PROVIDERS,
            Some(request(&initial, json!(["anthropic"]))),
            true
        )
    );
    assert_eq!(
        [a.0, b.0]
            .iter()
            .filter(|status| **status == StatusCode::OK)
            .count(),
        1,
        "{a:?} {b:?}"
    );
    assert!([a.0, b.0].contains(&StatusCode::CONFLICT));
    let (_, before) = call(&app, PROVIDERS, None, false).await;
    sqlx::raw_sql("CREATE FUNCTION provider_test_reject_receipt() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture rejection'; END $$; CREATE TRIGGER provider_test_receipt BEFORE INSERT ON provider_settings_operations FOR EACH ROW EXECUTE FUNCTION provider_test_reject_receipt();").execute(&pool).await.unwrap();
    assert_eq!(
        call(&app, PROVIDERS, Some(request(&before, json!([]))), true)
            .await
            .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let (_, after) = call(&app, PROVIDERS, None, false).await;
    assert_eq!(before, after);
}
