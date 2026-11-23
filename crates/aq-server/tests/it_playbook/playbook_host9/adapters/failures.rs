use super::*;

#[tokio::test]
async fn a_task_routing_cannot_route_is_raised_once_per_cause_and_again_for_a_new_version() {
    let h = host(true, None).await;
    let task = h.graph.task("Needs a level nobody offers", json!({}));
    let unoffered = json!({"level": "deep-high", "harnesses": ["claude"],
        "reasons": [{"code": "deep_reserved", "detail": "Hard analysis needs Deep."}]});
    assert_eq!(
        h.route(task, 3, unoffered.clone()).await.output["reason"],
        "not_offered"
    );
    let raised = h.raised(task).await;
    assert_eq!(raised.len(), 1);
    let (reason, cause, escalation, detail, project) = raised[0].clone();
    assert_eq!(
        (reason.as_str(), cause.as_str(), escalation.as_str()),
        ("no_candidate", "not_offered@3", "none")
    );
    assert_eq!(project, Some(h.scope.project_id));
    // In words the owner can act on, with the playbook's own reasons.
    assert!(
        detail.contains("chose level deep-high, harnesses claude"),
        "{detail}"
    );
    assert!(
        detail.contains("Settings > Intelligence") && detail.ends_with("Hard analysis needs Deep.")
    );
    // The same refusal again (another run, the same task version): not raised twice.
    assert_eq!(
        h.route(task, 3, unoffered.clone()).await.output["reason"],
        "not_offered"
    );
    h.f.time
        .fetch_add(600_000, std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        h.route(task, 3, unoffered.clone()).await.output["reason"],
        "not_offered"
    );
    assert_eq!(h.causes(task).await, ["not_offered@3"]);
    h.assert_escalation_is_idempotent(task, &unoffered).await;
    // A different cause for the same version is raised.
    let nobody = json!({"subject": {"id": task.to_string(), "revision": 3},
        "reasons": [{"code": "policy", "detail": "Never route payroll work."}]});
    assert_eq!(
        h.run("nobody", "work.unroutable", nobody).await.outcome,
        "recorded"
    );
    assert_eq!(h.causes(task).await, ["not_offered@3", "unroutable@3"]);
    let words = h.raised(task).await[1].3.clone();
    assert!(
        words.contains("allows no agent") && words.ends_with("Never route payroll work."),
        "{words}"
    );
    // A new task version: the same refusal is raised again, once.
    h.graph.nodes.lock().unwrap().get_mut(&task).unwrap()["version"] = json!(4);
    for _ in 0..2 {
        assert_eq!(
            h.route(task, 4, unoffered.clone()).await.output["reason"],
            "not_offered"
        );
    }
    let mut causes = h.causes(task).await;
    causes.sort();
    assert_eq!(causes, ["not_offered@3", "not_offered@4", "unroutable@3"]);
    // A route that is written raises nothing.
    let fine = h.graph.task("Routable", json!({}));
    assert_eq!(h.route(fine, 3, json!({})).await.outcome, "routed");
    assert!(h.raised(fine).await.is_empty());
}

#[tokio::test]
async fn a_task_unrouted_for_ten_minutes_is_raised_once_and_again_for_a_new_version() {
    let h = host(true, None).await;
    h.activated("sha256:routing-1").await;
    let task = h.graph.task("Its routing run never routes it", json!({}));
    h.wanted(task, 3, h.scope.project_id).await;
    assert_eq!(h.asked().await, 1);
    // Asked again every two minutes; nothing is raised before ten.
    for _ in 0..4 {
        h.later(120_000, task, 3).await;
        assert_eq!(h.asked().await, 1);
    }
    assert!(h.raised(task).await.is_empty());
    h.later(120_000, task, 3).await;
    h.asked().await;
    let raised = h.raised(task).await;
    assert_eq!(raised.len(), 1);
    assert_eq!(
        (raised[0].0.as_str(), raised[0].1.as_str()),
        ("deadline_exceeded", "unrouted@3")
    );
    assert!(
        raised[0].3.contains("more than 10 minutes"),
        "{}",
        raised[0].3
    );
    assert_eq!(raised[0].4, Some(h.scope.project_id));
    // Still waiting later: not raised twice for this version.
    for _ in 0..6 {
        h.later(120_000, task, 3).await;
        h.asked().await;
    }
    assert_eq!(h.causes(task).await, ["unrouted@3"]);
    // A new version waits its own ten minutes, then is raised once.
    h.later(1_000, task, 4).await;
    h.asked().await;
    assert_eq!(h.causes(task).await, ["unrouted@3"]);
    for _ in 0..6 {
        h.later(120_000, task, 4).await;
        h.asked().await;
    }
    assert_eq!(h.causes(task).await, ["unrouted@3", "unrouted@4"]);
    // A task already raised as not_offered is not also raised as waiting.
    let other = h.graph.task("Refused, then waiting", json!({}));
    h.wanted(other, 3, h.scope.project_id).await;
    h.asked().await;
    assert_eq!(
        h.route(other, 3, json!({"level": "deep-high"}))
            .await
            .output["reason"],
        "not_offered"
    );
    for _ in 0..7 {
        h.later(120_000, other, 3).await;
        h.asked().await;
    }
    assert_eq!(h.causes(other).await, ["not_offered@3"]);
}
// ------------------------------------------------ fix round (Codex review)

// Finding 6: a written route ends the waiting episode. Placement's last
// sighting of the task waiting stays fresh for minutes; it must not bring
// another event due once the task has its route.
#[tokio::test]
async fn a_routed_task_is_not_asked_about_again() {
    let h = host(true, None).await;
    h.activated("sha256:routing-1").await;
    let task = h.graph.task("Routed by its first run", json!({}));
    h.wanted(task, 3, h.scope.project_id).await;
    assert_eq!(h.asked().await, 1);
    assert_eq!(h.route(task, 3, json!({})).await.outcome, "routed");
    assert_eq!(h.waiting(task).await, None);
    for _ in 0..3 {
        h.f.time
            .fetch_add(120_000, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(h.asked().await, 0);
    }
    assert_eq!(h.events().await, 1);
    assert!(h.raised(task).await.is_empty());
}

// Finding 3: a run that read the task holds back a new ask only while it
// can still be running. Its end may never be readable (run history is a
// rolling record), so after the bound it no longer counts.
#[tokio::test]
async fn a_run_whose_end_is_not_on_record_stops_holding_back_the_ask() {
    let h = host(true, None).await;
    h.activated("sha256:routing-1").await;
    let task = h.graph.task("Its run left no history", json!({}));
    h.wanted(task, 3, h.scope.project_id).await;
    assert_eq!(h.asked().await, 1);
    // A run (not in any history) read the task and never routed it.
    assert_eq!(h.read(task, 3).await.outcome, "observed");
    for _ in 0..7 {
        h.later(120_000, task, 3).await;
        assert_eq!(h.asked().await, 0, "the run may still be running");
    }
    h.later(120_000, task, 3).await;
    assert_eq!(
        h.asked().await,
        1,
        "fifteen minutes on, it no longer holds the ask back"
    );
}

// Finding 5: a refusal is settled under the artifact that was asked. A
// run that began under an earlier artifact does not speak for the one
// activated since: that one is asked.
#[tokio::test]
async fn an_old_run_does_not_settle_the_task_for_a_newly_activated_artifact() {
    let h = host(true, None).await;
    h.activated("sha256:routing-1").await;
    let task = h.graph.task("Judged under the old document", json!({}));
    h.wanted(task, 3, h.scope.project_id).await;
    assert_eq!(h.asked().await, 1);
    // The owner's new document is activated while the old run is waiting.
    h.activated("sha256:routing-2").await;
    let input = json!({"subject": {"id": task.to_string(), "revision": 3},
        "reasons": [{"code": "policy", "detail": "The old document allowed nobody."}]});
    assert_eq!(
        h.run("old", "work.unroutable", input.clone()).await.outcome,
        "recorded"
    );
    let stamped: Option<String> =
        sqlx::query_scalar("SELECT artifact_digest FROM route_needed_emissions WHERE task_id=$1")
            .bind(task)
            .fetch_one(&h.f.db.pool().await)
            .await
            .unwrap();
    assert_eq!(stamped.as_deref(), Some("sha256:routing-1"));
    h.later(120_000, task, 3).await;
    assert_eq!(
        h.asked().await,
        1,
        "the new artifact is asked about the unchanged task"
    );
    // Its own refusal settles the task for it.
    assert_eq!(
        h.run("new", "work.unroutable", input).await.outcome,
        "recorded"
    );
    h.later(600_000, task, 3).await;
    assert_eq!(h.asked().await, 0);
    // And a route records the artifact that was asked, not a later one.
    let other = h
        .graph
        .task("Routed by a run of the old document", json!({}));
    h.wanted(other, 3, h.scope.project_id).await;
    assert_eq!(h.asked().await, 1);
    h.activated("sha256:routing-3").await;
    assert_eq!(h.route(other, 3, json!({})).await.outcome, "routed");
    let (_, route) = h.routing.writes.lock().unwrap().last().unwrap().clone();
    assert_eq!(
        route.unwrap().policy_digest.as_deref(),
        Some("sha256:routing-2")
    );
}

// Finding 2: the route is written before the intent's result is recorded.
// If the server stops between the two, the intent is dispatched again:
// the task has moved to its next version, so the repeat is `stale` and
// writes nothing more. The route is written once.
#[tokio::test]
async fn a_route_write_repeated_after_a_crash_is_stale_and_writes_nothing_more() {
    let h = host(true, None).await;
    let task = h.graph.task("Written, then the server stopped", json!({}));
    // The write of the lost dispatch, as the adapter makes it.
    let lost = aq_server::playbooks::domain::host9::route_from_input(
        &json!({"level": "standard-high", "reasons": []}),
        &h.routing.options.lock().unwrap().clone(),
        Default::default(),
    );
    let at = (h.scope.workspace_id, task, 3);
    assert!(matches!(
        h.routing.write(at, lost.as_ref()).await.unwrap(),
        Written::Routed { .. }
    ));
    let repeat = h.route(task, 3, json!({})).await;
    assert_eq!(repeat.outcome, "stale");
    assert_eq!(h.routing.writes.lock().unwrap().len(), 1);
    assert_eq!(h.graph.stored(task).unwrap().n, 1);
    assert!(h.raised(task).await.is_empty());
}
