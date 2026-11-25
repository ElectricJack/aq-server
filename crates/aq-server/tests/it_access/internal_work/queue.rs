use super::*;

#[tokio::test]
async fn blocking_tasks_are_claimed_before_older_background_tasks() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let background = store::submit(&pool, &request(false), &[api("openai")], NOW)
        .await
        .unwrap();
    let blocking = store::submit(&pool, &request(true), &[api("openai")], NOW + 10)
        .await
        .unwrap();
    let first = store::Queue::at(&pool, NOW + 20)
        .claim(Way::Api, &[], "runner-a")
        .await
        .unwrap()
        .expect("a queued task");
    assert_eq!(first.id, blocking);
    assert_eq!(first.candidate_index, 0);
    assert_eq!(first.deadline_at_ms, NOW + 10 + 60_000);
    assert_eq!((first.claimed_at_ms, first.candidate_count), (NOW + 20, 1));
    let second = store::Queue::at(&pool, NOW + 30)
        .claim(Way::Api, &[], "runner-a")
        .await
        .unwrap()
        .expect("the background task");
    assert_eq!(second.id, background);
    let row = store::get(&pool, blocking).await.unwrap().unwrap();
    assert_eq!(row.status, "running");
    assert_eq!(row.runner.as_deref(), Some("runner-a"));
    assert_eq!(row.claimed_at_ms, Some(NOW + 20));
    assert!(
        store::Queue::at(&pool, NOW + 40)
            .claim(Way::Api, &[], "runner-a")
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn a_task_is_claimed_once_by_concurrent_runners() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    let wide = sqlx::postgres::PgPoolOptions::new()
        .max_connections(8)
        .connect_with(pool.connect_options().as_ref().clone())
        .await
        .unwrap();
    let id = store::submit(&wide, &request(false), &[api("openai")], NOW)
        .await
        .unwrap();
    let claims = (0..8).map(|i| {
        let pool = wide.clone();
        tokio::spawn(async move {
            store::Queue::at(&pool, NOW + 1)
                .claim(Way::Api, &[], &format!("runner-{i}"))
                .await
                .unwrap()
                .map(|c| c.id)
        })
    });
    let mut won = Vec::new();
    for claim in claims.collect::<Vec<_>>() {
        if let Some(claimed) = claim.await.unwrap() {
            won.push(claimed);
        }
    }
    assert_eq!(won, [id]);
}

#[tokio::test]
async fn login_claim_skips_tasks_whose_candidate_is_api_or_another_harness() {
    let db = TestDb::create().await;
    let pool = migrated(&db).await;
    store::submit(&pool, &request(true), &[api("openai")], NOW)
        .await
        .unwrap();
    store::submit(
        &pool,
        &request(true),
        &[login("anthropic", "claude")],
        NOW + 1,
    )
    .await
    .unwrap();
    let codex = store::submit(&pool, &request(false), &[login("openai", "codex")], NOW + 2)
        .await
        .unwrap();
    let claimed = store::Queue::at(&pool, NOW + 3)
        .claim(Way::Login, &["codex"], "r")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, codex);
    assert_eq!(claimed.candidate.harness.as_deref(), Some("codex"));
    assert!(
        store::Queue::at(&pool, NOW + 4)
            .claim(Way::Login, &["codex"], "r")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store::Queue::at(&pool, NOW + 4)
            .claim(Way::Login, &[], "r")
            .await
            .unwrap()
            .is_none()
    );
}
