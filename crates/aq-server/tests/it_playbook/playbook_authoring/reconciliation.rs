use super::*;

/// Finding 3: a save whose engine activation fails stays the desired state,
/// and every pass tries again until the engine has it.
#[tokio::test]
async fn an_activation_that_fails_is_tried_again_until_the_engine_has_the_artifact() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    notify_source(&host, example("source.md"), 1).await;
    // Long after start: the start-up pass has nothing to put back.
    assert_eq!(authoring::reinstall_active(&host).await.unwrap(), 0);
    let (run, task) = authored(&host).await;
    host.engine.lock().unwrap().refuses_activation = true;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    assert_eq!(status(&pool, run).await.0, "saved");
    let digest = active(&pool, NOTIFY)
        .await
        .unwrap()
        .unwrap()
        .artifact_digest;
    let deployed = || async {
        sqlx::query_scalar::<_, bool>("SELECT deployed FROM playbook_authoring_runs WHERE id=$1")
            .bind(run)
            .fetch_one(&pool)
            .await
            .unwrap()
    };
    assert!(!deployed().await);
    assert!(host.engine.lock().unwrap().activations.is_empty());
    // Still failing: nothing is given up.
    authoring::tick(&host).await.unwrap();
    assert!(!deployed().await);
    // Authoring the unchanged text starts no run, so only the retry helps.
    assert_eq!(authoring::author(&host, NOTIFY).await.unwrap(), None);
    host.engine.lock().unwrap().refuses_activation = false;
    authoring::tick(&host).await.unwrap();
    assert!(deployed().await);
    let activations = host.engine.lock().unwrap().activations.clone();
    assert_eq!(activations, [(BINDING.to_string(), digest)]);
    // Acknowledged: later passes leave the engine alone.
    authoring::tick(&host).await.unwrap();
    assert_eq!(host.engine.lock().unwrap().activations.len(), 1);
}

/// Finding 2: while a rollback is under way its target cannot be deleted, and
/// the rollback ends with exactly that run active.
#[tokio::test]
async fn a_rollback_holds_its_target_until_it_is_active() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    notify_source(&host, example("source.md"), 1).await;
    let (first, task) = authored(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    notify_source(&host, &reworded(2), 2).await;
    let (_, task) = authored(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    host.engine.lock().unwrap().delete_during_activation = Some(first);
    authoring::rollback(&host, NOTIFY).await.unwrap();
    assert_eq!(host.engine.lock().unwrap().delete_waited, Some(true));
    assert_eq!(active(&pool, NOTIFY).await.unwrap().unwrap().id, first);
    // A rollback whose engine activation fails changes nothing.
    notify_source(&host, &reworded(3), 3).await;
    let (third, task) = authored(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    host.engine.lock().unwrap().refuses_activation = true;
    assert!(authoring::rollback(&host, NOTIFY).await.is_err());
    assert_eq!(active(&pool, NOTIFY).await.unwrap().unwrap().id, third);
}

/// Findings 1 and 5: registration creates and never replaces, and a source
/// keeps its binding.
#[tokio::test]
async fn registering_an_existing_source_is_refused_and_keeps_its_document_and_binding() {
    let (_db, host, api) = api().await;
    let first = registration("The first document.\n");
    assert_eq!(api.call("POST", SOURCES, Some(first.clone())).await.0, 200);
    let revision = source_view(&host).await["revision"].as_u64().unwrap();
    let newer = json!({"markdown": "The owner's newer document.\n", "expected_revision": revision});
    let path = format!("{SOURCES}/{NOTIFY}/markdown");
    assert_eq!(api.call("PUT", &path, Some(newer)).await.0, 200);
    // A retry of the first registration does not bring the old text back.
    let (status, refused) = api.call("POST", SOURCES, Some(first)).await;
    assert_eq!(status, 409, "{refused}");
    let stored = authoring::stored_markdown(&host.pool, NOTIFY)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.0, "The owner's newer document.\n");
    // No path moves a source to another binding.
    let moved = SourceRecord {
        binding: "aq.elsewhere".into(),
        ..notify()
    };
    let refused = register(&host.pool, &moved, "The owner's newer document.\n").await;
    assert!(matches!(refused, Err(Error::Conflict(_))), "{refused:?}");
    let kept = authoring::get(&host.pool, NOTIFY).await.unwrap().unwrap();
    assert_eq!(kept.binding, BINDING);
}

/// Finding 3: routing turns on only once the engine has the routing
/// playbook, not when it is merely saved.
#[cfg(not(all(feature = "delivery_playbook", feature = "project_environments")))]
#[tokio::test]
async fn the_switch_needs_a_routing_playbook_the_engine_has_acknowledged() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    let on = || aq_server::routing::set_enabled(&pool, true);
    assert!(on().await.is_err(), "no routing source yet");
    let (run, task) = routing_like_source(&host).await;
    assert!(on().await.is_err(), "a run is not a playbook");
    // The save succeeds; the engine refuses the activation.
    host.engine.lock().unwrap().refuses_activation = true;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    assert_eq!(status(&pool, run).await.0, "saved");
    assert!(!active(&pool, "routing").await.unwrap().unwrap().deployed);
    assert!(on().await.is_err(), "saved but not in the engine");
    assert!(!aq_server::routing::enabled(&pool).await.unwrap());
    // The retry deploys it; now the switch turns on.
    host.engine.lock().unwrap().refuses_activation = false;
    authoring::tick(&host).await.unwrap();
    assert!(active(&pool, "routing").await.unwrap().unwrap().deployed);
    on().await.unwrap();
    assert!(aq_server::routing::enabled(&pool).await.unwrap());
}

/// Finding 1: a retry of an activation that a rollback has overtaken does
/// nothing: the engine stays on what the tables call active.
#[tokio::test]
async fn a_late_activation_retry_never_undoes_a_rollback() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    notify_source(&host, example("source.md"), 1).await;
    let (first, task) = authored(&host).await;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    // The second save is recorded, but the engine does not take it.
    notify_source(&host, &reworded(2), 2).await;
    let (second, task) = authored(&host).await;
    host.engine.lock().unwrap().refuses_activation = true;
    authoring::completed(&host, task, &answered(saved_outcome()))
        .await
        .unwrap();
    assert_eq!(active(&pool, NOTIFY).await.unwrap().unwrap().id, second);
    host.engine.lock().unwrap().refuses_activation = false;
    // The owner rolls back before any retry.
    authoring::rollback(&host, NOTIFY).await.unwrap();
    let now = active(&pool, NOTIFY).await.unwrap().unwrap();
    assert_eq!((now.id, now.deployed), (first, true));
    let rolled_back = host.engine.lock().unwrap().activations.clone();
    assert_eq!(rolled_back.last().unwrap().1, now.artifact_digest);
    // The retry that had picked the second run finds it no longer active.
    assert!(!authoring::deploy(&host, second).await.unwrap());
    authoring::tick(&host).await.unwrap();
    assert_eq!(host.engine.lock().unwrap().activations, rolled_back);
    // A run that is active and waiting is deployed once, under the same rule.
    assert!(!authoring::deploy(&host, first).await.unwrap());
}

/// Finding 2: a save whose authoring was missed is authored when the server
/// reconciles, and a run that ended is not repeated while nothing changed.
#[tokio::test]
async fn a_missed_save_is_authored_on_reconciliation_and_an_ended_run_is_not_repeated() {
    let (_db, host) = setup().await;
    let pool = host.pool.clone();
    notify_source(&host, example("source.md"), 1).await;
    // The owner saved while the engine was not open: nothing was authored.
    host.engine.lock().unwrap().down = true;
    assert!(authoring::author(&host, NOTIFY).await.is_err());
    assert!(authoring::author_missed(&host, NOTIFY).await.is_err());
    assert_eq!(runs(&pool).await, 0);
    // The engine opens: the reconciliation authors the document.
    host.engine.lock().unwrap().down = false;
    let run = authoring::author_missed(&host, NOTIFY).await.unwrap();
    let run = run.expect("the missed save is authored");
    // While it runs, and after it ends with a question, nothing is repeated.
    assert_eq!(authoring::author_missed(&host, NOTIFY).await.unwrap(), None);
    let task = in_flight(&pool).await.unwrap()[0].internal_task_id.unwrap();
    let question = json!({"kind": "question", "question": "How long?",
        "sentences": [{"line": 1, "text": "x"}]});
    authoring::completed(&host, task, &answered(question))
        .await
        .unwrap();
    assert_eq!(status(&pool, run).await.0, "question");
    assert_eq!(authoring::author_missed(&host, NOTIFY).await.unwrap(), None);
    assert_eq!(runs(&pool).await, 1);
    // The owner changes the document (or Settings change the facts).
    notify_source(&host, &reworded(2), 2).await;
    let next = authoring::author_missed(&host, NOTIFY).await.unwrap();
    assert!(next.is_some_and(|next| next > run));
}
