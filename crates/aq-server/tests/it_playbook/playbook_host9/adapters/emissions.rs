use super::*;

#[tokio::test]
async fn an_unrouted_ready_task_emits_one_event_and_not_again_within_two_minutes() {
    let h = host(true, None).await;
    h.activated("sha256:routing-1").await;
    let task = h.graph.task("Waiting for a route", json!({}));
    h.wanted(task, 3, h.scope.project_id).await;
    assert_eq!(h.asked().await, 1);
    assert_eq!(h.events().await, 1);
    // The sealed event is the catalog's, for the task version.
    let page = h.service.outbox(&h.runtime, 0, 8).await.unwrap();
    let event = &page[0].2;
    assert_eq!(event.contract.name, "aq.work.route_needed");
    assert_eq!(
        event.payload,
        json!({"subject": {"id": task.to_string(), "revision": 3}})
    );
    assert_eq!(h.emission(task).await.unwrap().0, 3);
    // Not again while placement keeps seeing it wait, until two minutes passed.
    assert_eq!(h.asked().await, 0);
    h.later(119_000, task, 3).await;
    assert_eq!(h.asked().await, 0);
    h.later(1_000, task, 3).await;
    assert_eq!(h.asked().await, 1);
    assert_eq!(h.events().await, 2);
    // A task placement no longer sees waiting (routed, or not ready) is left alone.
    h.f.time
        .fetch_add(600_000, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(h.asked().await, 0);
}

#[tokio::test]
async fn a_new_task_version_emits_at_once() {
    let h = host(true, None).await;
    h.activated("sha256:routing-1").await;
    let task = h.graph.task("Edited while waiting", json!({}));
    h.wanted(task, 3, h.scope.project_id).await;
    assert_eq!(h.asked().await, 1);
    h.later(1_000, task, 4).await;
    assert_eq!(h.asked().await, 1);
    assert_eq!(
        (h.events().await, h.emission(task).await.unwrap().0),
        (2, 4)
    );
    assert_eq!(h.asked().await, 0);
}

#[tokio::test]
async fn a_task_nothing_can_take_is_not_asked_about_again_until_something_changes() {
    let h = host(true, None).await;
    h.activated("sha256:routing-1").await;
    let task = h.graph.task("No level for it", json!({}));
    h.wanted(task, 3, h.scope.project_id).await;
    assert_eq!(h.asked().await, 1);
    // The playbook's route is refused: Settings offer nothing at the level.
    let refused = h.route(task, 3, json!({"level": "deep-high"})).await;
    assert_eq!(refused.output["reason"], "not_offered");
    h.later(600_000, task, 3).await;
    assert_eq!(h.asked().await, 0);
    // Settings change: asked again, once.
    (h.routing.options.lock().unwrap().classes).insert(
        "deep-high".parse().unwrap(),
        vec![offer("anthropic", "claude")],
    );
    h.later(120_000, task, 3).await;
    assert_eq!(h.asked().await, 1);
    assert_eq!(h.emission(task).await.unwrap().1, None);
    // The document allows nobody: not again for Settings, only for a new artifact.
    let input = json!({"subject": {"id": task.to_string(), "revision": 3},
        "reasons": [{"code": "policy", "detail": "Never."}]});
    assert_eq!(
        h.run("never", "work.unroutable", input).await.outcome,
        "recorded"
    );
    h.routing.options.lock().unwrap().classes.clear();
    h.later(600_000, task, 3).await;
    assert_eq!(h.asked().await, 0);
    h.activated("sha256:routing-2").await;
    h.later(120_000, task, 3).await;
    assert_eq!(h.asked().await, 1);
    assert_eq!(h.events().await, 3);
}

#[tokio::test]
async fn nothing_is_emitted_without_an_active_routing_playbook_or_for_another_project() {
    // No routing artifact is active: there is nothing to route with.
    let h = host(true, None).await;
    let task = h.graph.task("Nobody to ask", json!({}));
    h.wanted(task, 3, h.scope.project_id).await;
    assert_eq!((h.asked().await, h.events().await), (0, 0));
    h.activated("sha256:routing-1").await;
    assert_eq!(h.asked().await, 1);
    // Without installation routing, only the namespace's project is asked about.
    let own = host(false, None).await;
    own.activated("sha256:routing-1").await;
    let elsewhere = own.graph.task("Another project", json!({}));
    own.wanted(elsewhere, 3, own.other_project).await;
    assert_eq!((own.asked().await, own.events().await), (0, 0));
    let wide = host(true, None).await;
    wide.activated("sha256:routing-1").await;
    wide.wanted(elsewhere, 3, wide.other_project).await;
    assert_eq!(wide.asked().await, 1);
}

/// Answers the first field `low` (or its first choice) and no flag.
// The whole path: placement records a waiting task, the server sends
// aq.work.route_needed, the event starts a run of the installed artifact
// that triggers on it (the engine's worked example, sealed here), and
// the run reads the task, asks the judgment and writes the route.
#[tokio::test]
async fn the_event_starts_a_run_of_an_installed_artifact_that_triggers_on_it() {
    use aq_playbooks_compiler::draft;
    use aq_server::playbooks::driver::Driver;
    let example = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../vendor/aq-playbooks-c744301/crates/aq-playbooks-compiler/authoring-examples/escalate-on-judgment");
    let workspace = draft::Workspace::read(&example).unwrap();
    let artifact = draft::seal(&workspace).unwrap();
    let h = host(true, Some(Arc::new(LowRisk))).await;
    (h.routing.options.lock().unwrap().classes).insert(
        "simple-high".parse().unwrap(),
        vec![offer("anthropic", "claude")],
    );
    h.service.retire_runtime(&h.runtime).await.unwrap();
    // The engine on protected PostgreSQL, in the fixture's database.
    let mut url = reqwest::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
    url.set_path(h.f.db.options().get_database().unwrap());
    let factory = move || {
        let config = url
            .as_str()
            .parse::<aq_playbook_host::postgres_client::Config>();
        let client = (config.unwrap()).connect(aq_playbook_host::postgres_client::NoTls);
        let keys = aq_playbook_host::postgres_storage::ProtectionKeyring::new(
            "host9",
            BTreeMap::from([("host9".into(), [9; 32])]),
        );
        aq_playbook_host::protected::storage(
            client.map_err(|_| aq_playbook_host::Error::Unknown)?,
            keys.unwrap(),
            std::time::Duration::from_secs(5),
            std::time::Duration::from_millis(500),
        )
    };
    let limits = aq_playbook_host::Limits::default();
    let driver = Driver::start(h.service.clone(), limits, factory)
        .await
        .unwrap();
    driver.install(artifact.clone()).await.unwrap();
    // Codex review 4: activated through the driver alone (as a configured
    // deployment or the authoring path does), with no ledger row: the
    // server's record of the engine binding is what the event source reads.
    let active = driver.activate("aq.routing".into(), artifact.digest.clone(), h.grant, 1);
    assert_eq!(active.await.unwrap()["artifact"], artifact.digest.as_str());
    let task = h
        .graph
        .task("Fix a typo in the install guide", json!({"aq:priority": 2}));
    h.wanted(task, 3, h.scope.project_id).await;
    for _ in 0..200 {
        driver.quantum().await.unwrap();
        if h.graph.stored(task).is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    h.assert_judged_route(task, &artifact.digest);
    // One cause, one run: more quanta start nothing and write nothing.
    for _ in 0..10 {
        driver.quantum().await.unwrap();
    }
    let runs = driver
        .inspect_runs(h.f.actor, Default::default())
        .await
        .unwrap();
    assert_eq!(runs.items.len(), 1);
    assert_eq!(h.routing.writes.lock().unwrap().len(), 1);
    driver.stop().await.unwrap();
}
