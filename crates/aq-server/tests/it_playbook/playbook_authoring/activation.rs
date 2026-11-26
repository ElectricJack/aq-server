use super::*;

#[tokio::test]
async fn submitting_records_the_internal_task_on_the_run() {
    let (_db, host) = setup().await;
    notify_source(&host, example("source.md"), 1).await;
    let (run, task) = authored(&host).await;
    let row = internal::get(&host.pool, task).await.unwrap().unwrap();
    assert_eq!(row.status, "queued");
    assert_eq!(row.workspace_kind, "agent");
    assert_eq!(row.purpose, Purpose::PlaybookCompilation);
    assert_eq!(
        (row.affected_kind.as_str(), row.affected_id.as_str()),
        ("playbook_source", NOTIFY)
    );
    let agent = row.request.agent.unwrap();
    assert_eq!(agent.files["source.md"], example("source.md"));
    for path in [
        "GUIDE.md",
        "LANGUAGE.md",
        "CATALOG.md",
        "record.json",
        "catalog.json",
    ] {
        assert!(agent.files.contains_key(path), "{path}");
    }
    assert!(!agent.files.contains_key("previous.artifact.json"));
    // The grant exists before the run starts (ruling R2).
    assert_eq!(host.engine.lock().unwrap().granted, 1);
    // Asking again while it runs starts nothing new.
    assert_eq!(authored(&host).await, (run, task));
    assert_eq!(runs(&host.pool).await, 1);
    assert_eq!(source_view(&host).await["latest"]["status"], "running");
}

#[tokio::test]
async fn a_saved_playbook_is_installed_and_activated_and_the_previous_kept_for_rollback() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    notify_source(&host, example("source.md"), 1).await;
    let (first, task) = authored(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    assert_eq!(status(&pool, first).await.0, "saved");
    let one = active(&pool, NOTIFY).await.unwrap().unwrap();
    assert_eq!((one.id, one.previous_digest.clone()), (first, None));
    // The digest is the server's own seal, not the agent's claim.
    assert_ne!(one.artifact_digest, format!("sha256:{}", "0".repeat(64)));
    {
        let engine = host.engine.lock().unwrap();
        assert!(engine.installed.contains(&one.artifact_digest));
        assert_eq!(
            engine.activations,
            [(BINDING.to_string(), one.artifact_digest.clone())]
        );
    }
    let view = source_view(&host).await;
    assert_eq!(view["active"]["can_rollback"], false);
    assert_eq!(view["active"]["comparison"], Value::Null);
    let cases = view["active"]["cases"].as_array().unwrap();
    assert_eq!(cases.len(), example_cases().len());
    for case in cases {
        assert_eq!(case["passed"], true);
        owner_text(case["result"].as_str().unwrap());
    }
    // Unchanged text and facts: nothing to author.
    assert_eq!(authoring::author(&host, NOTIFY).await.unwrap(), None);

    // The owner rewords the document; the second agent is shown the first playbook.
    let reworded = format!("{}\nThank you.\n", example("source.md"));
    notify_source(&host, &reworded, 2).await;
    let (second, task) = authored(&host).await;
    let files = internal::get(&pool, task)
        .await
        .unwrap()
        .unwrap()
        .request
        .agent
        .unwrap()
        .files;
    assert!(files["previous.artifact.json"].contains(&one.artifact_digest));
    assert_eq!(files["previous/source.md"], example("source.md"));
    assert!(files.contains_key("previous/draft.json"));
    let previous_cases = files
        .keys()
        .filter(|p| p.starts_with("previous/cases/"))
        .count();
    assert_eq!(previous_cases, example_cases().len());
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    let two = active(&pool, NOTIFY).await.unwrap().unwrap();
    assert_eq!(two.id, second);
    assert_ne!(two.artifact_digest, one.artifact_digest);
    assert_eq!(
        two.previous_digest.as_deref(),
        Some(one.artifact_digest.as_str())
    );
    let view = source_view(&host).await;
    assert_eq!(view["active"]["can_rollback"], true);
    assert_eq!(view["active"]["comparison"], json!({"change": "wording"}));
    assert!(view["active"]["previous"]["since_ms"].as_i64().unwrap() > 0);
    // Recording the same result twice changes nothing.
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    assert_eq!(host.engine.lock().unwrap().activations.len(), 2);
}

#[tokio::test]
async fn rollback_reactivates_the_previous_artifact() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    notify_source(&host, example("source.md"), 1).await;
    // Nothing to go back to yet.
    assert!(matches!(
        authoring::rollback(&host, NOTIFY).await,
        Err(Error::Conflict(_))
    ));
    let (first, task) = authored(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    assert!(matches!(
        authoring::rollback(&host, NOTIFY).await,
        Err(Error::Conflict(_))
    ));
    notify_source(&host, &reworded(2), 2).await;
    let (second, task) = authored(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    // A third version is being authored when the owner rolls back.
    notify_source(&host, &reworded(3), 3).await;
    let (third, _) = authored(&host).await;

    authoring::rollback(&host, NOTIFY).await.unwrap();
    let now = active(&pool, NOTIFY).await.unwrap().unwrap();
    assert_eq!(now.id, first);
    let last = host
        .engine
        .lock()
        .unwrap()
        .activations
        .last()
        .cloned()
        .unwrap();
    assert_eq!(last, (BINDING.to_string(), now.artifact_digest.clone()));
    // The run in flight is superseded: the owner chose the previous playbook.
    assert_eq!(status(&pool, third).await.0, "superseded");
    assert_eq!(status(&pool, second).await.0, "saved");
    let view = source_view(&host).await;
    assert_eq!(view["active"]["artifact"], now.artifact_digest);
    assert_eq!(view["active"]["can_rollback"], false);
    assert_eq!(view["latest"]["status"], "superseded");
    // A later save of the document starts a new run.
    let (fourth, _) = authored(&host).await;
    assert!(fourth > third);
}

#[tokio::test]
async fn a_save_from_a_superseded_run_is_recorded_and_never_activated() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    notify_source(&host, example("source.md"), 1).await;
    let (older, older_task) = authored(&host).await;
    // The owner saves again before the first agent has finished.
    notify_source(&host, "Ask for a description.\n", 2).await;
    let (newer, _) = authored(&host).await;
    assert_ne!(older, newer);
    authoring::completed(&host, older_task, &answered(saved_outcome()))
        .await
        .unwrap();
    assert_eq!(status(&pool, older).await.0, "superseded");
    assert_eq!(status(&pool, newer).await.0, "running");
    assert!(active(&pool, NOTIFY).await.unwrap().is_none());
    let engine = host.engine.lock().unwrap();
    assert!(engine.activations.is_empty() && engine.installed.is_empty());
}
