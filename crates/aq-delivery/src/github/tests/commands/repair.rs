use super::*;
const SECOND_TASK: Uuid = Uuid::from_u128(0x2345);
fn second(head: char, base: char, state: &str) -> Value {
    pull(
        8,
        json!({
            "mergeable_state": state,
            "head": {"ref": format!("aq/{SECOND_TASK}"), "sha": sha(head)},
            "base": {"ref": "main", "sha": sha(base)},
        }),
    )
}
fn green() -> Value {
    runs(&[
        ("check", "completed", Some("success")),
        ("deployment", "completed", Some("success")),
    ])
}
async fn merge(delivery: &Delivery, task: Uuid, pr: u64, head: char) -> Completion {
    done(
        delivery
            .pull_request_merge(
                TaskOperation::new(Uuid::new_v4(), &target(), task),
                pr,
                MergeGuard {
                    method: MergeMethod::Merge,
                    expected_head: &sha(head),
                },
            )
            .await,
    )
}
pub(super) async fn merge_first(delivery: &Delivery, mock: &Mock) {
    let first = pull(7, json!({}));
    let early_second = second('b', 'c', "clean");
    poll(mock, first.clone(), first.clone(), green());
    assert_eq!(
        poll_result(delivery, 7, &sha('a')).await.unwrap().outcome,
        "passed"
    );
    poll(mock, early_second.clone(), early_second, green());
    assert_eq!(
        poll_result(delivery, 8, &sha('b')).await.unwrap().outcome,
        "passed"
    );

    mock.reply(200, vec![], first);
    mock.reply(200, vec![], json!({"sha": sha('d'), "merged": true}));
    assert_eq!(merge(delivery, TASK, 7, 'a').await.outcome, "merged");
}
pub(super) async fn confirm_behind(delivery: &Delivery, mock: &Mock, git: &FakeGit) {
    // The next poll and merge both request the existing update-from-main
    // repair, even though this head's checks passed before the first merge.
    let behind = second('b', 'd', "behind");
    mock.reply(200, vec![], behind.clone());
    let check = poll_result(delivery, 8, &sha('b')).await.unwrap();
    assert_eq!(
        (check.outcome, check.diagnostic),
        ("conflict", Some("behind_base"))
    );
    mock.reply(200, vec![], behind);
    let blocked = merge(delivery, SECOND_TASK, 8, 'b').await;
    assert_eq!(
        (blocked.outcome, blocked.diagnostic),
        ("conflict", Some("behind_base"))
    );
    assert_eq!(
        git.operations(),
        ["ref.delete"],
        "the second branch stays in place"
    );
}
pub(super) async fn publish_repair(delivery: &Delivery, mock: &Mock) {
    // Repair publishes a new head on the same task branch and PR. Results
    // for the old head cannot let the repaired PR skip its new checks.
    let sync = done(delivery.main_sync(Uuid::new_v4(), &target()).await);
    assert_eq!(
        (sync.outcome, sync.output),
        ("synced", json!({"head": sha('d')}))
    );
    let publish = done(
        delivery
            .branch_publish(
                TaskOperation::new(Uuid::new_v4(), &target(), SECOND_TASK),
                Some(&sha('b')),
                Some(&sha('e')),
            )
            .await,
    );
    assert_eq!(
        (publish.outcome, publish.output),
        ("published", json!({"head": sha('e')}))
    );
    let repaired = second('e', 'd', "clean");
    mock.reply(200, vec![], repaired.clone());
    assert!(poll_result(delivery, 8, &sha('b')).await.is_none());
    poll(mock, repaired.clone(), repaired.clone(), runs(&[]));
    assert!(poll_result(delivery, 8, &sha('e')).await.is_none());
    poll(mock, repaired.clone(), repaired.clone(), green());
    assert_eq!(
        poll_result(delivery, 8, &sha('e')).await.unwrap().outcome,
        "passed"
    );
    mock.reply(200, vec![], repaired);
    mock.reply(200, vec![], json!({"sha": sha('f'), "merged": true}));
    assert_eq!(merge(delivery, SECOND_TASK, 8, 'e').await.outcome, "merged");
}
pub(super) fn check_git(git: &FakeGit) {
    assert_eq!(
        git.operations(),
        ["ref.delete", "repo.retain_ref", "publish", "ref.delete"]
    );
    assert_eq!(
        (
            &git.request(0)["params"]["ref"],
            &git.request(3)["params"]["ref"]
        ),
        (&json!(task_branch(TASK)), &json!(task_branch(SECOND_TASK)))
    );
    let request = git.request(2);
    let params = &request["params"];
    assert_eq!(
        (&params["target"], &params["update"]),
        (&json!(task_branch(SECOND_TASK)), &json!("replace"))
    );
    assert_eq!(
        (
            &params["expected_old"]["hex"],
            &params["expected_source"]["hex"]
        ),
        (&json!(sha('b')), &json!(sha('e')))
    );
}
pub(super) fn check_forge(mock: &Mock) {
    let requests = mock.seen();
    let puts: Vec<_> = requests.iter().filter(|r| r.method == "PUT").collect();
    assert_eq!(puts.len(), 2);
    assert_eq!(
        (
            puts[0].path.as_str(),
            puts[1].path.as_str(),
            &puts[1].body["sha"]
        ),
        (
            "/api/v3/repos/Acme/widgets/pulls/7/merge",
            "/api/v3/repos/Acme/widgets/pulls/8/merge",
            &json!(sha('e'))
        )
    );
    assert_eq!(
        requests
            .iter()
            .filter(|r| r
                .path
                .ends_with(&format!("/commits/{}/check-runs", sha('e'))))
            .count(),
        2
    );
}

#[tokio::test]
async fn two_independent_prs_repair_and_retest_the_second_after_the_first_lands() {
    let git = FakeGit::start(|request| {
        let mut reply = healthy(request);
        match request["operation"].as_str().unwrap() {
            "repo.retain_ref" => reply["head"]["hex"] = json!(sha('d')),
            "publish" => reply["new"]["hex"] = json!(sha('e')),
            _ => {}
        }
        reply
    });
    let (delivery, mock) = delivery(&git).await;
    merge_first(&delivery, &mock).await;
    confirm_behind(&delivery, &mock, &git).await;
    publish_repair(&delivery, &mock).await;
    check_git(&git);
    check_forge(&mock);
}
