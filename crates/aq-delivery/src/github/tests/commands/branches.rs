use super::*;

#[tokio::test]
async fn main_sync_retains_the_default_branch_and_tells_its_head() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    let c = done(delivery.main_sync(Uuid::from_u128(1), &target()).await);
    assert_eq!((c.outcome, c.output), ("synced", json!({"head": sha('9')})));
    let request = git.request(0);
    assert_eq!(
        (&request["operation"], &request["params"]),
        (&json!("repo.retain_ref"), &json!({"ref":"refs/heads/main"}))
    );
    assert_eq!(
        (
            &request["repository_id"],
            &request["expected_binding_generation"],
            &request["operation_id"]
        ),
        (
            &json!(Uuid::from_u128(7)),
            &json!(3),
            &json!(Uuid::from_u128(1))
        )
    );
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
            .branch_publish(
                TaskOperation::new(Uuid::from_u128(2), &target(), TASK),
                None,
                Some(&sha('a')),
            )
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
            .branch_publish(
                TaskOperation::new(Uuid::from_u128(3), &target(), TASK),
                Some(&old),
                None,
            )
            .await,
    );
    assert_eq!(git.request(1)["params"]["expected_old"]["hex"], old);
    assert_eq!(git.request(1)["params"]["update"], "replace");
    // The host gives aq-git the Settings deadline for a call that reaches the remote.
    let waiting = delivery.git_waiting(git.path(), std::time::Duration::from_secs(300));
    done(
        waiting
            .branch_publish(
                TaskOperation::new(Uuid::from_u128(5), &target(), TASK),
                Some(&old),
                None,
            )
            .await,
    );
    assert_eq!(git.request(0)["deadline_ms"], 30_000);
    assert_eq!(git.request(2)["deadline_ms"], 300_000);
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
            Some(hex) if hex == sha('d') || hex == sha('e') => json!({
                "outcome":"rejected","reason":"target_moved",
                "observed_target":{"algorithm":"sha1","hex":sha('d')}
            }),
            _ => {
                json!({"outcome":"already_present","observed_target":{"algorithm":"sha1","hex":sha('a')}})
            }
        }
    });
    let (delivery, _) = delivery(&git).await;
    for (head, outcome) in [
        ('a', "published"),
        ('b', "conflict"),
        ('c', "refused"),
        ('d', "published"),
        ('e', "conflict"),
    ] {
        let step = delivery
            .branch_publish(
                TaskOperation::new(Uuid::new_v4(), &target(), TASK),
                None,
                Some(&sha(head)),
            )
            .await;
        assert_eq!(done(step).outcome, outcome);
    }
}

#[tokio::test]
async fn publish_recheck_distinguishes_a_usable_lease_from_missing_access() {
    let git = FakeGit::start(healthy);
    let (delivery, mock) = delivery(&git).await;
    let reference = task_branch(TASK);
    let exact = |head| json!({"ref": reference, "object":{"type":"commit","sha":sha(head)}});
    mock.reply(200, vec![], json!([exact('a')]));
    mock.reply(200, vec![], json!([exact('b')]));
    mock.reply(200, vec![], json!([exact('c')]));
    mock.reply(
        200,
        vec![],
        json!([{"ref":format!("{reference}-other"),"object":{"type":"commit","sha":sha('c')}}]),
    );
    mock.reply(200, vec![], json!([]));
    mock.reply(404, vec![], json!({"message":"Not Found"}));
    for (lease, usable) in [
        (None, true),
        (Some(sha('b')), true),
        (None, false),
        (None, true),
        (Some(sha('b')), false),
        (None, false),
    ] {
        assert_eq!(
            delivery
                .publish_lease_usable(&target(), TASK, &sha('a'), lease.as_deref())
                .await,
            usable
        );
    }
    let get = ("GET".into(), format!("/git/matching-refs/heads/aq/{TASK}"));
    assert_eq!(paths(&mock), vec![get; 6]);
    assert!(git.operations().is_empty());
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
        Some(old) if old == sha('4') => json!({"outcome":"rejected","reason":"remote_rejected"}),
        _ => {
            json!({"error": {"code": "remote_unreachable", "message": "x", "retry": "safe_same_operation"}})
        }
    });
    let (delivery, _) = delivery(&git).await;
    let publish = async |old: Option<&str>| {
        done(
            delivery
                .branch_publish(
                    TaskOperation::new(Uuid::from_u128(4), &target(), TASK),
                    old,
                    None,
                )
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
    let rejected = publish(Some(&sha('4'))).await;
    assert_eq!(rejected.diagnostic, Some("remote_rejected"));
    assert_eq!(
        (rejected.outcome, rejected.output),
        ("refused", json!({"reason":"unreachable"}))
    );
    let c = publish(Some(&sha('3'))).await;
    assert!(c.diagnostic.is_none());
    assert_eq!(
        (c.outcome, c.output),
        ("refused", json!({"reason": "unreachable"}))
    );
}

/// 2026-11-25: a push whose outcome aq-git never learned held `aq/<task>`, and
/// every later publication was refused as plain `unreachable` for days.
#[tokio::test]
async fn a_destination_held_by_a_push_without_effect_is_released_and_published() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let blocker = Uuid::from_u128(9);
    let branch = format!("refs/heads/aq/{TASK}");
    for (remote, published) in [(sha('2'), true), (sha('5'), false)] {
        let held = AtomicBool::new(true);
        let held_ref = branch.clone();
        let git = FakeGit::start(move |r| match r["operation"].as_str().unwrap() {
            "publish" if held.load(Ordering::SeqCst) => {
                json!({"error": {"code": "operation_indeterminate",
                "message": "destination has an unresolved publication",
                "details": {"blocking_operation_id": blocker, "target": held_ref}}})
            }
            "operation.show" => json!({"generation": 2, "phase": "dispatched", "status": "pending",
                "metadata": {"candidate": sha('a'), "expected_old": sha('2'), "target": held_ref}}),
            "operation.reconcile" => {
                json!({"target_operation_id": blocker, "outcome": "indeterminate",
                "observed_target": {"algorithm": "sha1", "hex": remote}})
            }
            "operation.resolve" => {
                held.store(false, Ordering::SeqCst);
                json!({"target_operation_id": blocker, "resolution": {"outcome": "resolved"}})
            }
            _ => healthy(r),
        });
        let (delivery, _) = delivery(&git).await;
        let operation = Uuid::from_u128(4);
        let c = done(
            delivery
                .branch_publish(
                    TaskOperation::new(operation, &target(), TASK),
                    Some(&sha('2')),
                    None,
                )
                .await,
        );
        let show = &json!({"target_operation_id": blocker});
        assert_eq!(git.request(1)["params"], *show);
        assert_eq!(git.request(2)["params"]["expected_generation"], json!(2));
        if published {
            assert_eq!(
                (c.outcome, c.output),
                ("published", json!({"head": sha('a')}))
            );
            assert_eq!(
                git.operations(),
                [
                    "publish",
                    "operation.show",
                    "operation.reconcile",
                    "operation.resolve",
                    "publish"
                ]
            );
            let resolve = git.request(3);
            assert_eq!(resolve["params"]["disposition"], "permit_new_operation");
            assert_eq!(resolve["params"]["old_publishers_stopped"], true);
            // The held push is never resent; the new publication goes on under its own ID.
            assert_eq!(git.request(4)["operation_id"], json!(operation));
        } else {
            // The remote moved off the held push's lease: its effect is unknown.
            assert_eq!(c.diagnostic, Some("publication_indeterminate"));
            assert_eq!(
                (c.outcome, c.output),
                ("refused", json!({"reason": "unreachable"}))
            );
            assert_eq!(
                git.operations(),
                ["publish", "operation.show", "operation.reconcile"]
            );
        }
    }
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
