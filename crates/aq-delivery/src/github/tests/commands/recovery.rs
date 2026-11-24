use super::*;
pub(super) fn merged(head: char) -> Value {
    pull(
        7,
        json!({
            "state": "closed", "merged": true, "merge_commit_sha": sha('d'),
            "head": {"ref": format!("aq/{TASK}"), "sha": sha(head)},
        }),
    )
}
pub(super) async fn poll_merged(
    delivery: &Delivery,
    target: &DeliveryTarget,
) -> crate::commands::Run {
    delivery
        .step(
            FrozenCommand::new(
                Uuid::from_u128(9),
                "pull_request.await_checks",
                &json!({
                    "task": TASK, "pull_request": 7, "result_head": sha('a'),
                    "required": ["check", "deployment"], "deadline_at_ms": 1000,
                }),
            ),
            target,
            1000,
        )
        .await
        .unwrap()
}
pub(super) async fn reconcile(delivery: &Delivery, target: &DeliveryTarget) -> Completion {
    done(
        delivery
            .pull_request_merge(
                TaskOperation::new(Uuid::from_u128(10), target, TASK),
                7,
                MergeGuard {
                    method: MergeMethod::Merge,
                    expected_head: &sha('a'),
                    result_head: None,
                },
            )
            .await,
    )
}
