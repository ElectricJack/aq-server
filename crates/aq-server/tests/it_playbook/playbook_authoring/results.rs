use super::*;

#[tokio::test]
async fn a_save_whose_facts_changed_is_refused_and_authoring_starts_again() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    notify_source(&host, example("source.md"), 1).await;
    let (run, task) = authored(&host).await;
    // Settings change while the agent works.
    host.document(NOTIFY)
        .set(example("source.md"), 1, json!({"owner": "user:owner-2"}));
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    let refusal: String =
        sqlx::query_scalar("SELECT refusal FROM playbook_authoring_runs WHERE id=$1")
            .bind(run)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(status(&pool, run).await.0, "refused");
    assert_eq!(refusal, authoring::FACTS_CHANGED);
    assert!(active(&pool, NOTIFY).await.unwrap().is_none());
    assert!(host.engine.lock().unwrap().activations.is_empty());
    // Authoring started again for the new facts, with its task submitted.
    let again = in_flight(&pool).await.unwrap();
    assert_eq!(again.len(), 1);
    assert!(again[0].id > run);
    // It happens again: the restarts are capped (here at one), so it stops.
    host.document(NOTIFY)
        .set(example("source.md"), 1, json!({"owner": "user:owner-3"}));
    let task = again[0].internal_task_id.unwrap();
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    assert_eq!(status(&pool, again[0].id).await.0, "refused");
    assert!(in_flight(&pool).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_save_the_server_cannot_reproduce_is_refused_in_the_owners_words() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    notify_source(&host, example("source.md"), 1).await;
    let (run, task) = authored(&host).await;
    let mut outcome = saved_outcome();
    outcome["cases"][0]["expect"]["outcome"] = json!("no-such-outcome");
    authoring::completed(&host, task, &answered(outcome))
        .await
        .unwrap();
    assert_eq!(status(&pool, run).await.0, "refused");
    let view = source_view(&host).await;
    let refusal = view["latest"]["refusal"].as_str().unwrap();
    assert!(refusal.contains("does not run as it says"), "{refusal}");
    owner_text(refusal);
    assert_eq!(view["active"], Value::Null);
    assert!(host.engine.lock().unwrap().installed.is_empty());
    // An outcome that is not one at all fails the run.
    let (run, task) = authored(&host).await;
    authoring::completed(&host, task, &answered(json!({"kind": "saved"})))
        .await
        .unwrap();
    assert_eq!(status(&pool, run).await.0, "failed");
}

#[tokio::test]
async fn a_question_or_unable_changes_nothing_and_keeps_the_sentence() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    notify_source(&host, example("source.md"), 1).await;
    let lines: Vec<&str> = example("source.md").lines().collect();
    let (run, task) = authored(&host).await;
    // The agent's quote is a claim: the owner is shown the server's line.
    let question = json!({"kind": "question", "question": "How long is ten minutes?",
        "sentences": [{"line": 3, "end_line": 4, "text": "something the agent made up"}]});
    authoring::completed(&host, task, &answered(question))
        .await
        .unwrap();
    assert_eq!(status(&pool, run).await.0, "question");
    let view = source_view(&host).await;
    assert_eq!(view["latest"]["status"], "question");
    assert_eq!(
        view["latest"]["question"]["question"],
        "How long is ten minutes?"
    );
    let sentence = &view["latest"]["question"]["sentences"][0];
    assert_eq!(sentence["line"], 3);
    assert_eq!(sentence["text"], format!("{}\n{}", lines[2], lines[3]));
    assert!(view["latest"]["elapsed_ms"].as_i64().unwrap() >= 0);

    let (run, task) = authored(&host).await;
    let unable = json!({"kind": "unable", "reason": "Nothing can make a phone call.",
        "line": 1, "sentence": "not what line one says"});
    authoring::completed(&host, task, &answered(unable))
        .await
        .unwrap();
    assert_eq!(status(&pool, run).await.0, "unable");
    let view = source_view(&host).await;
    assert_eq!(view["latest"]["unable"]["sentence"], lines[0]);
    assert_eq!(view["latest"]["unable"]["line"], 1);
    assert_eq!(
        view["latest"]["unable"]["reason"],
        "Nothing can make a phone call."
    );

    // A fault in a host file is the server's, not the document's.
    let (run, task) = authored(&host).await;
    let unable = json!({"kind": "unable", "reason": "record.json allows a command the catalog lacks.",
        "file": "record.json"});
    authoring::completed(&host, task, &answered(unable))
        .await
        .unwrap();
    assert_eq!(status(&pool, run).await.0, "unable");
    let view = source_view(&host).await;
    assert_eq!(view["latest"]["unable"]["host_fault"], true);
    assert!(
        view["latest"]["unable"]["sentence"]
            .as_str()
            .unwrap()
            .contains("not in your document")
    );

    // A sentence that is not in the document fails the run.
    let (run, task) = authored(&host).await;
    let question = json!({"kind": "question", "question": "?",
        "sentences": [{"line": 4000, "text": "x"}]});
    authoring::completed(&host, task, &answered(question))
        .await
        .unwrap();
    assert_eq!(status(&pool, run).await.0, "failed");

    // None of it changed what is in effect.
    assert!(active(&pool, NOTIFY).await.unwrap().is_none());
    let engine = host.engine.lock().unwrap();
    assert!(engine.installed.is_empty() && engine.activations.is_empty());
}

#[tokio::test]
async fn a_failed_agent_fails_the_run_and_the_loop_records_what_ended() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    notify_source(&host, example("source.md"), 1).await;
    let (run, task) = authored(&host).await;
    // Nothing has ended: a pass changes nothing.
    assert_eq!(authoring::tick(&host).await.unwrap(), 1);
    assert_eq!(status(&pool, run).await.0, "running");
    // A fake agent runs the task and leaves a prepared outcome.
    let claimed = internal::Queue::at(&pool, clock_ms())
        .claim_login(&["claude"], true, "fake-agent")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(claimed.id, task);
    internal::Queue::at(&pool, clock_ms())
        .complete(
            internal::CandidateAttempt::new(task, 0),
            &saved_outcome(),
            None,
        )
        .await
        .unwrap();
    authoring::tick(&host).await.unwrap();
    assert_eq!(status(&pool, run).await.0, "saved");
    assert_eq!(host.engine.lock().unwrap().activations.len(), 1);
    assert_eq!(authoring::tick(&host).await.unwrap(), 0);

    // An agent that ends without an outcome: the run fails, the playbook stays.
    notify_source(&host, "Ask for a description.\n", 2).await;
    let (run, task) = authored(&host).await;
    let failed = aq_internal_work::Outcome::Failed {
        reason: aq_internal_work::Reason::InvalidOutput,
        detail: "the agent ended without saving, asking or giving up".into(),
        attempts: vec![],
    };
    authoring::completed(&host, task, &failed).await.unwrap();
    assert_eq!(status(&pool, run).await.0, "failed");
    let view = source_view(&host).await;
    assert!(
        view["latest"]["refusal"]
            .as_str()
            .unwrap()
            .contains("without saving")
    );
    assert!(view["active"]["artifact"].is_string());
    assert_eq!(host.engine.lock().unwrap().activations.len(), 1);
}

#[tokio::test]
async fn finished_runs_are_a_rolling_set_that_keeps_the_active_and_the_previous() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    notify_source(&host, example("source.md"), 1).await;
    let (first, task) = authored(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    notify_source(&host, &reworded(2), 2).await;
    let (second, task) = authored(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    // Six questions follow; the test host keeps three finished runs.
    for revision in 3..9 {
        notify_source(&host, &format!("Ask {revision} times.\n"), revision).await;
        let (_, task) = authored(&host).await;
        let question = json!({"kind": "question", "question": "How often?",
            "sentences": [{"line": 1, "text": "x"}]});
        authoring::completed(&host, task, &answered(question))
            .await
            .unwrap();
    }
    let kept: Vec<(i64, String)> =
        sqlx::query_as("SELECT id,status FROM playbook_authoring_runs ORDER BY id")
            .fetch_all(&pool)
            .await
            .unwrap();
    assert_eq!(kept.len(), 5, "{kept:?}");
    // The active run and the one a rollback brings back are never deleted.
    assert_eq!((kept[0].0, kept[1].0), (first, second));
    assert!(kept[2..].iter().all(|(_, status)| status == "question"));
    assert_eq!(active(&pool, NOTIFY).await.unwrap().unwrap().id, second);
    authoring::rollback(&host, NOTIFY).await.unwrap();
    assert_eq!(active(&pool, NOTIFY).await.unwrap().unwrap().id, first);
}

#[tokio::test]
async fn after_a_restart_the_active_playbooks_are_installed_and_activated_again() {
    let (_db, host) = setup().await;
    notify_source(&host, example("source.md"), 1).await;
    let (_, task) = authored(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    let digest = active(&host.pool, NOTIFY)
        .await
        .unwrap()
        .unwrap()
        .artifact_digest;
    // A new process with an engine that lost the artifact.
    *host.engine.lock().unwrap() = Engine::default();
    assert_eq!(authoring::reinstall_active(&host).await.unwrap(), 1);
    let engine = host.engine.lock().unwrap();
    assert!(engine.installed.contains(&digest));
    assert_eq!(engine.activations, [(BINDING.to_string(), digest)]);
    assert_eq!(engine.granted, 1);
}

#[test]
fn the_authoring_catalog_is_the_host9_registry() {
    use aq_server::playbooks::review_domain::CatalogProfile;
    let pinned = authoring::compiler::docs::catalog_registry();
    assert!(pinned == CatalogProfile::Host9.registry());
}
