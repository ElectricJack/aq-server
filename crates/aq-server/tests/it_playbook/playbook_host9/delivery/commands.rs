use super::*;
use aq_delivery::{
    Checks, ChecksState, GitHubAdapter, GitHubRepository, MergeMethod, MergeOutcome, Mergeability,
    PullRequest, PullRequestState,
    commands::{
        Delivery, DeliveryTarget, GitFailure, GitPort, GitTarget, RefDeletion, RefPublication,
        RetainedRef,
    },
    github::GitHubResult,
};
use aq_server::playbooks::host_adapters::DeliveryOps;

fn oid(c: char) -> String {
    c.to_string().repeat(40)
}

mod forge;
use forge::Fake;

struct Ops(Arc<Fake>, DeliveryTarget);
#[async_trait::async_trait]
impl DeliveryOps for Ops {
    async fn target(&self, _: Uuid) -> Result<Option<DeliveryTarget>> {
        Ok(Some(self.1.clone()))
    }
    async fn commands(&self) -> Option<Delivery> {
        Some(Delivery::new(self.0.clone(), self.0.clone()))
    }
    async fn pull_request_text(&self, _: Uuid, _: Uuid) -> Result<(String, String)> {
        Ok(("Task title".into(), "The report".into()))
    }
}

async fn harness() -> (D, Arc<Fake>) {
    harness_for(DeliveryTarget {
        git: GitTarget {
            repository_id: Uuid::from_u128(1),
            binding_generation: 1,
        },
        github: GitHubRepository {
            owner: "Acme".into(),
            name: "widgets".into(),
        },
        default_branch: "main".into(),
    })
    .await
}

async fn harness_for(target: DeliveryTarget) -> (D, Arc<Fake>) {
    let mut h = delivery(false).await;
    let fake = Arc::new(Fake {
        log: Mutex::default(),
        checks: Mutex::new((ChecksState::Pending, vec![])),
        pr_state: Mutex::new(PullRequestState::Open),
        mergeability: Mutex::new(Mergeability::Clean),
        delete_unavailable: Mutex::new(false),
        merge_sha: Mutex::new(oid('d')),
    });
    // By value: the port of a shared (cloned) service is not replaced.
    h.service = h
        .service
        .with_delivery_ops(Arc::new(Ops(fake.clone(), target)));
    (h, fake)
}
/// A disposable Host10 producer with its real operation/evidence store.
/// Only central Git and the forge response are scripted.
pub(crate) struct GraphMergeHost {
    host: D,
    forge: Arc<Fake>,
    pub(crate) repository: Uuid,
}

impl GraphMergeHost {
    pub(crate) async fn new() -> Self {
        let repository = Uuid::new_v4();
        let (host, forge) = harness_for(DeliveryTarget {
            git: GitTarget {
                repository_id: repository,
                binding_generation: 1,
            },
            github: GitHubRepository {
                owner: "ElectricJack".into(),
                name: "aq-graph".into(),
            },
            default_branch: "main".into(),
        })
        .await;
        Self {
            host,
            forge,
            repository,
        }
    }

    pub(crate) fn fixture(&self) -> &common::profiled::Fixture {
        &self.host.f
    }

    pub(crate) async fn merge(&self, revision: &str) -> Uuid {
        let (task, _) = self.host.stopped(self.host.scope.project_id, "success");
        self.host.pull_request(task, "passed", json!([])).await;
        *self.forge.merge_sha.lock().unwrap() = revision.into();
        let handle = self
            .host
            .accepted(
                "pull_request.merge",
                json!({"task": task, "method": "merge", "expected_head_sha": oid('a')}),
            )
            .await;
        assert_eq!(self.host.pass().await, 1);
        Uuid::parse_str(&handle).unwrap()
    }
}

impl D {
    /// A command that answers `accepted {handle}`: its handle.
    async fn accepted(&self, name: &str, input: Value) -> String {
        let result = self
            .run(&format!("{name}-{}", Uuid::new_v4()), name, input)
            .await;
        assert_eq!(result.outcome, "accepted", "{name}: {}", result.output);
        result.output["handle"].as_str().unwrap().to_owned()
    }
    async fn pass(&self) -> usize {
        (self
            .service
            .collect_delivery_operations(&self.runtime, 4)
            .await)
            .unwrap()
    }
    /// Every `aq.delivery.completed` sent so far: (operation, payload).
    async fn completions(&self) -> Vec<(String, Value)> {
        let page = self.service.outbox(&self.runtime, 0, 16).await.unwrap();
        page.into_iter()
            .filter(|e| e.2.contract.name == "aq.delivery.completed")
            .map(|e| {
                // The event names no subject or revision.
                assert_eq!((&e.2.subject, &e.2.revision), (&None, &None));
                (e.2.correlation["operation"].clone(), e.2.payload)
            })
            .collect()
    }
}

#[tokio::test]
async fn main_sync_completes_once_with_the_head_and_main_moves_in_the_facts() {
    let (h, fake) = harness().await;
    let (task, _) = h.stopped(h.scope.project_id, "success");
    let handle = h
        .accepted(
            "main.sync",
            json!({"project": h.scope.project_id.to_string()}),
        )
        .await;
    assert!(
        h.completions().await.is_empty(),
        "nothing is done at dispatch"
    );
    assert_eq!(h.pass().await, 1);
    assert_eq!(
        h.completions().await,
        [(
            handle,
            json!({"outcome": "synced", "output": {"head": oid('9')}})
        )]
    );
    assert_eq!(*fake.log.lock().unwrap(), ["retain refs/heads/main"]);
    assert_eq!(h.read(task).await.output["facts"]["main_head"], oid('9'));
    // A completed operation is not run or completed again.
    assert_eq!(h.pass().await, 0);
    assert_eq!(h.completions().await.len(), 1);
    // An unknown project is refused at once.
    let refused = h
        .run(
            "sync-unknown",
            "main.sync",
            json!({"project": "not-a-project"}),
        )
        .await;
    assert_eq!(
        (refused.outcome.as_str(), &refused.output["reason"]),
        ("refused", &json!("unknown_project"))
    );
}

#[tokio::test]
async fn a_result_is_published_opened_checked_merged_and_its_branch_deleted() {
    let (h, fake) = harness().await;
    let (task, _) = h.stopped(h.scope.project_id, "success");
    let task_id = json!({"task": task.to_string()});
    let branch = format!("refs/heads/aq/{task}");

    let publish = h.accepted("branch.publish", task_id.clone()).await;
    let open = h.accepted("pull_request.open", task_id.clone()).await;
    assert_eq!(h.pass().await, 2);
    let done = h.completions().await;
    assert_eq!(
        done[0],
        (
            publish,
            json!({"outcome": "published", "output": {"head": oid('a')}})
        )
    );
    assert_eq!(
        done[1],
        (
            open,
            json!({"outcome": "opened", "output": {"number": 7, "url": "https://example.test/pr/7"}})
        )
    );
    // The first publication leases on an absent branch.
    assert_eq!(
        fake.log.lock().unwrap()[..2],
        [
            format!("publish {branch} lease absent"),
            "open Task title".to_string()
        ]
    );
    let facts = h.read(task).await.output["facts"].clone();
    assert_eq!(facts["pull_request"]["state"], "open");
    assert_eq!(facts["pull_request"]["number"], 7);

    assert_pending_checks_complete_once(&h, &fake, task).await;

    let merge = h
        .accepted(
            "pull_request.merge",
            json!({"task": task.to_string(), "method": "merge", "expected_head_sha": oid('a')}),
        )
        .await;
    assert_eq!(h.pass().await, 1);
    let done = h.completions().await;
    assert_eq!(done[3], (merge, json!({"outcome": "merged", "output": {}})));
    let log = fake.log.lock().unwrap().clone();
    assert_eq!(
        log[log.len() - 2..],
        [
            format!("merge merge {}", oid('a')),
            format!("delete {branch}")
        ]
    );
    assert_eq!(
        h.read(task).await.output["facts"]["pull_request"]["state"],
        "merged"
    );
}

async fn assert_pending_checks_complete_once(h: &D, fake: &Fake, task: Uuid) {
    // Checks are pending: the wait runs, and is not completed.
    let wait = h
        .accepted(
            "pull_request.await_checks",
            json!({"task": task.to_string(), "deadline_ms": 600_000}),
        )
        .await;
    assert_eq!(h.pass().await, 0);
    assert_eq!(h.completions().await.len(), 2);
    assert_eq!(
        h.read(task).await.output["facts"]["pull_request"]["checks"]["state"],
        "pending"
    );
    // They pass: exactly one completion, however many passes follow.
    *fake.checks.lock().unwrap() = (ChecksState::Passed, vec![]);
    assert_eq!(h.pass().await, 1);
    assert_eq!(h.pass().await, 0);
    let done = h.completions().await;
    assert_eq!(done.len(), 3);
    assert_eq!(done[2], (wait, json!({"outcome": "passed", "output": {}})));
}

#[tokio::test]
async fn a_behind_check_returns_the_result_for_update_from_main_and_keeps_the_pr() {
    let (h, fake) = harness().await;
    let (task, pending) = h.stopped(h.scope.project_id, "success");
    h.pull_request(task, "passed", json!([])).await;
    *fake.mergeability.lock().unwrap() = Mergeability::Behind;
    let wait = h
        .accepted(
            "pull_request.await_checks",
            json!({"task": task, "required": ["check", "deployment"], "deadline_ms": 600_000}),
        )
        .await;
    assert_eq!(h.pass().await, 1);
    assert_eq!(
        h.completions().await,
        [(wait, json!({"outcome": "conflict", "output": {}}))]
    );

    // The policy's existing conflict path returns the current result.
    *h.settle.answer.lock().unwrap() = Settled::Returned;
    let returned = h
        .run(
            "behind-repair",
            "work.return",
            json!({
                "task": task, "result_id": pending.result_id,
                "reason": "merge_conflict", "update_from_main": true
            }),
        )
        .await;
    assert_eq!(returned.outcome, "returned");
    let repair: bool =
        sqlx::query_scalar("SELECT update_from_main FROM delivery_decisions WHERE result_id=$1")
            .bind(pending.result_id)
            .fetch_one(&h.pool().await)
            .await
            .unwrap();
    assert!(repair);
    let facts = h.read(task).await.output["facts"].clone();
    assert_eq!(facts["pull_request"]["number"], 7);
    assert_eq!(facts["pull_request"]["state"], "open");
    assert!(fake.log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn merge_evidence_survives_cleanup_retry() {
    let (h, fake) = harness().await;
    let (task, result) = h.stopped(h.scope.project_id, "success");
    h.pull_request(task, "passed", json!([])).await;
    let handle = h
        .accepted(
            "pull_request.merge",
            json!({
                "task": task, "method": "merge", "expected_head_sha": oid('a')
            }),
        )
        .await;

    *fake.delete_unavailable.lock().unwrap() = true;
    assert_eq!(h.pass().await, 0);
    assert!(h.completions().await.is_empty());

    let pool = h.pool().await;
    let operation = Uuid::parse_str(&handle).unwrap();
    let evidence = || {
        sqlx::query_scalar::<_, Value>(
            "SELECT to_jsonb(e) FROM delivery_merge_evidence e WHERE operation_id=$1",
        )
        .bind(operation)
        .fetch_one(&pool)
    };
    let saved = evidence().await.unwrap();
    assert_eq!(saved["task_id"], json!(task));
    assert_eq!(saved["project_id"], json!(h.scope.project_id));
    assert_eq!(saved["attempt_id"], json!(result.attempt_id));
    assert_eq!(saved["result_id"], json!(result.result_id));
    assert_eq!(
        saved["target"]["git"]["repository_id"],
        json!(Uuid::from_u128(1))
    );
    assert_eq!(
        saved["target"]["github"],
        json!({"owner": "Acme", "name": "widgets"})
    );
    assert_eq!(saved["target_ref"], "refs/heads/main");
    assert_eq!(saved["pull_request"], 7);
    assert_eq!(saved["expected_head_sha"], oid('a'));
    assert_eq!(saved["merge_sha"], oid('d'));

    *fake.delete_unavailable.lock().unwrap() = false;
    h.later(1);
    assert_eq!(h.pass().await, 1);
    assert_eq!(h.pass().await, 0);
    assert_eq!(
        h.completions().await,
        [(handle, json!({"outcome": "merged", "output": {}}))]
    );
    assert_eq!(evidence().await.unwrap(), saved);
    let delete = format!("delete refs/heads/aq/{task}");
    assert_eq!(
        *fake.log.lock().unwrap(),
        [format!("merge merge {}", oid('a')), delete.clone(), delete]
    );
}

#[tokio::test]
async fn stale_result_with_same_revision_and_head_never_publishes_or_merges() {
    let (h, fake) = harness().await;
    let (task, original) = h.stopped(h.scope.project_id, "success");
    let input = json!({"task": task});
    let run = h.event_run(task).await;
    h.pull_request(task, "passed", json!([])).await;
    let mut merge = h.intent(
        "fenced-merge",
        "pull_request.merge",
        json!({
            "task": task, "method": "merge", "expected_head_sha": oid('a'),
        }),
    );
    merge.run.clone_from(&run);
    assert_eq!(h.try_run(&merge).await.unwrap().outcome, "accepted");
    let mut before = h.intent("fenced-before", "branch.publish", input.clone());
    before.run.clone_from(&run);
    assert_eq!(h.try_run(&before).await.unwrap().outcome, "accepted");
    assert_ne!(h.supersede_report(task), original.result_id);
    assert_eq!(h.read(task).await.output["facts"]["head"], oid('a'));

    let mut after = h.intent("fenced-after", "branch.publish", input);
    after.run = run;
    assert!(matches!(
        h.try_run(&after).await,
        Err(aq_server::playbooks::domain::Error::Conflict)
    ));
    assert_eq!(h.pass().await, 2);
    let completions = h.completions().await;
    assert_eq!(completions.len(), 2);
    assert!(
        completions
            .iter()
            .all(|(_, answer)| answer["outcome"] == "refused")
    );
    assert!(fake.log.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_tracked_closed_or_merged_pr_is_never_recreated() {
    let (h, fake) = harness().await;
    let (task, original) = h.stopped(h.scope.project_id, "success");
    h.pull_request(task, "pending", json!([])).await;
    let task_input = json!({"task": task});
    *fake.pr_state.lock().unwrap() = PullRequestState::Closed;
    h.accepted("branch.publish", task_input.clone()).await;
    assert_eq!(h.pass().await, 1);
    assert_eq!(h.completions().await[0].1["outcome"], "refused");
    let checks = json!({"task": task, "deadline_ms": 1000});
    h.accepted("pull_request.await_checks", checks.clone())
        .await;
    assert_eq!(h.pass().await, 1);
    assert_eq!(h.completions().await[1].1["outcome"], "closed");
    assert!(fake.log.lock().unwrap().is_empty());

    *fake.pr_state.lock().unwrap() = PullRequestState::Merged;
    *fake.checks.lock().unwrap() = (ChecksState::Passed, vec![]);
    h.accepted("branch.publish", task_input).await;
    assert_eq!(h.pass().await, 1);
    assert_eq!(h.completions().await[2].1["outcome"], "published");
    h.accepted("pull_request.await_checks", checks).await;
    assert_eq!(h.pass().await, 1);
    assert_eq!(h.completions().await[3].1["outcome"], "passed");
    h.accepted(
        "pull_request.merge",
        json!({
            "task": task, "method": "merge", "expected_head_sha": oid('a'),
        }),
    )
    .await;
    assert_eq!(h.pass().await, 1);
    assert_eq!(h.completions().await[4].1["outcome"], "merged");
    let receipt: (i64, Uuid, String) = sqlx::query_as(
        "SELECT pull_request,result_id,merge_sha FROM delivery_merge_evidence WHERE task_id=$1",
    )
    .bind(task)
    .fetch_one(&h.pool().await)
    .await
    .unwrap();
    assert_eq!(receipt, (7, original.result_id, oid('d')));
    assert_eq!(
        *fake.log.lock().unwrap(),
        [
            format!("merge merge {}", oid('a')),
            format!("delete refs/heads/aq/{task}"),
        ]
    );
}

#[tokio::test]
async fn failed_checks_and_the_deadline_each_complete_the_wait_once() {
    let (h, fake) = harness().await;
    // The wait is bounded by Settings, not by what the playbook asked for.
    h.limits(DeliveryLimits {
        longest_check_wait_ms: 60_000,
        ..Default::default()
    })
    .await;
    let (failing, _) = h.stopped(h.scope.project_id, "success");
    let (slow, _) = h.stopped(h.scope.project_id, "success");
    for task in [failing, slow] {
        h.pull_request(task, "pending", json!([])).await;
    }
    let wait = |task: Uuid| json!({"task": task.to_string(), "deadline_ms": 604_800_000});
    let failed = h.accepted("pull_request.await_checks", wait(failing)).await;
    *fake.checks.lock().unwrap() = (ChecksState::Failed, vec!["unit".into()]);
    assert_eq!(h.pass().await, 1);
    assert_eq!(
        h.completions().await,
        [(
            failed,
            json!({"outcome": "failed", "output": {"names": ["unit"]}})
        )]
    );
    let timed_out = h.accepted("pull_request.await_checks", wait(slow)).await;
    *fake.checks.lock().unwrap() = (ChecksState::Pending, vec![]);
    h.later(59_999);
    assert_eq!(h.pass().await, 0);
    h.later(1);
    assert_eq!(h.pass().await, 1);
    assert_eq!(h.pass().await, 0);
    let done = h.completions().await;
    assert_eq!(done.len(), 2);
    assert_eq!(
        done[1],
        (timed_out, json!({"outcome": "timeout", "output": {}}))
    );
}

#[tokio::test]
async fn close_closes_and_deletes_the_branch_and_unknown_tasks_are_answered_by_the_catalog() {
    let (h, fake) = harness().await;
    let (task, _) = h.stopped(h.scope.project_id, "success");
    h.pull_request(task, "pending", json!([])).await;
    let close = h
        .accepted("pull_request.close", json!({"task": task.to_string()}))
        .await;
    assert_eq!(h.pass().await, 1);
    assert_eq!(
        h.completions().await,
        [(close, json!({"outcome": "closed", "output": {}}))]
    );
    assert_eq!(
        fake.log.lock().unwrap().clone(),
        ["close".to_string(), format!("delete refs/heads/aq/{task}")]
    );
    assert_unknown_task_outcomes(&h).await;
    // A merge with no open pull request, and a publication of a task
    // that has no result branch, are refused.
    let (unpublished, _) = h.stopped(h.scope.project_id, "success");
    let merge =
        json!({"task": unpublished.to_string(), "method": "squash", "expected_head_sha": oid('a')});
    let r = h.run("merge-none", "pull_request.merge", merge).await;
    assert_eq!(r.output["reason"], "no_pull_request");
    h.port
        .latest
        .lock()
        .unwrap()
        .get_mut(&unpublished)
        .unwrap()
        .head
        .clear();
    let r = h
        .run(
            "publish-none",
            "branch.publish",
            json!({"task": unpublished.to_string()}),
        )
        .await;
    assert_eq!(
        (r.outcome.as_str(), &r.output["reason"]),
        ("refused", &json!("no_branch"))
    );
    // operation.get reports a running delivery operation, and only that.
    let (running, _) = h.stopped(h.scope.project_id, "success");
    let handle = h
        .accepted("branch.publish", json!({"task": running.to_string()}))
        .await;
    let get = h
        .run("get-delivery", "operation.get", json!({"handle": handle}))
        .await;
    assert_eq!(get.outcome, "running");
}

async fn assert_unknown_task_outcomes(h: &D) {
    // A task the host does not know: refused where the catalog has a
    // refusal, `closed` where it has none.
    let unknown = json!({"task": Uuid::new_v4().to_string()});
    for (name, reason) in [
        ("branch.publish", "unknown_task"),
        ("pull_request.open", "unknown_task"),
    ] {
        let r = h
            .run(&format!("{name}-unknown"), name, unknown.clone())
            .await;
        assert_eq!(
            (r.outcome.as_str(), &r.output["reason"]),
            ("refused", &json!(reason))
        );
    }
    let merge = json!({"task": Uuid::new_v4().to_string(), "method": "merge", "expected_head_sha": oid('a')});
    let r = h.run("merge-unknown", "pull_request.merge", merge).await;
    assert_eq!(r.output["reason"], "unknown_task");
    let before = h.completions().await.len();
    h.accepted("pull_request.close", unknown.clone()).await;
    h.accepted(
        "pull_request.await_checks",
        json!({"task": Uuid::new_v4().to_string(), "deadline_ms": 5000}),
    )
    .await;
    assert_eq!(h.pass().await, 2);
    let done = h.completions().await;
    assert_eq!(done.len(), before + 2);
    assert!(done[before..].iter().all(|d| d.1["outcome"] == "closed"));
}
