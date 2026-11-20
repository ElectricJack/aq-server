//! The delivery commands against the scripted GitHub stand-in and a fake
//! aq-git on a unix socket: one completion per outcome.
use super::*;
use crate::commands::{
    CheckPoll, Completion, Delivery, DeliveryTarget, GitPort, GitTarget, SocketGit, Step,
    bounded_wait_ms, task_branch,
};
use crate::policy::{GitHubRepository, MergeMethod};
use std::path::PathBuf;
use uuid::Uuid;

const TASK: Uuid = Uuid::from_u128(0x1234);

fn target() -> DeliveryTarget {
    DeliveryTarget {
        git: GitTarget {
            repository_id: Uuid::from_u128(7),
            binding_generation: 3,
        },
        github: GitHubRepository {
            owner: "Acme".into(),
            name: "widgets".into(),
        },
        default_branch: "main".into(),
    }
}
fn sha(c: char) -> String {
    c.to_string().repeat(40)
}
fn pull(number: u64, extra: Value) -> Value {
    let mut v = json!({
        "number": number,
        "html_url": format!("https://github.com/Acme/widgets/pull/{number}"),
        "state": "open", "merged": false, "merged_at": null,
        "mergeable": true, "mergeable_state": "clean",
        "head": {"ref": format!("aq/{TASK}"), "sha": sha('a')},
        "base": {"ref": "main", "sha": sha('c')},
    });
    for (k, val) in extra.as_object().unwrap() {
        v[k] = val.clone();
    }
    v
}
fn runs(runs: &[(&str, &str, Option<&str>)]) -> Value {
    let runs: Vec<Value> = runs
        .iter()
        .map(|(n, s, c)| json!({"name": n, "status": s, "conclusion": c}))
        .collect();
    json!({"total_count": runs.len(), "check_runs": runs})
}
fn no_statuses() -> Value {
    json!({"state": "pending", "total_count": 0, "statuses": []})
}

/// A fake aq-git: answers each frame with `answer(request)` and keeps it.
struct FakeGit {
    path: PathBuf,
    seen: Arc<Mutex<Vec<Value>>>,
    _dir: tempfile::TempDir,
}
impl FakeGit {
    fn start(answer: impl Fn(&Value) -> Value + Send + Sync + 'static) -> Self {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("git.sock");
        let listener = tokio::net::UnixListener::bind(&path).unwrap();
        let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let mut line = String::new();
                let mut reader = BufReader::new(&mut stream);
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    continue;
                }
                let request: Value = serde_json::from_str(&line).unwrap();
                let result = answer(&request);
                log.lock().unwrap().push(request.clone());
                let mut frame = json!({"protocol_version": 1, "request_id": request["request_id"]});
                match result.get("error") {
                    Some(error) => {
                        frame["ok"] = json!(false);
                        frame["error"] = error.clone();
                    }
                    None => {
                        frame["ok"] = json!(true);
                        frame["result"] = result;
                    }
                }
                let mut bytes = serde_json::to_vec(&frame).unwrap();
                bytes.push(b'\n');
                let _ = stream.write_all(&bytes).await;
            }
        });
        Self {
            path,
            seen,
            _dir: dir,
        }
    }
    fn git(&self) -> Arc<dyn GitPort> {
        Arc::new(SocketGit::new(&self.path))
    }
    fn operations(&self) -> Vec<String> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|r| r["operation"].as_str().unwrap().to_string())
            .collect()
    }
    fn request(&self, index: usize) -> Value {
        self.seen.lock().unwrap()[index].clone()
    }
}
/// The aq-git epic 0 operations, answered as a healthy central git would.
fn healthy(request: &Value) -> Value {
    match request["operation"].as_str().unwrap() {
        "repo.retain_ref" => {
            json!({"ref": request["params"]["ref"], "head": {"algorithm":"sha1","hex":sha('9')},
            "retention_handle": Uuid::from_u128(5)})
        }
        "publish" => {
            json!({"outcome": "applied", "target":request["params"]["target"], "old":request["params"]["expected_old"], "new":{"algorithm":"sha1","hex":sha('a')}})
        }
        "ref.delete" => json!({"outcome": "deleted"}),
        other => panic!("unexpected aq-git operation {other}"),
    }
}
async fn delivery(git: &FakeGit) -> (Delivery, Mock) {
    let (app, mock) = app().await;
    mock.token("ghs_cmd", Utc::now() + Span::hours(1));
    (Delivery::new(git.git(), Arc::new(app)), mock)
}
fn done(step: Step) -> Completion {
    match step {
        Step::Done(c) => c,
        Step::Retry(why) => panic!("expected a completion, got a retry: {why}"),
    }
}
fn paths(mock: &Mock) -> Vec<(String, String)> {
    mock.seen()
        .iter()
        .skip(1)
        .map(|s| {
            (
                s.method.clone(),
                s.path
                    .trim_start_matches("/api/v3/repos/Acme/widgets")
                    .to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn main_sync_retains_the_default_branch_and_tells_its_head() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    let c = done(delivery.main_sync(Uuid::from_u128(1), &target()).await);
    assert_eq!((c.outcome, c.output), ("synced", json!({"head": sha('9')})));
    let request = git.request(0);
    assert_eq!(request["operation"], "repo.retain_ref");
    assert_eq!(request["params"], json!({"ref": "refs/heads/main"}));
    assert_eq!(request["repository_id"], Uuid::from_u128(7).to_string());
    assert_eq!(request["expected_binding_generation"], 3);
    assert_eq!(request["operation_id"], Uuid::from_u128(1).to_string());
    assert!(mock.seen().is_empty(), "main.sync makes no GitHub call");
    // aq-git not answering is the catalog's `unreachable`.
    let gone = Delivery::new(
        Arc::new(SocketGit::new("/nonexistent/aq-git.sock")),
        Arc::new(crate::UnconfiguredGitHub),
    );
    let c = done(gone.main_sync(Uuid::from_u128(1), &target()).await);
    assert_eq!(
        (c.outcome, c.output),
        ("refused", json!({"reason": "unreachable"}))
    );
}

#[tokio::test]
async fn branch_publish_pushes_the_task_branch_under_a_lease() {
    let git = FakeGit::start(healthy);
    let (delivery, _) = delivery(&git).await;
    let c = done(
        delivery
            .branch_publish(Uuid::from_u128(2), &target(), TASK, None, Some(&sha('a')))
            .await,
    );
    assert_eq!(
        (c.outcome, c.output),
        ("published", json!({"head": sha('a')}))
    );
    let request = git.request(0);
    let branch = task_branch(TASK);
    assert_eq!(request["operation"], "publish");
    assert_eq!(
        request["params"],
        json!({"source_ref":branch, "target":branch, "expected_old":{"algorithm":"sha1","hex":"0".repeat(40)}, "expected_source":{"algorithm":"sha1","hex":sha('a')}})
    );
    // A later publication leases on the head the last one reached.
    let old = sha('1');
    done(
        delivery
            .branch_publish(Uuid::from_u128(3), &target(), TASK, Some(&old), None)
            .await,
    );
    assert_eq!(git.request(1)["params"]["expected_old"]["hex"], old);
}

#[tokio::test]
async fn publishing_a_recorded_result_checks_its_source_and_recovers_an_existing_remote_head() {
    let git = FakeGit::start(|request| {
        let source = &request["params"]["expected_source"];
        match source["hex"].as_str() {
            Some(hex) if hex == sha('a') => {
                json!({"outcome":"already_present","observed_target":source})
            }
            Some(hex) if hex == sha('b') => json!({"error":{"code":"source_moved"}}),
            _ => {
                json!({"outcome":"already_present","observed_target":{"algorithm":"sha1","hex":sha('a')}})
            }
        }
    });
    let (delivery, _) = delivery(&git).await;
    for (head, outcome) in [('a', "published"), ('b', "conflict"), ('c', "refused")] {
        let step = delivery
            .branch_publish(Uuid::new_v4(), &target(), TASK, None, Some(&sha(head)))
            .await;
        assert_eq!(done(step).outcome, outcome);
    }
}

#[tokio::test]
async fn branch_publish_reports_a_lost_lease_a_missing_branch_and_an_unreachable_remote() {
    let git = FakeGit::start(|r| match r["params"]["expected_old"]["hex"].as_str() {
        Some(old) if old == "0".repeat(40) => {
            json!({"outcome": "rejected", "reason": "target_moved"})
        }
        Some(old) if old == sha('2') => {
            json!({"error": {"code": "object_unavailable", "message": "no such ref"}})
        }
        _ => {
            json!({"error": {"code": "remote_unreachable", "message": "x", "retry": "safe_same_operation"}})
        }
    });
    let (delivery, _) = delivery(&git).await;
    let publish = async |old: Option<&str>| {
        done(
            delivery
                .branch_publish(Uuid::from_u128(4), &target(), TASK, old, None)
                .await,
        )
    };
    let c = publish(None).await;
    assert_eq!((c.outcome, c.output), ("conflict", json!({})));
    let c = publish(Some(&sha('2'))).await;
    assert_eq!(
        (c.outcome, c.output),
        ("refused", json!({"reason": "no_branch"}))
    );
    let c = publish(Some(&sha('3'))).await;
    assert_eq!(
        (c.outcome, c.output),
        ("refused", json!({"reason": "unreachable"}))
    );
}

#[tokio::test]
async fn aq_git_is_never_asked_to_publish_or_delete_outside_the_task_prefix() {
    let git = FakeGit::start(healthy);
    let port = git.git();
    let repo = target().git;
    for reference in ["refs/heads/main", "refs/tags/aq/x", "refs/heads/aq/../main"] {
        let published = port
            .publish_ref(Uuid::from_u128(1), &repo, reference, None, None)
            .await;
        assert_eq!(published.unwrap_err().code, "invalid_ref", "{reference}");
        let deleted = port
            .delete_ref(Uuid::from_u128(1), &repo, reference, None)
            .await;
        assert_eq!(deleted.unwrap_err().code, "invalid_ref", "{reference}");
    }
    assert!(git.operations().is_empty());
}

#[tokio::test]
async fn pull_request_open_finds_or_opens_and_refuses_an_unpublished_branch() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    mock.reply(200, vec![], json!([pull(7, json!({}))]));
    mock.reply(200, vec![], pull(7, json!({})));
    let c = done(
        delivery
            .pull_request_open(&target(), TASK, "Task", "Report")
            .await,
    );
    assert_eq!(c.outcome, "opened");
    assert_eq!(
        c.output,
        json!({"number": 7, "url": "https://github.com/Acme/widgets/pull/7"})
    );
    assert_eq!(c.pull_request.unwrap().state, "open");
    assert_eq!(
        mock.seen()[1].query.as_deref(),
        Some(format!("state=all&head=Acme%3Aaq%2F{TASK}&base=main").as_str())
    );
    // None open: opened now.
    mock.reply(200, vec![], json!([]));
    mock.reply(201, vec![], pull(8, json!({})));
    let c = done(
        delivery
            .pull_request_open(&target(), TASK, "Task", "Report")
            .await,
    );
    assert_eq!(c.output["number"], 8);
    assert_eq!(
        mock.seen().last().unwrap().body,
        json!({"title": "Task", "head": format!("aq/{TASK}"), "base": "main", "body": "Report"})
    );
    // The branch is not on GitHub: 422.
    mock.reply(200, vec![], json!([]));
    mock.reply(422, vec![], json!({"message": "Validation Failed"}));
    let c = done(
        delivery
            .pull_request_open(&target(), TASK, "Task", "Report")
            .await,
    );
    assert_eq!(
        (c.outcome, c.output),
        ("refused", json!({"reason": "not_published"}))
    );
    // GitHub does not answer.
    mock.reply(503, vec![], json!({"message": "down"}));
    let c = done(
        delivery
            .pull_request_open(&target(), TASK, "Task", "Report")
            .await,
    );
    assert_eq!(
        (c.outcome, c.output),
        ("refused", json!({"reason": "unreachable"}))
    );
}

/// One poll with a required list: the pull request (state), its mergeability,
/// then the checks (pull request, protection requirements, runs, statuses).
fn poll(mock: &Mock, state: Value, mergeable: Value, check_runs: Value) {
    mock.reply(200, vec![], state);
    mock.reply(200, vec![], mergeable);
    mock.reply(200, vec![], pull(7, json!({})));
    mock.reply(200, vec![], json!([]));
    mock.reply(200, vec![], json!({"name": "main", "protected": false}));
    mock.reply(200, vec![], check_runs);
    mock.reply(200, vec![], no_statuses());
}
async fn step(delivery: &Delivery, now: i64, deadline: i64) -> Option<Completion> {
    poll_of(delivery, now, deadline).await.completion
}
async fn poll_of(delivery: &Delivery, now: i64, deadline: i64) -> CheckPoll {
    let required = ["unit".to_string()];
    delivery
        .await_checks_step(&target(), 7, Some(&required[..]), now, deadline)
        .await
}

#[tokio::test]
async fn await_checks_completes_once_with_passed_failed_conflict_closed_or_timeout() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    let open = || pull(7, json!({}));
    let ok = || runs(&[("unit", "completed", Some("success"))]);
    let pending = || runs(&[("unit", "in_progress", None)]);

    poll(&mock, open(), open(), ok());
    let p = poll_of(&delivery, 1_000, 5_000).await;
    let c = p.completion.unwrap();
    assert_eq!((c.outcome, c.output), ("passed", json!({})));
    assert_eq!(p.observed.unwrap().checks_state, "passed");

    poll(
        &mock,
        open(),
        open(),
        runs(&[("unit", "completed", Some("failure"))]),
    );
    let p = poll_of(&delivery, 1_000, 5_000).await;
    let c = p.completion.unwrap();
    assert_eq!(
        (c.outcome, c.output),
        ("failed", json!({"names": ["unit"]}))
    );
    assert_eq!(p.observed.unwrap().checks_failed, ["unit"]);

    // A conflict is found before the checks are read.
    mock.reply(200, vec![], open());
    mock.reply(
        200,
        vec![],
        pull(7, json!({"mergeable": false, "mergeable_state": "dirty"})),
    );
    let p = poll_of(&delivery, 1_000, 5_000).await;
    assert_eq!(p.completion.unwrap().outcome, "conflict");
    assert_eq!(p.observed.unwrap().mergeable, "conflicting");

    // Closed without a merge (and merged elsewhere) is `closed`.
    mock.reply(200, vec![], pull(7, json!({"state": "closed"})));
    assert_eq!(
        step(&delivery, 1_000, 5_000).await.unwrap().outcome,
        "closed"
    );

    // Pending before the deadline: no completion. At the deadline: timeout.
    poll(&mock, open(), open(), pending());
    let p = poll_of(&delivery, 4_999, 5_000).await;
    assert!(p.completion.is_none());
    assert_eq!(p.observed.unwrap().checks_state, "pending");
    poll(&mock, open(), open(), pending());
    let c = step(&delivery, 5_000, 5_000).await.unwrap();
    assert_eq!((c.outcome, c.output), ("timeout", json!({})));

    // A read that fails decides nothing; the deadline still ends the wait.
    mock.reply(503, vec![], json!({"message": "down"}));
    assert!(step(&delivery, 1_000, 5_000).await.is_none());
    mock.reply(503, vec![], json!({"message": "down"}));
    assert_eq!(
        step(&delivery, 9_000, 5_000).await.unwrap().outcome,
        "timeout"
    );

    // A decision that is made wins over a deadline that has also passed.
    poll(&mock, open(), open(), ok());
    assert_eq!(
        step(&delivery, 9_000, 5_000).await.unwrap().outcome,
        "passed"
    );
}

#[test]
fn the_wait_is_bounded_by_the_longest_check_wait_setting() {
    assert_eq!(bounded_wait_ms(1_800_000, 3_600_000), 1_800_000);
    assert_eq!(bounded_wait_ms(604_800_000, 3_600_000), 3_600_000);
}

#[tokio::test]
async fn merge_merges_then_deletes_the_branch_on_the_remote_and_in_the_pool() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    mock.reply(200, vec![], pull(7, json!({})));
    mock.reply(
        200,
        vec![],
        json!({"sha": sha('d'), "merged": true, "message": "ok"}),
    );
    let c = done(
        delivery
            .pull_request_merge(
                Uuid::from_u128(6),
                &target(),
                TASK,
                7,
                MergeMethod::Merge,
                &sha('a'),
            )
            .await,
    );
    assert_eq!((c.outcome, c.output), ("merged", json!({})));
    assert_eq!(c.pull_request.unwrap().state, "merged");
    assert_eq!(
        mock.seen()[2].body,
        json!({"merge_method": "merge", "sha": sha('a')})
    );
    assert_eq!(git.operations(), ["ref.delete"]);
    assert_eq!(
        git.request(0)["params"],
        json!({"ref": task_branch(TASK), "remote": true, "expected_old": {"algorithm": "sha1", "hex": sha('a')}})
    );
}

#[tokio::test]
async fn merge_outcomes_other_than_merged_delete_nothing() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    let merge = async || {
        done(
            delivery
                .pull_request_merge(
                    Uuid::from_u128(6),
                    &target(),
                    TASK,
                    7,
                    MergeMethod::Squash,
                    &sha('a'),
                )
                .await,
        )
    };
    mock.reply(
        200,
        vec![],
        pull(7, json!({"mergeable": false, "mergeable_state": "dirty"})),
    );
    assert_eq!(merge().await.outcome, "conflict");
    mock.reply(200, vec![], pull(7, json!({})));
    mock.reply(
        405,
        vec![],
        json!({"message": "Required review is missing"}),
    );
    assert_eq!(merge().await.outcome, "not_mergeable");
    mock.reply(
        200,
        vec![],
        pull(7, json!({"head": {"ref": "x", "sha": sha('b')}})),
    );
    let c = merge().await;
    assert_eq!(
        (c.outcome, c.output),
        ("refused", json!({"reason": "head_changed"}))
    );
    mock.reply(404, vec![], json!({"message": "Not Found"}));
    let c = merge().await;
    assert_eq!(
        (c.outcome, c.output),
        ("refused", json!({"reason": "no_pull_request"}))
    );
    mock.reply(403, vec![], json!({"message": "forbidden"}));
    let c = merge().await;
    assert_eq!(
        (c.outcome, c.output),
        ("refused", json!({"reason": "unreachable"}))
    );
    assert!(git.operations().is_empty());
}

#[tokio::test]
async fn a_merged_pull_request_whose_branch_is_not_deleted_yet_is_retried_not_refused() {
    let healthy_after = Arc::new(Mutex::new(false));
    let flag = healthy_after.clone();
    let git = FakeGit::start(move |r| {
        if *flag.lock().unwrap() {
            healthy(r)
        } else {
            json!({"error": {"code": "io_error", "message": "busy", "retry": "safe_same_operation"}})
        }
    });
    let (delivery, mock) = delivery(&git).await;
    let merge = async || {
        delivery
            .pull_request_merge(
                Uuid::from_u128(6),
                &target(),
                TASK,
                7,
                MergeMethod::Merge,
                &sha('a'),
            )
            .await
    };
    mock.reply(200, vec![], pull(7, json!({})));
    mock.reply(
        200,
        vec![],
        json!({"sha": sha('d'), "merged": true, "message": "ok"}),
    );
    assert!(matches!(merge().await, Step::Retry(_)));
    // The retry finds the pull request merged at the expected head: no second
    // merge, and the deletion goes through with the same operation id.
    *healthy_after.lock().unwrap() = true;
    mock.reply(
        200,
        vec![],
        pull(
            7,
            json!({"state": "closed", "merged": true, "merge_commit_sha": sha('d')}),
        ),
    );
    assert_eq!(done(merge().await).outcome, "merged");
    assert_eq!(
        git.request(0)["operation_id"],
        git.request(1)["operation_id"]
    );
    let puts = paths(&mock).into_iter().filter(|(m, _)| m == "PUT").count();
    assert_eq!(puts, 1, "merged once");
}

#[tokio::test]
async fn merge_recovers_after_an_unknown_reply() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    let merge = async || {
        delivery
            .pull_request_merge(
                Uuid::from_u128(6),
                &target(),
                TASK,
                7,
                MergeMethod::Merge,
                &sha('a'),
            )
            .await
    };
    mock.reply(200, vec![], pull(7, json!({})));
    mock.reply(503, vec![], json!({"message": "reply lost"}));
    assert!(matches!(merge().await, Step::Retry(_)));
    assert!(git.operations().is_empty());

    mock.reply(
        200,
        vec![],
        pull(
            7,
            json!({
                "state": "closed", "merged": true, "merge_commit_sha": sha('d')
            }),
        ),
    );
    let completion = done(merge().await);
    assert_eq!(
        (completion.outcome, completion.output),
        ("merged", json!({}))
    );
    assert_eq!(
        paths(&mock),
        [
            ("GET".into(), "/pulls/7".into()),
            ("PUT".into(), "/pulls/7/merge".into()),
            ("GET".into(), "/pulls/7".into()),
        ]
    );
    assert_eq!(git.operations(), ["ref.delete"]);
}

#[tokio::test]
async fn a_merged_pr_is_recovered_only_at_the_checked_head_without_another_forge_write() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    let target = target();
    let merged = |head| {
        pull(
            7,
            json!({
                "state": "closed", "merged": true, "merge_commit_sha": sha('d'),
                "head": {"ref": format!("aq/{TASK}"), "sha": sha(head)},
            }),
        )
    };
    let poll = async || {
        delivery
            .step(
                Uuid::from_u128(9),
                "pull_request.await_checks",
                &json!({
                    "task": TASK, "pull_request": 7, "result_head": sha('a'),
                    "required": ["check", "deployment"], "deadline_at_ms": 1000,
                }),
                &target,
                1000,
            )
            .await
            .unwrap()
    };
    let reconcile = async || {
        done(
            delivery
                .pull_request_merge(
                    Uuid::from_u128(10),
                    &target,
                    TASK,
                    7,
                    MergeMethod::Merge,
                    &sha('a'),
                )
                .await,
        )
    };

    mock.reply(200, vec![], merged('b'));
    assert_eq!(done(poll().await.step.unwrap()).outcome, "timeout");
    mock.reply(200, vec![], merged('b'));
    assert_eq!(reconcile().await.output["reason"], "head_changed");
    assert!(git.operations().is_empty());

    let reply_checks = |check_runs| {
        mock.reply(200, vec![], merged('a'));
        mock.reply(200, vec![], merged('a'));
        mock.reply(200, vec![], json!([]));
        mock.reply(200, vec![], json!({"name": "main", "protected": false}));
        mock.reply(200, vec![], check_runs);
        mock.reply(200, vec![], no_statuses());
    };
    reply_checks(runs(&[("check", "completed", Some("success"))]));
    assert_eq!(done(poll().await.step.unwrap()).outcome, "timeout");
    assert!(git.operations().is_empty());

    reply_checks(runs(&[
        ("check", "completed", Some("success")),
        ("deployment", "completed", Some("success")),
    ]));
    let checked = poll().await;
    assert_eq!(done(checked.step.unwrap()).outcome, "passed");
    assert_eq!(checked.observed.unwrap().state, "merged");
    mock.reply(200, vec![], merged('a'));
    assert_eq!(reconcile().await.outcome, "merged");

    assert!(paths(&mock).iter().all(|(method, _)| method == "GET"));
    assert_eq!(git.operations(), ["ref.delete"]);
    assert_eq!(git.request(0)["params"]["expected_old"]["hex"], sha('a'));
}

#[tokio::test]
async fn close_closes_the_pull_request_and_deletes_the_branch() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    mock.reply(200, vec![], pull(7, json!({"state": "closed"})));
    let c = done(
        delivery
            .pull_request_close(Uuid::from_u128(8), &target(), TASK, 7)
            .await,
    );
    assert_eq!((c.outcome, c.output), ("closed", json!({})));
    assert_eq!(c.pull_request.unwrap().state, "closed");
    assert_eq!(
        paths(&mock),
        [("PATCH".to_string(), "/pulls/7".to_string())]
    );
    assert_eq!(git.operations(), ["ref.delete"]);
    // GitHub down: the close is retried, the branch is left alone.
    mock.reply(503, vec![], json!({"message": "down"}));
    assert!(matches!(
        delivery
            .pull_request_close(Uuid::from_u128(8), &target(), TASK, 7)
            .await,
        Step::Retry(_)
    ));
    assert_eq!(git.operations(), ["ref.delete"]);
}

#[tokio::test]
async fn a_frozen_request_runs_as_the_command_it_was_accepted_for() {
    use crate::commands::{without_delivery, without_target};
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    let operation = Uuid::from_u128(9);
    let run = async |command: &str, request: Value, now: i64| {
        delivery
            .step(operation, command, &request, &target(), now)
            .await
    };
    let task = TASK.to_string();
    // publish: the lease is the one frozen with the request.
    let r = run(
        "branch.publish",
        json!({"task": task, "expected_old": sha('1')}),
        0,
    )
    .await;
    let Some(Step::Done(c)) = r.unwrap().step else {
        panic!("not done")
    };
    assert_eq!(c.outcome, "published");
    assert_eq!(git.request(0)["params"]["expected_old"], sha('1'));
    // merge with no pull request: refused, nothing sent anywhere.
    let r = run(
        "pull_request.merge",
        json!({"task": task, "pull_request": null, "method": "squash", "expected_head_sha": sha('a')}),
        0,
    )
    .await;
    let Some(Step::Done(c)) = r.unwrap().step else {
        panic!("not done")
    };
    assert_eq!(
        (c.outcome, c.output),
        ("refused", json!({"reason": "no_pull_request"}))
    );
    // await: keeps waiting while pending, and says what it saw; no pull request: closed.
    poll(
        &mock,
        pull(7, json!({})),
        pull(7, json!({})),
        runs(&[("unit", "queued", None)]),
    );
    let r = run(
        "pull_request.await_checks",
        json!({"task": task, "pull_request": 7, "required": ["unit"], "deadline_at_ms": 5000}),
        1000,
    )
    .await
    .unwrap();
    assert!(r.step.is_none());
    assert_eq!(r.observed.unwrap().checks_state, "pending");
    let r = run(
        "pull_request.await_checks",
        json!({"task": task, "pull_request": null, "deadline_at_ms": 5000}),
        1000,
    )
    .await;
    let Some(Step::Done(c)) = r.unwrap().step else {
        panic!("not done")
    };
    assert_eq!(c.outcome, "closed");
    // A request that does not fit its command is not run.
    assert!(run("branch.publish", json!({}), 0).await.is_none());
    assert!(
        run(
            "pull_request.await_checks",
            json!({"task": task, "pull_request": 7}),
            0
        )
        .await
        .is_none()
    );
    assert!(
        run("work.complete", json!({"task": task}), 0)
            .await
            .is_none()
    );
    // A project that is no longer bound, and a delivery that is not configured.
    let Step::Done(c) = without_target("main.sync") else {
        panic!()
    };
    assert_eq!(c.output, json!({"reason": "unknown_project"}));
    let Step::Done(c) = without_target("pull_request.close") else {
        panic!()
    };
    assert_eq!(c.outcome, "closed");
    assert!(without_delivery("pull_request.await_checks").is_none());
    assert!(matches!(
        without_delivery("pull_request.close"),
        Some(Step::Retry(_))
    ));
    let Some(Step::Done(c)) = without_delivery("pull_request.merge") else {
        panic!()
    };
    assert_eq!(c.output, json!({"reason": "unreachable"}));
}
