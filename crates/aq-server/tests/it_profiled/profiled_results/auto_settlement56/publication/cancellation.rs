use super::*;

#[tokio::test]
async fn non_success_or_cancelled_report_never_reaches_aq_git() {
    use aq_admission::transport::ProcessOutcome;
    use aq_server::profiled_execution::settlement::publication;
    // A non-Success report is refused by D3 before any delivery.
    for outcome in [ProcessOutcome::Failed, ProcessOutcome::Unknown] {
        let p = publish_env_with(Some(outcome)).await;
        let calls = silent_git(&p.socket);
        let s =
            publication::deliver_then_settle(&p.env.f.state, &p.delivery, p.env.report.report_id)
                .await
                .unwrap()
                .unwrap();
        p.env
            .assert_not_completed(&s, AutoDisposition::ReworkRequired, "not_accepted")
            .await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            0,
            "{outcome:?} reached aq-git"
        );
        assert!(evidence_rows(&p.env).await.is_empty());
    }
    // An owner cancel of an accepted, held result lands before delivery.
    let p = publish_env().await;
    let calls = silent_git(&p.socket);
    let receipt = aq_server::profiled_execution::cancellation::cancel(
        &p.env.f.state,
        p.env.report.binding.attempt_id,
        Uuid::new_v4(),
        "owner cancel before publication",
    )
    .await
    .unwrap();
    assert_eq!(json!(receipt.disposition), "requested");
    let s = publication::deliver_then_settle(&p.env.f.state, &p.delivery, p.env.report.report_id)
        .await
        .unwrap()
        .unwrap();
    p.env
        .assert_not_completed(&s, AutoDisposition::ReworkRequired, "not_accepted")
        .await;
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "cancelled result reached aq-git"
    );
    assert!(evidence_rows(&p.env).await.is_empty());
}

#[tokio::test]
async fn cancel_between_candidate_and_publish_is_never_published() {
    use aq_server::profiled_execution::settlement::publication;
    let p = publish_env().await;
    let env = &p.env;
    let pool = env.f.db.pool().await;
    // The cancel lands while aq-git is building the candidate.
    let peer = git_peer_cancelling(
        pool.clone(),
        p.socket.clone(),
        vec![ready_candidate(&"c".repeat(40))],
        Some((0, env.f.state.clone(), env.report.binding.attempt_id)),
    )
    .await;
    let s = publication::deliver_then_settle(&env.f.state, &p.delivery, env.report.report_id)
        .await
        .unwrap()
        .unwrap();
    let (sent, cancelled) = peer.await.unwrap();
    assert_eq!(cancelled.as_deref(), Some("requested"));
    assert_eq!(sent.len(), 1, "only candidate.create reached aq-git");
    env.assert_not_completed(&s, AutoDisposition::ReworkRequired, "not_accepted")
        .await;
    let publishes: i64 =
        sqlx::query_scalar("SELECT count(*) FROM delivery_intents WHERE operation='publish'")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(publishes, 0, "no publish intent was ever frozen");
    assert!(evidence_rows(env).await.is_empty());
    assert!(head_of(env).await.is_none());
}

#[tokio::test]
async fn cancel_during_publishing_is_recorded_as_already_publishing() {
    use aq_server::profiled_execution::settlement::publication;
    let p = publish_env().await;
    let env = &p.env;
    let pool = env.f.db.pool().await;
    let base = admitted(env).await.resolved_object.hex;
    let candidate = "c".repeat(40);
    // The cancel lands while the publish request is in flight (fence held).
    let peer = git_peer_cancelling(
        pool.clone(),
        p.socket.clone(),
        vec![
            ready_candidate(&candidate),
            json!({"ok":true,"operation_state":"complete","result":{"outcome":"applied","target":"refs/heads/main",
                "old":{"algorithm":"sha1","hex":base},"new":{"algorithm":"sha1","hex":candidate},
                "acknowledged_at":"2026-11-05T00:00:00Z"}}),
        ],
        Some((1, env.f.state.clone(), env.report.binding.attempt_id)),
    )
    .await;
    let s = publication::deliver_then_settle(&env.f.state, &p.delivery, env.report.report_id)
        .await
        .unwrap()
        .unwrap();
    let (_, cancelled) = peer.await.unwrap();
    assert_eq!(cancelled.as_deref(), Some("already_publishing"));
    let cancellations: i64 =
        sqlx::query_scalar("SELECT count(*) FROM profiled_cancellations WHERE attempt_id=$1")
            .bind(env.report.binding.attempt_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(cancellations, 0, "the fenced publication was not cancelled");
    assert_eq!(s.disposition, AutoDisposition::Confirmed, "{s:?}");
    assert_eq!(head_of(env).await.unwrap().oid, candidate);
}

#[tokio::test]
async fn ambiguous_or_foreign_presence_stays_held_without_a_source_head() {
    use aq_server::profiled_execution::settlement::publication;
    let candidate = "c".repeat(40);
    for (case, reply) in [
        (
            "operation_indeterminate",
            json!({"ok":false,"operation_state":"pending","error":{"code":"operation_indeterminate",
            "message":"push outcome unknown","details":{"dispatched":true,"remote_effect_unknown":true},"retry":"reconcile_first"}}),
        ),
        (
            "observed_target_differs",
            json!({"ok":true,"operation_state":"complete","result":{"outcome":"already_present",
            "observed_target":{"algorithm":"sha1","hex":"d".repeat(40)},"observed_at":"2026-11-05T00:00:00Z",
            "caused_by_this_operation":false}}),
        ),
    ] {
        let p = publish_env().await;
        let env = &p.env;
        let peer = git_peer(
            env.f.db.pool().await,
            p.socket.clone(),
            vec![ready_candidate(&candidate), reply],
        )
        .await;
        let s = publication::deliver_then_settle(&env.f.state, &p.delivery, env.report.report_id)
            .await
            .unwrap()
            .unwrap();
        peer.await.unwrap();
        env.assert_not_completed(&s, AutoDisposition::Held, "delivery_indeterminate")
            .await;
        check_indeterminate_evidence(env, case).await;
        // Held, never re-pushed: a replay does no aq-git I/O.
        std::fs::remove_file(&p.socket).unwrap();
        let calls = silent_git(&p.socket);
        let again =
            publication::deliver_then_settle(&env.f.state, &p.delivery, env.report.report_id)
                .await
                .unwrap()
                .unwrap();
        assert_eq!(
            again.reason.as_deref(),
            Some("delivery_indeterminate"),
            "{case}"
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0, "{case}");
    }
}

async fn check_indeterminate_evidence(env: &AutoEnv, case: &str) {
    let rows = evidence_rows(env).await;
    assert_eq!(rows.len(), 1, "{case}");
    assert_eq!(rows[0].0, "indeterminate", "{case}");
    assert_eq!(rows[0].1["result"]["reason"], case, "{case}");
    assert!(
        rows[0].1["phase"].is_null(),
        "{case}: final evidence replaced the fence"
    );
    assert!(head_of(env).await.is_none(), "{case}");
}
