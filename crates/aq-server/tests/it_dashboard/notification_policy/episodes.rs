use super::*;
#[tokio::test]
async fn credential_invalid_notifies_once_per_episode() {
    let e = Env::new(true).await;
    let opened = e.event(Kind::CredentialInvalid, "profile-a", Transition::Opened);
    let first = e.policy.observe(opened.clone()).await.unwrap();
    assert_eq!(
        json!([
            first["outcome"],
            first["external_state"],
            first["generation"]
        ]),
        json!(["notified", "routed", 1])
    );
    assert_eq!(e.messages().await.len(), 1);
    let operations = e.operations().await;

    // A replayed source event creates no second intent and no new operation.
    assert_eq!(e.policy.observe(opened.clone()).await.unwrap(), first);
    assert_eq!(e.messages().await.len(), 1);
    assert_eq!(e.operations().await, operations);

    // The same episode reported again by a new source event stays silent.
    let again = e
        .policy
        .observe(e.event(Kind::CredentialInvalid, "profile-a", Transition::Opened))
        .await
        .unwrap();
    assert_eq!(again["outcome"], "already_open");
    assert_eq!(e.messages().await.len(), 1);

    // An independent subject is its own episode.
    let other = e
        .policy
        .observe(e.event(Kind::CredentialInvalid, "profile-b", Transition::Opened))
        .await
        .unwrap();
    assert_eq!(other["outcome"], "notified");
    assert_eq!(e.messages().await.len(), 2);

    // Resolve, then reopen: exactly one more notification, with a new identity.
    let resolved = e
        .policy
        .observe(e.event(Kind::CredentialInvalid, "profile-a", Transition::Resolved))
        .await
        .unwrap();
    assert_eq!(resolved["outcome"], "resolved");
    let quiet = e
        .policy
        .observe(e.event(Kind::CredentialInvalid, "profile-a", Transition::Resolved))
        .await
        .unwrap();
    assert_eq!(quiet["outcome"], "not_open");
    e.time.fetch_add(120_000, Ordering::SeqCst);
    let reopened = e
        .policy
        .observe(e.event(Kind::CredentialInvalid, "profile-a", Transition::Opened))
        .await
        .unwrap();
    assert_eq!(reopened["outcome"], "notified");
    assert_eq!(reopened["generation"], 2);
    assert_ne!(reopened["message_id"], first["message_id"]);
    let messages = e.messages().await;
    assert_eq!(messages.len(), 3);
    assert!(
        messages[0].contains("Credential invalid") && messages[0].contains("profile-a"),
        "{}",
        messages[0]
    );
    assert!(messages.iter().all(|m| m.len() <= 600));
}

#[tokio::test]
async fn flood_of_failures_collapses_to_bounded_notifications() {
    let e = Env::new(true).await;
    for n in 0..50 {
        let outcome = e
            .policy
            .observe(e.event(
                Kind::TaskFailedOrBlocked,
                &format!("task-{n}"),
                Transition::Opened,
            ))
            .await
            .unwrap();
        let expected = if n < 3 { "notified" } else { "collapsed" };
        assert_eq!(outcome["outcome"], expected, "event {n}");
    }
    assert_eq!(e.messages().await.len(), 3);
    // The window is still open: nothing to summarize yet.
    assert_eq!(e.policy.flush().await.unwrap(), 0);
    assert_eq!(e.messages().await.len(), 3);
    e.time.fetch_add(60_000, Ordering::SeqCst);
    assert_eq!(e.policy.flush().await.unwrap(), 1);
    assert_eq!(e.policy.flush().await.unwrap(), 0);
    let messages = e.messages().await;
    assert_eq!(messages.len(), 4, "{messages:?}");
    assert!(messages[3].contains("47 more"), "{}", messages[3]);
    // Collapsed episodes are still open: a repeat does not notify.
    let repeat = e
        .policy
        .observe(e.event(Kind::TaskFailedOrBlocked, "task-49", Transition::Opened))
        .await
        .unwrap();
    assert_eq!(repeat["outcome"], "already_open");
    assert_eq!(e.messages().await.len(), 4);
}

#[tokio::test]
async fn no_external_route_is_reported_not_claimed() {
    let e = Env::new(false).await;
    let outcome = e
        .policy
        .observe(e.event(Kind::QuestionAsked, "question-1", Transition::Opened))
        .await
        .unwrap();
    assert_eq!(outcome["outcome"], "notified");
    assert_eq!(outcome["external_state"], "no_external_route");
    let deliveries: i64 = sqlx::query_scalar("SELECT count(*) FROM notification_deliveries")
        .fetch_one(&e.policy.service().pool)
        .await
        .unwrap();
    assert_eq!(deliveries, 0);
}

#[test]
fn every_kind_has_a_stable_wire_name_and_label() {
    let names: Vec<_> = Kind::ALL.iter().map(|k| k.as_str()).collect();
    assert_eq!(
        names,
        [
            "task.failed_or_blocked",
            "decision.needed",
            "question.asked",
            "credential.invalid",
            "milestone.gate_ready",
            "worker.offline",
            "storage.pressure",
            "sandbox.preserved",
            "session.stalled"
        ]
    );
    assert_eq!(
        Kind::from_failure_reason("credential_invalid"),
        Some(Kind::CredentialInvalid)
    );
    assert_eq!(Kind::from_failure_reason("timeout"), None);
}
