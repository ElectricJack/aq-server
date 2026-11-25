use super::*;
use crate::commands::{without_delivery, without_target};
async fn run(
    delivery: &Delivery,
    command: &str,
    request: Value,
    now: i64,
) -> Option<crate::commands::Run> {
    let operation = Uuid::from_u128(9);
    delivery
        .step(
            FrozenCommand::new(operation, command, &request),
            &target(),
            now,
        )
        .await
}
pub(super) async fn publish_and_refuse(delivery: &Delivery, git: &FakeGit) {
    let task = TASK.to_string();
    // publish: the lease is the one frozen with the request.
    let r = run(
        delivery,
        "branch.publish",
        json!({"task": task, "expected_old": sha('1')}),
        0,
    )
    .await;
    let Some(Step::Done(c)) = r.unwrap().step else {
        panic!("not done")
    };
    assert_eq!(c.outcome, "published");
    assert_eq!(git.request(0)["params"]["expected_old"]["hex"], sha('1'));
    // merge with no pull request: refused, nothing sent anywhere.
    let r = run(delivery,
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
}
pub(super) async fn await_checks(delivery: &Delivery, mock: &Mock) {
    let task = TASK.to_string();
    // await: keeps waiting while pending, and says what it saw; no pull request: closed.
    poll(
        mock,
        pull(7, json!({})),
        pull(7, json!({})),
        runs(&[("unit", "queued", None)]),
    );
    let r = run(
        delivery,
        "pull_request.await_checks",
        json!({"task": task, "pull_request": 7, "required": ["unit"], "deadline_at_ms": 5000}),
        1000,
    )
    .await
    .unwrap();
    assert!(r.step.is_none());
    assert_eq!(r.observed.unwrap().checks_state, "pending");
    let r = run(
        delivery,
        "pull_request.await_checks",
        json!({"task": task, "pull_request": null, "deadline_at_ms": 5000}),
        1000,
    )
    .await;
    let Some(Step::Done(c)) = r.unwrap().step else {
        panic!("not done")
    };
    assert_eq!(c.outcome, "closed");
}
pub(super) async fn malformed(delivery: &Delivery) {
    let task = TASK.to_string();
    // A request that does not fit its command is not run.
    assert!(
        run(delivery, "branch.publish", json!({}), 0)
            .await
            .is_none()
    );
    assert!(
        run(
            delivery,
            "pull_request.await_checks",
            json!({"task": task, "pull_request": 7}),
            0
        )
        .await
        .is_none()
    );
    assert!(
        run(delivery, "work.complete", json!({"task": task}), 0)
            .await
            .is_none()
    );
}
pub(super) fn unconfigured() {
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

#[tokio::test]
async fn a_frozen_request_runs_as_the_command_it_was_accepted_for() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    publish_and_refuse(&delivery, &git).await;
    await_checks(&delivery, &mock).await;
    malformed(&delivery).await;
    unconfigured();
}
