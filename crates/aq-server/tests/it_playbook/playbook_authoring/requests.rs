use super::*;

#[tokio::test]
async fn a_save_of_unchanged_text_and_facts_starts_no_run() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    ensure_builtin(&pool, &routing()).await.unwrap();
    host.document("routing").set(POLICY, 3, options());
    let first = requested(&host, "routing")
        .await
        .unwrap()
        .expect("first run");
    assert_eq!(status(&pool, first).await, ("running".into(), None));
    // Nothing is active yet, so the same text asks again, and gets the run
    // already running for it.
    let again = requested(&host, "routing")
        .await
        .unwrap()
        .expect("no active run yet");
    assert_eq!(again, first);
    assert_eq!(runs(&pool).await, 1);
    saved(&pool, again, "sha256:first", true).await;

    let active_run = active(&pool, "routing").await.unwrap().expect("active");
    assert_eq!(active_run.id, again);
    assert_eq!(active_run.source_revision, 3);
    assert_eq!(active_run.source_digest, authoring::source_digest(POLICY));
    assert_eq!(active_run.artifact_digest, "sha256:first");
    let before = runs(&pool).await;

    assert_eq!(requested(&host, "routing").await.unwrap(), None);
    // A later structure revision with the same text and facts (a description
    // edit elsewhere in the document) starts nothing either.
    host.document("routing").set(POLICY, 4, options());
    assert_eq!(requested(&host, "routing").await.unwrap(), None);
    assert_eq!(runs(&pool).await, before);

    // Changed facts start a run; the active one stays.
    host.document("routing")
        .set(POLICY, 5, json!({"routing_options": {"classes": {}}}));
    let changed = requested(&host, "routing")
        .await
        .unwrap()
        .expect("facts changed");
    assert_eq!(status(&pool, changed).await.0, "running");
    assert_eq!(active(&pool, "routing").await.unwrap().unwrap().id, again);

    // A source nobody recorded is not found.
    let missing = requested(&host, "nothing").await;
    assert!(matches!(missing, Err(Error::NotFound(_))), "{missing:?}");
}

#[tokio::test]
async fn a_newer_request_supersedes_a_running_one() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    ensure_builtin(&pool, &routing()).await.unwrap();
    host.document("routing").set(POLICY, 1, options());
    let first = requested(&host, "routing").await.unwrap().unwrap();
    saved(&pool, first, "sha256:first", true).await;

    host.document("routing")
        .set("Research is Standard.", 2, options());
    let second = requested(&host, "routing").await.unwrap().unwrap();
    host.document("routing")
        .set("Research is Complex.", 3, options());
    let third = requested(&host, "routing").await.unwrap().unwrap();
    assert!(first < second && second < third);

    let (second_status, finished) = status(&pool, second).await;
    assert_eq!(second_status, "superseded");
    assert!(finished.is_some());
    assert_eq!(status(&pool, third).await, ("running".into(), None));
    // A saved run is not running, so it is never superseded: it stays active.
    assert_eq!(status(&pool, first).await.0, "saved");
    assert_eq!(active(&pool, "routing").await.unwrap().unwrap().id, first);

    // Only older runs of the same source are superseded.
    assert_eq!(supersede_older(&pool, "routing", third).await.unwrap(), 0);
    assert_eq!(supersede_older(&pool, "routing", second).await.unwrap(), 0);
    ensure_builtin(
        &pool,
        &SourceRecord {
            id: "other".into(),
            binding: "aq.other".into(),
            ..routing()
        },
    )
    .await
    .unwrap();
    host.document("other").set(POLICY, 1, options());
    let other = requested(&host, "other").await.unwrap().unwrap();
    assert_eq!(status(&pool, third).await.0, "running");
    assert_eq!(
        supersede_older(&pool, "routing", other + 1).await.unwrap(),
        1
    );
    assert_eq!(status(&pool, third).await.0, "superseded");
    assert_eq!(status(&pool, other).await.0, "running");
}
