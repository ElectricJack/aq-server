use super::*;
#[tokio::test]
async fn committed_intents_retry_after_producer_already_sees_open_episode() {
    let e = Env::new(false).await;
    let pool = &e.policy.service().pool;
    sqlx::query(
        "UPDATE project_execution_bindings SET binding=jsonb_set(binding,'{enabled}','false')",
    )
    .execute(pool)
    .await
    .unwrap();
    assert!(
        e.policy
            .observe(e.event(Kind::QuestionAsked, "pending", Transition::Opened))
            .await
            .is_err()
    );
    assert!(e.messages().await.is_empty());
    sqlx::query(
        "UPDATE project_execution_bindings SET binding=jsonb_set(binding,'{enabled}','true')",
    )
    .execute(pool)
    .await
    .unwrap();
    assert_eq!(e.policy.retry_pending().await.unwrap(), 1);
    assert_eq!(e.policy.retry_pending().await.unwrap(), 0);
    assert_eq!(e.messages().await.len(), 1);
}

#[tokio::test]
async fn gc_preserves_open_pending_and_unflushed_state_and_uses_new_identity() {
    use aq_server::notifications::policy::RETENTION_MS;
    let e = Env::new(false).await;
    let old = e
        .policy
        .observe(e.event(Kind::QuestionAsked, "resolved", Transition::Opened))
        .await
        .unwrap();
    e.policy
        .observe(e.event(Kind::QuestionAsked, "resolved", Transition::Resolved))
        .await
        .unwrap();
    for name in ["open-a", "open-b", "collapsed"] {
        e.policy
            .observe(e.event(Kind::QuestionAsked, name, Transition::Opened))
            .await
            .unwrap();
    }
    let pool = &e.policy.service().pool;
    sqlx::query(
        "UPDATE project_execution_bindings SET binding=jsonb_set(binding,'{enabled}','false')",
    )
    .execute(pool)
    .await
    .unwrap();
    e.time.store(720_000, Ordering::SeqCst);
    assert!(
        e.policy
            .observe(e.event(Kind::QuestionAsked, "pending", Transition::Opened))
            .await
            .is_err()
    );
    e.policy
        .observe(e.event(Kind::QuestionAsked, "pending", Transition::Resolved))
        .await
        .unwrap();
    e.time.fetch_add(RETENTION_MS as i64 + 1, Ordering::SeqCst);
    e.policy.garbage_collect().await.unwrap();
    assert_retained(pool).await;
    sqlx::query(
        "UPDATE project_execution_bindings SET binding=jsonb_set(binding,'{enabled}','true')",
    )
    .execute(pool)
    .await
    .unwrap();
    assert_eq!(e.policy.retry_pending().await.unwrap(), 1);
    assert_eq!(e.policy.flush().await.unwrap(), 1);
    e.policy.garbage_collect().await.unwrap();
    let new = e
        .policy
        .observe(e.event(Kind::QuestionAsked, "resolved", Transition::Opened))
        .await
        .unwrap();
    assert_ne!(
        old["message_id"], new["message_id"],
        "GC must not reuse generation-1 message identity"
    );
}

#[tokio::test]
async fn gc_reclaims_capacity_in_all_three_policy_tables() {
    use aq_server::notifications::policy::RETENTION_MS;
    let e = Env::new(false).await;
    let pool = &e.policy.service().pool;
    e.time.store((2 * RETENTION_MS) as i64, Ordering::SeqCst);
    sqlx::query("INSERT INTO notification_episodes(episode_key,kind,subject,project_id,workspace_id,generation,open,opened_at_ms,resolved_at_ms,outcome,identity) SELECT 'old:'||n,'question.asked',n::text,$1,$2,1,FALSE,1,1,'collapsed',md5(n::text)::uuid FROM generate_series(1,4096) n")
        .bind(e.scope.project_id).bind(e.scope.workspace_id).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO notification_policy_events(source_event_id,request_digest,response,created_at_ms,submitted) SELECT md5(n::text)::uuid,md5(n::text)||md5(n::text),'{}',1,TRUE FROM generate_series(1,8192) n").execute(pool).await.unwrap();
    sqlx::query("INSERT INTO notification_flood_windows(project_id,workspace_id,window_start_ms) SELECT $1,$2,n*60000 FROM generate_series(1,1024) n")
        .bind(e.scope.project_id).bind(e.scope.workspace_id).execute(pool).await.unwrap();
    assert!(
        e.policy
            .observe(e.event(Kind::QuestionAsked, "new", Transition::Opened))
            .await
            .is_err()
    );
    assert_eq!(e.policy.garbage_collect().await.unwrap(), 1536);
    assert_eq!(
        e.policy
            .observe(e.event(Kind::QuestionAsked, "new", Transition::Opened))
            .await
            .unwrap()["outcome"],
        "notified"
    );
}

async fn assert_retained(pool: &sqlx::PgPool) {
    let subjects: Vec<String> =
        sqlx::query_scalar("SELECT subject FROM notification_episodes ORDER BY subject")
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(subjects, ["collapsed", "open-a", "open-b", "pending"]);
    let pending: i64 =
        sqlx::query_scalar("SELECT count(*) FROM notification_policy_events WHERE NOT submitted")
            .fetch_one(pool)
            .await
            .unwrap();
    assert_eq!(pending, 1);
    let windows: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM notification_flood_windows WHERE collapsed>0 AND NOT submitted",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(windows, 1);
}
