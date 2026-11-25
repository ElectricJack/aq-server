use super::*;

#[tokio::test]
async fn a_changed_record_alone_starts_a_run() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    ensure_builtin(&pool, &routing()).await.unwrap();
    host.document("routing").set(POLICY, 1, options());
    let first = requested(&host, "routing").await.unwrap().unwrap();
    saved(&pool, first, "sha256:first", true).await;
    assert_eq!(requested(&host, "routing").await.unwrap(), None);
    let active_run = active(&pool, "routing").await.unwrap().unwrap();
    assert_eq!(
        active_run.record_digest,
        authoring::record_digest(&routing())
    );

    // A newer server widens the built-in record: same text, same facts.
    let wider = SourceRecord {
        commands: names(&["work.read", "work.route", "judgment.assess", "work.note"]),
        ..routing()
    };
    ensure_builtin(&pool, &wider).await.unwrap();
    let second = requested(&host, "routing")
        .await
        .unwrap()
        .expect("record changed");
    assert_eq!(status(&pool, second).await.0, "running");

    // A registered source given fewer commands is authored again too.
    register(&pool, &triage("aq.triage"), "Label new tasks.")
        .await
        .unwrap();
    host.document("triage")
        .set("Label new tasks.", 1, json!({}));
    let label = requested(&host, "triage").await.unwrap().unwrap();
    saved(&pool, label, "sha256:label", true).await;
    let narrower = SourceRecord {
        commands: names(&["work.read"]),
        ..triage("aq.triage")
    };
    register(&pool, &narrower, "Label new tasks.")
        .await
        .unwrap();
    // The record revision is the registered Markdown's revision.
    host.document("triage")
        .set("Label new tasks.", 2, json!({}));
    assert!(requested(&host, "triage").await.unwrap().is_some());
}

#[tokio::test]
async fn reverting_the_text_while_a_run_is_in_flight_supersedes_it() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    ensure_builtin(&pool, &routing()).await.unwrap();
    host.document("routing").set(POLICY, 1, options());
    let first = requested(&host, "routing").await.unwrap().unwrap();
    saved(&pool, first, "sha256:first", true).await;

    host.document("routing")
        .set("Research is Complex.", 2, options());
    let second = requested(&host, "routing").await.unwrap().unwrap();
    // The owner reverts to the active text before the second run finishes.
    host.document("routing").set(POLICY, 3, options());
    assert_eq!(requested(&host, "routing").await.unwrap(), None);
    assert_eq!(status(&pool, second).await.0, "superseded");

    // The second run's late result is not recorded and activates nothing.
    assert!(
        !finish(&pool, second, &save("sha256:second", true))
            .await
            .unwrap()
    );
    assert_eq!(active(&pool, "routing").await.unwrap().unwrap().id, first);
}

#[tokio::test]
async fn a_superseded_run_never_becomes_active() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    ensure_builtin(&pool, &routing()).await.unwrap();
    host.document("routing").set(POLICY, 1, options());
    let first = requested(&host, "routing").await.unwrap().unwrap();
    saved(&pool, first, "sha256:first", true).await;
    host.document("routing")
        .set("Research is Standard.", 2, options());
    let older = requested(&host, "routing").await.unwrap().unwrap();
    host.document("routing")
        .set("Research is Complex.", 3, options());
    let newer = requested(&host, "routing").await.unwrap().unwrap();

    // The newer result arrives first and takes effect in place of the first.
    assert!(
        finish(&pool, newer, &save("sha256:newer", true))
            .await
            .unwrap()
    );
    let now_active = active(&pool, "routing").await.unwrap().unwrap();
    assert_eq!(
        (now_active.id, now_active.artifact_digest.as_str()),
        (newer, "sha256:newer")
    );
    let first_active: bool =
        sqlx::query_scalar("SELECT active FROM playbook_authoring_runs WHERE id=$1")
            .bind(first)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!first_active);
    assert_eq!(status(&pool, first).await.0, "saved");

    // The superseded run's result, in any order, records nothing.
    for ended in [save("sha256:older", true), Finished::default()] {
        assert!(!finish(&pool, older, &ended).await.unwrap());
    }
    assert_eq!(status(&pool, older).await.0, "superseded");
    assert_eq!(active(&pool, "routing").await.unwrap().unwrap().id, newer);
    // And the newer run's result is recorded once.
    assert!(!finish(&pool, newer, &Finished::default()).await.unwrap());
    assert_eq!(status(&pool, newer).await.0, "saved");

    // Only a saved run with its artifact can be recorded as saved or active.
    host.document("routing")
        .set("Research is Trivial.", 4, options());
    let last = requested(&host, "routing").await.unwrap().unwrap();
    let not_saved = Finished {
        activate: true,
        ..Finished::default()
    };
    let wrong = finish(&pool, last, &not_saved).await;
    assert!(matches!(wrong, Err(Error::Invalid(_))), "{wrong:?}");
    let no_artifact = Finished {
        status: Ended::Saved,
        ..Finished::default()
    };
    let wrong = finish(&pool, last, &no_artifact).await;
    assert!(matches!(wrong, Err(Error::Invalid(_))), "{wrong:?}");
    assert_eq!(status(&pool, last).await.0, "running");
}

#[tokio::test]
async fn a_request_that_read_older_text_records_nothing() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    ensure_builtin(&pool, &routing()).await.unwrap();
    host.document("routing")
        .set("Research is Complex.", 5, options());
    let newer = requested(&host, "routing").await.unwrap().unwrap();
    // A request that read revision 4 before revision 5 was saved arrives late.
    host.document("routing").set(POLICY, 4, options());
    assert_eq!(requested(&host, "routing").await.unwrap(), None);
    assert_eq!(status(&pool, newer).await.0, "running");
    assert_eq!(runs(&pool).await, 1);
}

#[tokio::test]
async fn concurrent_requests_with_a_host_that_reads_the_pool_all_finish() {
    let (_db, host) = setup().await;
    // The test pool has two connections; the host reads through it too.
    let host = Host {
        reads_pool: true,
        engine: Arc::default(),
        stored: false,
        ..host
    };
    let pool = host.pool.clone();
    ensure_builtin(&pool, &routing()).await.unwrap();
    host.document("routing").set(POLICY, 1, options());
    let requests = (0..6).map(|_| {
        let host = host.clone();
        tokio::spawn(async move { requested(&host, "routing").await })
    });
    let all = futures_util::future::join_all(requests);
    let all = tokio::time::timeout(Duration::from_secs(20), all)
        .await
        .expect("no request waits on the pool");
    let ids: BTreeSet<i64> = (all.into_iter())
        .map(|r| r.unwrap().unwrap().expect("a run"))
        .collect();
    // One request recorded the run; the others found it running.
    assert_eq!(ids.len(), 1, "{ids:?}");
    assert_eq!(runs(&pool).await, 1);
}
