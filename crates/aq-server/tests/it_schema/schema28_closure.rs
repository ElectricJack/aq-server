//! Actual ordered schema28; no runtime activation or simulated old-version label.
use aq_server::{
    store::Store,
    test_support::{TestDb, migrate_profile},
};
#[tokio::test]
async fn fresh_closure_schema_is_empty_and_default_router_has_no_owner() {
    use tower::ServiceExt;
    let db = TestDb::create().await;
    let store = db.store().await;
    let pool = db.pool().await;
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        aq_server::STATE_FORMAT_VERSION.to_string()
    );
    for table in [
        "profiled_native_registration_history",
        "profiled_launch_registrations",
        "profiled_launch_closures",
    ] {
        let n: i64 = sqlx::query_scalar(&format!("SELECT count(*) FROM {table}"))
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 0);
    }
    let mut config:aq_server::config::Config=toml::from_str("format_version=1\nenvironment='development'\n[server]\nadmin_bind='127.0.0.1:0'\nworker_bind='127.0.0.1:0'\npublic_origin='http://127.0.0.1:8080'\n[auth]\nmode='dev'\ndev_email='nobody@example.invalid'\n[bootstrap]\nenabled=false\n").unwrap();
    config.profiled_execution = None;
    let state = aq_server::AppState {
        internal_work: Default::default(),
        store: store.clone(),
        config: std::sync::Arc::new(config),
        identity: std::sync::Arc::new(aq_server::identity::DevIdentity::new(
            "nobody@example.invalid".into(),
        )),
        clock: std::sync::Arc::new(chrono::Utc::now),
        agent_access: None,
        source_retention: None,
    };
    let response = aq_server::build_worker_router(state)
        .oneshot(
            axum::http::Request::post("/v1/worker/profiled/launch-closure")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 404);
}
#[tokio::test]
async fn actual27_upgrades_to28_and_collision_rolls_back_before_explicit_retry() {
    let db = TestDb::create().await;
    let pool = db.pool().await;
    migrate_profile(&pool, 27).await.unwrap();
    sqlx::query("CREATE TABLE profiled_launch_closures(fixture INTEGER)")
        .execute(&pool)
        .await
        .unwrap();
    assert!(Store::connect_with(db.options()).await.is_err());
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        "27"
    );
    assert!(
        !sqlx::query_scalar::<_, bool>(
            "SELECT to_regclass('public.profiled_native_registration_history') IS NOT NULL"
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM schema_migrations")
            .fetch_one(&pool)
            .await
            .unwrap(),
        27
    );
    sqlx::query("DROP TABLE profiled_launch_closures")
        .execute(&pool)
        .await
        .unwrap();
    drop(Store::connect_with(db.options()).await.unwrap());
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        aq_server::STATE_FORMAT_VERSION.to_string()
    );
    assert!(matches!(
        migrate_profile(&pool, 27).await,
        Err(aq_server::store::StoreError::IncompatibleState {
            found,
            supported: 27
        }) if found==aq_server::STATE_FORMAT_VERSION
    ));
}

#[tokio::test]
async fn registration_history_has_no_quota_and_row_retention_keeps_each_nodes_newest() {
    // Architecture 0006. Tiny synthetic history payloads, not decoder-valid
    // runtime registration claims.
    let db = TestDb::create().await;
    let _store = db.store().await;
    let pool = db.pool().await;
    let node = uuid::Uuid::new_v4();
    sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at) VALUES($1,'history-fixture',$2,'0.3.3',2,now())").bind(node).bind(format!("{:064x}", 1)).execute(&pool).await.unwrap();
    // More boots than the old per-node (1,024) and whole-table (4,096) quotas.
    sqlx::query("INSERT INTO profiled_native_registration_history(registration_id,node_id,boot_id,identity_digest,semantic_digest,metadata_bytes,recorded_at_ms) SELECT md5('seed-'||g)::UUID,$1,md5('boot-'||g)::UUID,md5(g::TEXT)||md5(g::TEXT),repeat('c',64),convert_to('{}','UTF8'),g FROM generate_series(1,4200) g").bind(node).execute(&pool).await.unwrap();
    // An ordinary DELETE is still refused.
    assert!(
        sqlx::query("DELETE FROM profiled_native_registration_history")
            .execute(&pool)
            .await
            .is_err()
    );
    let log = aq_server::row_retention::HeartbeatLog {
        keep: 3,
        delete_batch: 10_000,
    };
    let deleted = aq_server::row_retention::registration_history(&pool, 1, &log)
        .await
        .unwrap();
    assert_eq!(deleted, 4197);
    let kept: Vec<i64> = sqlx::query_scalar(
        "SELECT recorded_at_ms FROM profiled_native_registration_history ORDER BY 1",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(kept, [4198, 4199, 4200]);
}
