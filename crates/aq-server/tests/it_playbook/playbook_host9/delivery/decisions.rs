//! Catalog 2.3.0 decision commands (Epic 0, spec 0016 section 6.2):
//! `work.complete`, `work.return`, `work.hold` and `supervisor.notify`. The
//! server's settlement is the fake [`super::Settle`]; its fences and the
//! Graph are tested with a recorded result in
//! `it_profiled::profiled_results::auto_settlement56::playbook`.
use super::{D, delivery};
use aq_server::playbooks::{
    domain::{Error, delivery_decisions::Settled},
    host_adapters::HostResult,
};
use serde_json::{Value, json};
use uuid::Uuid;

type Decision = (String, Option<String>, Option<String>, Option<bool>);

fn decide(task: Uuid, result: Uuid) -> Value {
    json!({"task": task.to_string(), "result_id": result.to_string()})
}
fn back(task: Uuid, result: Uuid, reason: &str, detail: Option<&str>, update: bool) -> Value {
    let mut input = json!({"task": task.to_string(), "result_id": result.to_string(),
        "reason": reason, "update_from_main": update});
    if let Some(detail) = detail {
        input["detail"] = json!(detail);
    }
    input
}
fn hold(task: Uuid, result: Uuid, reason: &str) -> Value {
    json!({"task": task.to_string(), "result_id": result.to_string(), "reason": reason})
}
fn refused(reason: &str) -> HostResult {
    HostResult {
        outcome: "refused".into(),
        output: json!({"reason": reason}),
    }
}
fn empty(outcome: &str) -> HostResult {
    HostResult {
        outcome: outcome.into(),
        output: json!({}),
    }
}

impl D {
    /// The task's next attempt recorded a newer result; the earlier one stays known.
    fn next_result(&self, task: Uuid) -> Uuid {
        let mut latest = self.port.latest.lock().unwrap();
        let latest = latest.get_mut(&task).unwrap();
        latest.result_id = Uuid::new_v4();
        latest.attempt_id = Uuid::new_v4();
        (self.port.known.lock().unwrap()).insert((task, latest.result_id));
        latest.result_id
    }
    /// A replacement report from the same attempt, with its graph and Git facts unchanged.
    pub(super) fn supersede_report(&self, task: Uuid) -> Uuid {
        let mut latest = self.port.latest.lock().unwrap();
        let latest = latest.get_mut(&task).unwrap();
        latest.result_id = Uuid::new_v4();
        (self.port.known.lock().unwrap()).insert((task, latest.result_id));
        latest.result_id
    }

    async fn decision(&self, result: Uuid) -> Option<Decision> {
        let sql = "SELECT decision,reason,detail,update_from_main FROM delivery_decisions WHERE result_id=$1";
        let query = sqlx::query_as(sql).bind(result);
        query.fetch_optional(&self.pool().await).await.unwrap()
    }
    async fn returns(&self, task: Uuid) -> Value {
        self.read(task).await.output["facts"]["returns"].clone()
    }
    fn settled(&self) -> Vec<(String, Uuid)> {
        self.settle.calls.lock().unwrap().clone()
    }
    fn answers(&self, answer: Settled) {
        *self.settle.answer.lock().unwrap() = answer;
    }
    async fn messages(&self, body: &str) -> i64 {
        let sql = r#"SELECT count(*) FROM mailbox_messages WHERE body=$1 AND sender='{"kind":"system","component":"internal_work"}'::jsonb"#;
        let query = sqlx::query_scalar(sql).bind(body);
        query.fetch_one(&self.pool().await).await.unwrap()
    }
}

#[tokio::test]
async fn the_delivery_binding_does_not_accept_manual_result_ids() {
    let h = delivery(false).await;
    let request = super::PrepareRequest {
        request_id: Uuid::new_v4(),
        grant_id: h.grant,
        grant_revision: 1,
        binding: "aq.delivery".into(),
        expected_activation_revision: 1,
        entry: "main".into(),
        input: json!({}),
    };
    assert!(matches!(
        h.service.prepare(h.f.actor, request).await,
        Err(Error::Forbidden)
    ));
}

#[tokio::test]
async fn work_complete_completes_the_latest_result_and_replays_its_first_answer() {
    let h = delivery(false).await;
    h.activated().await;
    let (task, p) = h.stopped(h.scope.project_id, "success");
    assert_eq!(h.asked().await, 1);
    let input = decide(task, p.result_id);
    let done = h.run("complete-1", "work.complete", input.clone()).await;
    assert_eq!(done, empty("completed"));
    assert_eq!(h.settled(), [("complete".to_string(), p.result_id)]);
    let decided = h.decision(p.result_id).await.unwrap();
    assert_eq!(decided, ("completed".into(), None, None, None));
    // A replay of the intent is its first answer; nothing is settled again.
    assert_eq!(
        h.run("complete-1", "work.complete", input.clone()).await,
        done
    );
    // So is another run's call on the completed result.
    let other = h.run("complete-2", "work.complete", input.clone()).await;
    assert_eq!(other, done);
    assert_eq!(h.settled().len(), 1);
    // The same intent with other input is a conflict, never a second effect.
    let changed = h.intent("complete-1", "work.complete", decide(task, Uuid::new_v4()));
    assert!(matches!(h.try_run(&changed).await, Err(Error::Conflict)));
    // A decided result is never announced again.
    h.later(10_000_000);
    assert_eq!(h.asked().await, 0);
}

#[tokio::test]
async fn work_complete_refuses_a_stale_unknown_or_fenced_result_and_changes_nothing() {
    let h = delivery(false).await;
    let (task, first) = h.stopped(h.scope.project_id, "success");
    let next = h.next_result(task);
    // Not the task's latest result: the newer one has its own run.
    let stale = h.run("stale", "work.complete", decide(task, first.result_id));
    assert_eq!(stale.await, refused("stale_result"));
    let unknown = h.run("unknown", "work.complete", decide(task, Uuid::new_v4()));
    assert_eq!(unknown.await, refused("unknown_result"));
    let odd = json!({"task": task.to_string(), "result_id": "not-a-result"});
    assert_eq!(
        h.run("odd-result", "work.complete", odd).await,
        refused("unknown_result")
    );
    let missing = h.run("missing", "work.complete", decide(Uuid::new_v4(), next));
    assert_eq!(missing.await, refused("unknown_task"));
    let odd = json!({"task": "not-a-task", "result_id": next.to_string()});
    assert_eq!(
        h.run("odd-task", "work.complete", odd).await,
        refused("unknown_task")
    );
    // A task of another project: not this namespace's to decide on.
    let (elsewhere, theirs) = h.stopped(h.other_project, "success");
    let foreign = h.run(
        "foreign",
        "work.complete",
        decide(elsewhere, theirs.result_id),
    );
    assert_eq!(foreign.await, refused("unknown_task"));
    assert!(h.settled().is_empty());
    // The settlement's fences (current attempt, report head, stop proof):
    // a result the attempt moved past is refused, and no decision is recorded.
    h.answers(Settled::Stale);
    let fenced = h.run("fenced", "work.complete", decide(task, next));
    assert_eq!(fenced.await, refused("stale_result"));
    assert_eq!(h.settled(), [("complete".to_string(), next)]);
    for result in [first.result_id, next] {
        assert_eq!(h.decision(result).await, None);
    }
}

#[tokio::test]
async fn installation_routing_decides_on_every_bound_project() {
    let h = delivery(true).await;
    let (elsewhere, theirs) = h.stopped(h.other_project, "success");
    let done = h.run(
        "foreign",
        "work.complete",
        decide(elsewhere, theirs.result_id),
    );
    assert_eq!(done.await, empty("completed"));
}

#[tokio::test]
async fn a_lost_settlement_answer_leaves_work_complete_pending_until_a_replay() {
    let h = delivery(false).await;
    let (task, p) = h.stopped(h.scope.project_id, "success");
    h.answers(Settled::Pending);
    let intent = h.intent("complete-lost", "work.complete", decide(task, p.result_id));
    assert!(matches!(h.try_run(&intent).await, Err(Error::Unknown)));
    assert_eq!(h.decision(p.result_id).await, None);
    // The replay resumes the pending intent: the same settlement again.
    h.answers(Settled::Completed);
    assert_eq!(h.try_run(&intent).await.unwrap(), empty("completed"));
    let calls = h.settled();
    assert_eq!(calls, vec![("complete".to_string(), p.result_id); 2]);
    assert_eq!(h.decision(p.result_id).await.unwrap().0, "completed");
}

#[tokio::test]
async fn the_driver_resumes_an_admitted_return_after_restart_and_deadline() {
    use aq_playbook_authoring::{builtin, request};
    use aq_playbooks_compiler::draft;
    use aq_server::playbooks::driver::Driver;
    use std::{collections::BTreeMap, time::Duration};

    let h = delivery(false).await;
    h.service.retire_runtime(&h.runtime).await.unwrap();
    h.answers(Settled::Pending);
    let (task, pending) = h.stopped(h.scope.project_id, "failed");
    let source = builtin::delivery();
    let catalog = request::pinned_catalog(&source).unwrap();
    let mut draft_value: Value = serde_json::from_str(include_str!(
        "../../fixtures/delivery_reads_the_task.draft.json"
    ))
    .unwrap();
    let step = &mut draft_value["steps"]["hold"];
    step["op"]["command"] = json!("work.return");
    step["op"]["input"]["fields"]["update_from_main"] = json!({"op":"literal","value":false});
    step["edges"] = json!({"returned":"delivered", "refused":"failed"});
    let artifact = draft::seal(&draft::Workspace {
        markdown: "Return an already stopped result to the queue.".into(),
        record: request::engine_record(&source, "delivery-replay"),
        catalog: request::catalog_for(&source, &catalog).unwrap(),
        facts: BTreeMap::new(),
        draft: serde_json::from_value(draft_value).unwrap(),
    })
    .unwrap();
    let mut url = reqwest::Url::parse(&std::env::var("DATABASE_URL").unwrap()).unwrap();
    url.set_path(h.f.db.options().get_database().unwrap());
    let factory = move || {
        use aq_playbook_host::{
            Error, postgres_client as pg, postgres_storage as store, protected,
        };
        let config: pg::Config = url.as_str().parse().unwrap();
        let client = config.connect(pg::NoTls).map_err(|_| Error::Unknown)?;
        let keys =
            store::ProtectionKeyring::new("test", BTreeMap::from([("test".into(), [9; 32])]))
                .unwrap();
        protected::storage(
            client,
            keys,
            Duration::from_secs(5),
            Duration::from_millis(500),
        )
    };
    let driver = Driver::start(h.service.clone(), Default::default(), factory.clone())
        .await
        .unwrap();
    driver.install(artifact.clone()).await.unwrap();
    driver
        .activate(source.binding, artifact.digest, h.grant, 1)
        .await
        .unwrap();
    for _ in 0..20 {
        driver.quantum().await.unwrap();
        if !h.settled().is_empty() {
            break;
        }
    }
    let pool = h.pool().await;
    let retained = || {
        sqlx::query_as::<_, (String, Uuid, String)>(
        "SELECT intent_id,operation_id,status FROM playbook_effects WHERE command_name='work.return'",
    ).fetch_one(&pool)
    };
    let (intent, operation, status) = retained().await.unwrap();
    assert_eq!(status, "pending");
    assert_eq!(h.decision(pending.result_id).await, None);
    driver.stop().await.unwrap();

    h.later(15_000);
    h.answers(Settled::Returned);
    let driver = Driver::start(h.service.clone(), Default::default(), factory)
        .await
        .unwrap();
    driver.recover().await.unwrap();
    for _ in 0..8 {
        driver.quantum().await.unwrap();
    }
    assert_eq!(
        retained().await.unwrap(),
        (intent, operation, "returned".into())
    );
    assert_eq!(h.decision(pending.result_id).await.unwrap().0, "returned");
    let returns: i64 =
        sqlx::query_scalar("SELECT returns::bigint FROM delivery_tasks WHERE task_id=$1")
            .bind(task)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(returns, 1);
    let runs = driver
        .inspect_runs(h.f.actor, Default::default())
        .await
        .unwrap();
    assert_eq!(runs.items.len(), 1);
    assert_eq!(runs.items[0].status, "completed");
    assert_eq!(h.settled(), vec![("policy".into(), pending.result_id); 2]);

    // A final receipt also survives losing the answer before the engine sees it.
    let (_, lost) = h.stopped(h.scope.project_id, "failed");
    driver.test_lose_next_host_reply("work.return");
    for _ in 0..20 {
        driver.quantum().await.unwrap();
        if h.decision(lost.result_id).await.is_some() {
            break;
        }
    }
    assert_eq!(h.decision(lost.result_id).await.unwrap().0, "returned");
    h.later(15_000);
    for _ in 0..8 {
        driver.quantum().await.unwrap();
    }
    let runs = driver
        .inspect_runs(h.f.actor, Default::default())
        .await
        .unwrap();
    assert_eq!(runs.items.len(), 2);
    assert!(runs.items.iter().all(|run| run.status == "completed"));
    assert_eq!(h.settled().len(), 3);

    // An admitted decision can wait for authority. It must never run on a revoked grant.
    h.answers(Settled::Pending);
    let (_, revoked) = h.stopped(h.scope.project_id, "failed");
    for _ in 0..20 {
        driver.quantum().await.unwrap();
        if h.settled()
            .iter()
            .any(|(_, result)| *result == revoked.result_id)
        {
            break;
        }
    }
    assert_eq!(h.settled().len(), 4);
    h.service.revoke(h.f.actor, h.grant).await.unwrap();
    h.answers(Settled::Returned);
    h.later(15_000);
    for _ in 0..8 {
        driver.quantum().await.unwrap();
    }
    assert_eq!(h.settled().len(), 4);
    assert_eq!(h.decision(revoked.result_id).await, None);
    driver.stop().await.unwrap();
}

#[tokio::test]
async fn work_return_sends_the_task_back_and_counts_each_result_once() {
    let h = delivery(false).await;
    h.answers(Settled::Returned);
    let (task, first) = h.stopped(h.scope.project_id, "success");
    assert_eq!(h.returns(task).await, 0);
    let input = back(task, first.result_id, "checks_failed", Some("unit"), false);
    let sent = h.run("return-1", "work.return", input.clone()).await;
    assert_eq!(sent, empty("returned"));
    assert_eq!(
        h.settled(),
        [("checks_failed".to_string(), first.result_id)]
    );
    assert_eq!(h.returns(task).await, 1);
    // What the next attempt is told: the reason, the detail and whether to update from main.
    let decided = h.decision(first.result_id).await.unwrap();
    let want = ("returned", Some("checks_failed"), Some("unit"), Some(false));
    assert_eq!(
        (
            decided.0.as_str(),
            decided.1.as_deref(),
            decided.2.as_deref(),
            decided.3
        ),
        want
    );
    // A replay, and another run's return of the same result, count nothing more.
    assert_eq!(h.run("return-1", "work.return", input.clone()).await, sent);
    assert_eq!(h.run("return-2", "work.return", input).await, sent);
    assert_eq!(h.returns(task).await, 1);
    assert_eq!(h.settled().len(), 1);
    // A returned result is not completed afterwards.
    let late = h.run("late", "work.complete", decide(task, first.result_id));
    assert_eq!(late.await, refused("stale_result"));
    // The next attempt's result goes back on a conflict, updated from main.
    let next = h.next_result(task);
    let conflict = back(task, next, "merge_conflict", None, true);
    let sent = h.run("return-3", "work.return", conflict).await;
    assert_eq!(sent, empty("returned"));
    assert_eq!(h.returns(task).await, 2);
    let decided = h.decision(next).await.unwrap();
    assert_eq!(
        decided,
        (
            "returned".into(),
            Some("merge_conflict".into()),
            None,
            Some(true)
        )
    );
    // A fenced or stale return changes nothing and counts nothing.
    let third = h.next_result(task);
    h.answers(Settled::Stale);
    let fenced = h.run(
        "return-4",
        "work.return",
        back(task, third, "policy", None, false),
    );
    assert_eq!(fenced.await, refused("stale_result"));
    let stale = back(task, next, "policy", None, false);
    assert_eq!(
        h.run("return-5", "work.return", stale).await,
        refused("stale_result")
    );
    assert_eq!(h.returns(task).await, 2);
    assert_eq!(h.decision(third).await, None);
}

#[tokio::test]
async fn work_hold_holds_a_result_until_a_later_decision() {
    let h = delivery(false).await;
    h.activated().await;
    let (task, p) = h.stopped(h.scope.project_id, "failed");
    assert_eq!(h.asked().await, 1);
    let held = h.run(
        "hold-1",
        "work.hold",
        hold(task, p.result_id, "returns_exhausted"),
    );
    assert_eq!(held.await, empty("held"));
    // Holding settles nothing and tells nobody; the result is not announced again.
    assert!(h.settled().is_empty());
    let decided = h.decision(p.result_id).await.unwrap();
    assert_eq!(
        decided,
        ("held".into(), Some("returns_exhausted".into()), None, None)
    );
    h.later(10_000_000);
    assert_eq!(h.asked().await, 0);
    // A second hold keeps the first.
    let again = h.run("hold-2", "work.hold", hold(task, p.result_id, "policy"));
    assert_eq!(again.await, empty("held"));
    assert_eq!(h.decision(p.result_id).await, Some(decided));
    // A later decision ends the hold.
    h.answers(Settled::Returned);
    let input = back(task, p.result_id, "policy", None, false);
    assert_eq!(
        h.run("return", "work.return", input).await,
        empty("returned")
    );
    assert_eq!(h.returns(task).await, 1);
    assert_eq!(h.decision(p.result_id).await.unwrap().0, "returned");
    // A hold after a final decision changes nothing.
    let late = h.intent("hold-3", "work.hold", hold(task, p.result_id, "policy"));
    assert!(matches!(h.try_run(&late).await, Err(Error::Conflict)));
    assert_eq!(h.decision(p.result_id).await.unwrap().0, "returned");
    // A stale result must never produce a held receipt.
    let (other, earlier) = h.stopped(h.scope.project_id, "success");
    h.next_result(other);
    let stale = h.intent(
        "hold-4",
        "work.hold",
        hold(other, earlier.result_id, "policy"),
    );
    assert!(matches!(h.try_run(&stale).await, Err(Error::Conflict)));
    assert_eq!(h.decision(earlier.result_id).await, None);
    // A task or result that does not exist is an invalid request.
    for input in [
        hold(Uuid::new_v4(), p.result_id, "policy"),
        hold(task, Uuid::new_v4(), "policy"),
    ] {
        let intent = h.intent(&format!("hold-{}", Uuid::new_v4()), "work.hold", input);
        assert!(matches!(h.try_run(&intent).await, Err(Error::Invalid)));
    }
}

#[tokio::test]
async fn a_stale_hold_and_its_notification_cannot_act_on_a_same_head_replacement() {
    let h = delivery(false).await;
    let (task, original) = h.stopped(h.scope.project_id, "success");
    let run = h.event_run(task).await;
    let mut hold_intent = h.intent(
        "event-hold",
        "work.hold",
        hold(task, original.result_id, "policy"),
    );
    hold_intent.run.clone_from(&run);
    let mut notice = h.intent(
        "event-notice",
        "supervisor.notify",
        json!({
            "project": original.project_id, "identity": original.result_id, "text": "stale hold",
        }),
    );
    notice.run = run;
    assert!(matches!(h.try_run(&notice).await, Err(Error::Conflict)));
    assert_eq!(h.try_run(&hold_intent).await.unwrap(), empty("held"));

    h.supersede_report(task);
    assert_eq!(h.try_run(&hold_intent).await.unwrap(), empty("held"));
    assert!(matches!(h.try_run(&notice).await, Err(Error::Conflict)));
    assert_eq!(h.messages("stale hold").await, 0);
}

#[tokio::test]
async fn held_result_notice_identifies_the_work_that_needs_attention() {
    let h = delivery(false).await;
    let (task, original) = h.stopped(h.scope.project_id, "failed");
    let run = h.event_run(task).await;
    let mut hold_intent = h.intent(
        "hold",
        "work.hold",
        hold(task, original.result_id, "policy"),
    );
    hold_intent.run.clone_from(&run);
    assert_eq!(h.try_run(&hold_intent).await.unwrap(), empty("held"));
    let mut notice = h.intent("notify", "supervisor.notify",
        json!({"project":original.project_id, "identity":original.result_id, "text":"Delivery requires attention."}));
    notice.run = run;
    assert_eq!(h.try_run(&notice).await.unwrap(), empty("sent"));
    let body: String = sqlx::query_scalar("SELECT m.body FROM mailbox_messages m JOIN delivery_notifications n USING(message_id) WHERE n.project_id=$1")
        .bind(original.project_id).fetch_one(&h.pool().await).await.unwrap();
    for id in [
        original.project_id,
        task,
        original.attempt_id,
        original.result_id,
    ] {
        assert!(body.contains(&id.to_string()), "{body}");
    }
    assert!(body.contains("policy") && body.contains("Delivery requires attention."));
    assert!(body.contains("Recorded outcome: failed.") && body.contains("task.work"));
    assert!(
        body.contains(&format!("Retained head: {}", "a".repeat(40)))
            && body.contains("task.history")
    );
}

#[tokio::test]
async fn supervisor_notify_tells_the_projects_supervisor_once_per_identity() {
    let h = delivery(false).await;
    let notify = |project: Uuid, identity: &str, text: &str| json!({"project": project.to_string(), "identity": identity, "text": text});
    let own = h.scope.project_id;
    let text = format!("Held after three returns ({})", Uuid::new_v4());
    let sent = h.run(
        "notify-1",
        "supervisor.notify",
        notify(own, "result-1", &text),
    );
    assert_eq!(sent.await, empty("sent"));
    assert_eq!(h.messages(&text).await, 1);
    // A replay of the intent sends nothing more.
    let replay = h.run(
        "notify-1",
        "supervisor.notify",
        notify(own, "result-1", &text),
    );
    assert_eq!(replay.await, empty("sent"));
    // Another call with the same identity, whatever its text, sends nothing.
    let other = format!("Another sentence ({})", Uuid::new_v4());
    let duplicate = h.run(
        "notify-2",
        "supervisor.notify",
        notify(own, "result-1", &other),
    );
    assert_eq!(duplicate.await, empty("duplicate"));
    assert_eq!((h.messages(&text).await, h.messages(&other).await), (1, 0));
    // Another identity is another message.
    let next = h.run(
        "notify-3",
        "supervisor.notify",
        notify(own, "result-2", &text),
    );
    assert_eq!(next.await, empty("sent"));
    assert_eq!(h.messages(&text).await, 2);
    // Another project is not this namespace's to address.
    let foreign = notify(h.other_project, "result-1", &text);
    let intent = h.intent("notify-4", "supervisor.notify", foreign);
    assert!(matches!(h.try_run(&intent).await, Err(Error::Forbidden)));
    assert_eq!(h.messages(&text).await, 2);
}

#[tokio::test]
async fn supervisor_notify_is_once_per_identity_in_each_project() {
    let h = delivery(true).await;
    let text = format!("Held ({})", Uuid::new_v4());
    for (id, project) in [("own", h.scope.project_id), ("other", h.other_project)] {
        let input = json!({"project": project.to_string(), "identity": "result-1", "text": text});
        let sent = h.run(id, "supervisor.notify", input).await;
        assert_eq!(sent, empty("sent"));
    }
    assert_eq!(h.messages(&text).await, 2);
    // A project the server does not run is refused even with installation routing.
    let input = json!({"project": Uuid::new_v4().to_string(), "identity": "x", "text": text});
    let intent = h.intent("unbound", "supervisor.notify", input);
    assert!(matches!(h.try_run(&intent).await, Err(Error::Forbidden)));
}
