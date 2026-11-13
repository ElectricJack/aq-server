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
async fn read_committed_history_quota_resamples_after_global_lock_wait_across_nodes() {
    // Exercise the SQL protection boundary with tiny synthetic history payloads.
    // These are quota fixtures, not decoder-valid runtime registration claims.
    let db = TestDb::create().await;
    let _store = db.store().await;
    // Barrier + two simultaneous mutators + independent observer. The generic
    // fixture pool has only two connections and cannot exercise this race.
    let pool = sqlx::postgres::PgPoolOptions::new()
        .max_connections(4)
        .connect_with(db.options())
        .await
        .unwrap();
    let nodes = (0..7).map(|_| uuid::Uuid::new_v4()).collect::<Vec<_>>();
    for (i, node) in nodes.iter().enumerate() {
        sqlx::query("INSERT INTO nodes(id,name,credential_hash,worker_version,protocol_version,enrolled_at) VALUES($1,'quota-fixture',$2,'0.3.3',2,now())").bind(node).bind(format!("{i:064x}")).execute(&pool).await.unwrap();
    }
    sqlx::query("INSERT INTO profiled_native_registration_history(registration_id,node_id,boot_id,identity_digest,semantic_digest,metadata_bytes) SELECT md5('quota-seed-'||g)::UUID,($1::UUID[])[1+(g%5)],md5('boot-'||g)::UUID,md5(g::TEXT)||md5(g::TEXT),repeat('a',64),convert_to('{}','UTF8') FROM generate_series(1,4095) g").bind(&nodes[..5]).execute(&pool).await.unwrap();
    let mut barrier = pool.begin().await.unwrap();
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('aq-native-registration-history-v1',0))",
    )
    .execute(&mut *barrier)
    .await
    .unwrap();
    let mut tasks = vec![];
    for node in &nodes[5..] {
        let pool = pool.clone();
        let node = *node;
        tasks.push(tokio::spawn(async move {
            sqlx::query("/* closure28-history-race */ INSERT INTO profiled_native_registration_history(registration_id,node_id,boot_id,identity_digest,semantic_digest,metadata_bytes) VALUES($1,$2,$3,repeat('b',64),repeat('c',64),convert_to('{}','UTF8'))").bind(uuid::Uuid::new_v4()).bind(node).bind(uuid::Uuid::new_v4()).execute(&pool).await
        }));
    }
    tokio::time::timeout(std::time::Duration::from_secs(5),async {
        loop {
            let waiters:i64=sqlx::query_scalar("SELECT count(*) FROM pg_stat_activity WHERE datname=current_database() AND wait_event='advisory' AND query LIKE '/* closure28-history-race */%'").fetch_one(&pool).await.unwrap();
            if waiters==2{break}tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }).await.expect("both independent nodes must wait before releasing quota barrier");
    barrier.commit().await.unwrap();
    let mut passed = 0;
    let mut refused = 0;
    for task in tasks {
        match task.await.unwrap() {
            Ok(_) => passed += 1,
            Err(error) => {
                assert!(
                    error.to_string().contains("registration history quota"),
                    "{error}"
                );
                refused += 1
            }
        }
    }
    assert_eq!((passed, refused), (1, 1));
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM profiled_native_registration_history")
            .fetch_one(&pool)
            .await
            .unwrap(),
        4096
    );
}
