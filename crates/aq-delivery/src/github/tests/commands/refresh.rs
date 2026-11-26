//! A pull request that is only behind main, with green checks, is refreshed by
//! the host (GitHub's update-branch) instead of going back to an agent.
use super::*;
use crate::commands::Refreshed;

fn green() -> Value {
    runs(&[
        ("check", "completed", Some("success")),
        ("deployment", "completed", Some("success")),
    ])
}
/// PR 7 at `head` on base `base`.
fn at(head: char, base: char, state: &str) -> Value {
    pull(
        7,
        json!({"mergeable_state": state, "head": {"ref": format!("aq/{TASK}"), "sha": sha(head)},
               "base": {"ref": "main", "sha": sha(base)}}),
    )
}
fn commit(head: char, parents: &[char]) -> Value {
    let parents: Vec<Value> = parents.iter().map(|p| json!({"sha": sha(*p)})).collect();
    json!({"sha": sha(head), "parents": parents})
}
/// One wait step for result head b with the host's recorded `refresh`.
async fn wait(delivery: &Delivery, refresh: Value) -> crate::commands::Run {
    let request = json!({
        "pull_request": 7, "result_head": sha('b'), "check_target": target(),
        "required": ["check", "deployment"], "deadline_at_ms": 5_000, "refresh": refresh,
    });
    delivery
        .step(
            FrozenCommand::new(Uuid::new_v4(), "pull_request.await_checks", &request),
            &target(),
            1_000,
        )
        .await
        .unwrap()
}
async fn merge(delivery: &Delivery, pull_request_head: Option<char>) -> Completion {
    let request = json!({
        "task": TASK.to_string(), "pull_request": 7, "method": "merge",
        "expected_head_sha": sha('b'), "pull_request_head": pull_request_head.map(sha),
    });
    let run = delivery
        .step(
            FrozenCommand::new(Uuid::new_v4(), "pull_request.merge", &request),
            &target(),
            1_000,
        )
        .await
        .unwrap();
    done(run.step.unwrap())
}
fn updates(mock: &Mock) -> Vec<Value> {
    let seen = mock.seen();
    seen.iter()
        .filter(|r| r.method == "PUT" && r.path.ends_with("/pulls/7/update-branch"))
        .map(|r| r.body.clone())
        .collect()
}

#[tokio::test]
async fn a_behind_green_pr_is_refreshed_checked_again_and_merged_at_the_new_head() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    // Main moved from c to d: the PR at b is behind, its checks are green.
    poll(
        &mock,
        at('b', 'd', "behind"),
        at('b', 'd', "behind"),
        green(),
    );
    mock.reply(
        202,
        vec![],
        json!({"message": "Updating pull request branch."}),
    );
    let run = wait(&delivery, json!({"allow": true})).await;
    assert_eq!(run.step, None);
    assert_eq!(
        run.refreshed,
        Some(Refreshed::Requested {
            from: sha('b'),
            base: sha('d')
        })
    );
    assert_eq!(updates(&mock), [json!({"expected_head_sha": sha('b')})]);

    // GitHub's merge commit 1 of b and d is the head now: its checks run.
    let recorded = json!({"from": sha('b'), "base": sha('d'), "allow": false});
    mock.reply(200, vec![], at('1', 'd', "clean"));
    mock.reply(200, vec![], commit('1', &['b', 'd']));
    read_checks(&mock, at('1', 'd', "clean"), runs(&[]));
    let run = wait(&delivery, recorded).await;
    assert_eq!(
        (run.step, run.refreshed),
        (None, Some(Refreshed::Merged(sha('1'))))
    );
    let merged = json!({"from": sha('b'), "base": sha('d'), "merged": sha('1'), "allow": false});
    poll(&mock, at('1', 'd', "clean"), at('1', 'd', "clean"), green());
    let run = wait(&delivery, merged).await;
    assert_eq!(done(run.step.unwrap()).outcome, "passed");
    assert_eq!(run.refreshed, None);

    // The merge is pinned to 1; the central ref is still b.
    mock.reply(200, vec![], at('1', 'd', "clean"));
    no_queue(&mock);
    mock.reply(200, vec![], json!({"sha": sha('f'), "merged": true}));
    assert_eq!(merge(&delivery, Some('1')).await.outcome, "merged");
    let seen = mock.seen();
    let put = seen.iter().find(|r| r.path.ends_with("/merge")).unwrap();
    assert_eq!(put.body["sha"], json!(sha('1')));
    drop(seen);
    assert_eq!(updates(&mock).len(), 1, "one refresh");
    assert_eq!(git.operations(), ["ref.delete", "ref.delete"], "no publish");
    let lease = |i: usize| {
        let params = git.request(i)["params"].clone();
        (
            params["remote"].clone(),
            params["expected_old"]["hex"].clone(),
        )
    };
    assert_eq!(lease(0), (json!(false), json!(sha('b'))));
    assert_eq!(lease(1), (json!(true), json!(sha('1'))));
}

#[tokio::test]
async fn a_head_that_is_not_a_recorded_refresh_of_the_result_is_not_accepted() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    // A pending refresh from b, but 3's parents are not [b, a base commit].
    let pending = json!({"from": sha('b'), "base": sha('d'), "allow": false});
    for parents in [&['b', 'e'][..], &['d', 'b'], &['b']] {
        mock.reply(200, vec![], at('3', 'd', "clean"));
        mock.reply(200, vec![], commit('3', parents));
        let run = wait(&delivery, pending.clone()).await;
        assert_eq!((run.step, run.refreshed), (None, None), "{parents:?}");
    }
    // No refresh recorded: GitHub is not even asked about 1.
    mock.reply(200, vec![], at('1', 'd', "clean"));
    let run = wait(&delivery, json!({"allow": true})).await;
    assert_eq!((run.step, run.refreshed), (None, None));
    assert!(!mock.seen().iter().any(|r| r.path.ends_with(&sha('1'))));
    // A merge for b without a recorded refresh head refuses the moved head.
    mock.reply(200, vec![], at('1', 'd', "clean"));
    let c = merge(&delivery, None).await;
    assert_eq!(
        (c.outcome, c.output),
        ("refused", json!({"reason": "head_changed"}))
    );
    assert!(git.operations().is_empty());
}

#[tokio::test]
async fn a_refreshed_head_that_falls_behind_again_is_refreshed_from_itself() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    // 1 (from b) is green but main moved on to e: refresh again, from 1.
    let merged = json!({"from": sha('b'), "base": sha('d'), "merged": sha('1'), "allow": true});
    poll(
        &mock,
        at('1', 'e', "behind"),
        at('1', 'e', "behind"),
        green(),
    );
    mock.reply(202, vec![], json!({}));
    let run = wait(&delivery, merged).await;
    assert_eq!(
        (run.step, run.refreshed),
        (
            None,
            Some(Refreshed::Requested {
                from: sha('1'),
                base: sha('e')
            })
        )
    );
    assert_eq!(updates(&mock), [json!({"expected_head_sha": sha('1')})]);
    // GitHub merged main as it was when it ran (4, not e): 2's parents are [1, 4].
    let chained = json!({"from": sha('1'), "base": sha('e'), "allow": false});
    mock.reply(200, vec![], at('2', '4', "clean"));
    mock.reply(200, vec![], commit('2', &['1', '4']));
    read_checks(&mock, at('2', '4', "clean"), green());
    let run = wait(&delivery, chained).await;
    assert_eq!(done(run.step.unwrap()).outcome, "passed");
    assert_eq!(run.refreshed, Some(Refreshed::Merged(sha('2'))));
}

#[tokio::test]
async fn a_refresh_github_refuses_or_the_host_does_not_allow_is_not_a_merge_attempt() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    // Another task holds the repository's refresh slot: keep waiting.
    poll(
        &mock,
        at('b', 'd', "behind"),
        at('b', 'd', "behind"),
        green(),
    );
    let run = wait(&delivery, json!({"allow": false})).await;
    assert_eq!(
        (run.step, run.refreshed),
        (None, Some(Refreshed::Queued(sha('b'))))
    );
    // GitHub refuses the update (a real conflict): the agent repairs it.
    poll(
        &mock,
        at('b', 'd', "behind"),
        at('b', 'd', "behind"),
        green(),
    );
    mock.reply(
        422,
        vec![],
        json!({"message": "merge conflict between base and head"}),
    );
    let run = wait(&delivery, json!({"allow": true})).await;
    let c = done(run.step.unwrap());
    assert_eq!((c.outcome, c.diagnostic), ("conflict", Some("behind_base")));
    // GitHub is down: try again on the next look.
    poll(
        &mock,
        at('b', 'd', "behind"),
        at('b', 'd', "behind"),
        green(),
    );
    mock.reply(503, vec![], json!({"message": "down"}));
    let run = wait(&delivery, json!({"allow": true})).await;
    assert_eq!((run.step, run.refreshed), (None, None));
    // A wait frozen without the refresh setting: the agent updates it.
    poll(
        &mock,
        at('b', 'd', "behind"),
        at('b', 'd', "behind"),
        green(),
    );
    let run = wait(&delivery, json!({"allow": null})).await;
    assert_eq!(done(run.step.unwrap()).diagnostic, Some("behind_base"));
    assert_eq!(updates(&mock).len(), 2);
}

#[tokio::test]
async fn green_checks_while_github_recomputes_mergeability_keep_the_wait_open() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    // Main just moved: GitHub has not computed whether b is behind yet.
    let recomputing = || {
        pull(
            7,
            json!({"mergeable": null, "mergeable_state": "unknown",
        "head": {"ref": format!("aq/{TASK}"), "sha": sha('b')}}),
        )
    };
    poll(&mock, recomputing(), recomputing(), green());
    let run = wait(&delivery, json!({"allow": true})).await;
    assert_eq!((run.step, run.refreshed), (None, None));
    // Then it knows: behind is refreshed, up to date passes.
    poll(
        &mock,
        at('b', 'd', "behind"),
        at('b', 'd', "behind"),
        green(),
    );
    mock.reply(202, vec![], json!({}));
    let run = wait(&delivery, json!({"allow": true})).await;
    assert!(matches!(run.refreshed, Some(Refreshed::Requested { .. })));
    poll(&mock, at('b', 'c', "clean"), at('b', 'c', "clean"), green());
    let run = wait(&delivery, json!({"allow": true})).await;
    assert_eq!(done(run.step.unwrap()).outcome, "passed");
}

#[tokio::test]
async fn a_head_and_base_already_asked_for_are_not_asked_for_again() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    // A refresh from b behind d was asked for and GitHub made nothing of it.
    let asked = json!({"from": sha('b'), "base": sha('d'), "allow": true});
    poll(
        &mock,
        at('b', 'd', "behind"),
        at('b', 'd', "behind"),
        green(),
    );
    let run = wait(&delivery, asked.clone()).await;
    assert_eq!(
        (run.step, run.refreshed),
        (None, Some(Refreshed::Queued(sha('b'))))
    );
    assert!(updates(&mock).is_empty());
    // Main moved on to e: that is a new pair.
    poll(
        &mock,
        at('b', 'e', "behind"),
        at('b', 'e', "behind"),
        green(),
    );
    mock.reply(202, vec![], json!({}));
    wait(&delivery, asked).await;
    assert_eq!(updates(&mock), [json!({"expected_head_sha": sha('b')})]);
}
