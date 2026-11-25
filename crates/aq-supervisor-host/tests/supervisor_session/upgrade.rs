use super::*;

#[tokio::test]
async fn main_delivery_database_upgrades_without_losing_history() {
    let db = aq_server::test_support::TestDb::create().await;
    let pool = db.pool().await;
    aq_server::test_support::migrate_profile(&pool, 45)
        .await
        .unwrap();
    sqlx::query("INSERT INTO delivery_intents(operation_id,repository_id,binding_generation,operation,inputs,input_digest,payload_digest,request) VALUES($1,$2,1,'candidate.show','{}',repeat('0',64),repeat('0',64),$3)")
        .bind(Uuid::new_v4()).bind(Uuid::new_v4()).bind(vec![1u8]).execute(&pool).await.unwrap();
    assert!(
        !sqlx::query_scalar::<_, bool>("SELECT to_regclass('central_session_heads') IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap()
    );
    drop(db.store().await);
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT value FROM meta WHERE key='state_format_version'")
            .fetch_one(&pool)
            .await
            .unwrap(),
        aq_server::STATE_FORMAT_VERSION.to_string()
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM delivery_intents")
            .fetch_one(&pool)
            .await
            .unwrap(),
        1
    );
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT to_regclass('central_host_launches') IS NOT NULL")
            .fetch_one(&pool)
            .await
            .unwrap()
    );
}
