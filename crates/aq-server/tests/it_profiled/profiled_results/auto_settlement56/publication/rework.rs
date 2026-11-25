use super::*;

/// Review R1-M2: a Failed report whose agent said `Outcome: blocked` is reworked
/// with the reason `agent_blocked`; any other failure stays `not_accepted`.
#[tokio::test]
async fn a_blocked_agent_is_reworked_as_agent_blocked() {
    use aq_admission::transport::ProcessOutcome;
    for (conclusion, reason) in [
        (
            "Outcome: blocked; Changes: none; Copied: none; Claude exit 0; Result: no access to the repo",
            "agent_blocked",
        ),
        (
            "Outcome: done; Changes: none; Copied: none; Claude exit 1; Result: crashed",
            "not_accepted",
        ),
    ] {
        let env = Box::pin(auto_env_source(
            "not_required",
            axum::http::StatusCode::OK,
            ReportSetup {
                keep_checks: false,
                process_outcome: Some(ProcessOutcome::Failed),
                changed: false,
                conclusion: Some(conclusion),
                ..ReportSetup::default()
            },
        ))
        .await;
        let s = env.settle().await;
        env.assert_not_completed(&s, AutoDisposition::ReworkRequired, reason)
            .await;
    }
}

/// Review I-2: a worker report whose agent claimed changes (or copied back
/// only files Git ignores) while the retained source is unchanged is refused
/// for rework, never completed as `no_change`; a claim of none completes.
#[tokio::test]
async fn claimed_changes_that_did_not_land_are_reworked() {
    for (conclusion, expected) in [
        (
            "Changes: src/lib.rs; Copied: none; Claude exit 0; Result: done",
            Some("claimed_changes_missing"),
        ),
        (
            "Changes: target/debug/aq-server; Claude exit 0; Summary: built",
            Some("claimed_changes_missing"),
        ),
        (
            "Changes: none; Copied: target/x; Claude exit 0; Result: CI passes",
            None,
        ),
    ] {
        let env = Box::pin(auto_env_source(
            "not_required",
            axum::http::StatusCode::OK,
            ReportSetup {
                keep_checks: false,
                changed: false,
                conclusion: Some(conclusion),
                ..ReportSetup::default()
            },
        ))
        .await;
        let s = env.settle().await;
        match expected {
            Some(reason) => {
                env.assert_not_completed(&s, AutoDisposition::ReworkRequired, reason)
                    .await
            }
            None => assert_eq!(
                s.disposition,
                AutoDisposition::Confirmed,
                "{conclusion}: {s:?}"
            ),
        }
    }
}

#[tokio::test]
async fn moved_target_is_unpublished_rework_and_never_advances_source() {
    use aq_server::profiled_execution::settlement::publication;
    let p = publish_env().await;
    let env = &p.env;
    let pool = env.f.db.pool().await;
    let peer = git_peer(
        pool.clone(),
        p.socket.clone(),
        vec![
            ready_candidate(&"c".repeat(40)),
            json!({"ok":true,"operation_state":"complete","result":{"outcome":"rejected","reason":"target_moved",
                "observed_target":{"algorithm":"sha1","hex":"d".repeat(40)},"observed_at":"2026-11-05T00:00:00Z"}}),
        ],
    )
    .await;
    let s = publication::deliver_then_settle(&env.f.state, &p.delivery, env.report.report_id)
        .await
        .unwrap()
        .unwrap();
    peer.await.unwrap();
    env.assert_not_completed(&s, AutoDisposition::ReworkRequired, "delivery_unpublished")
        .await;
    let source = &admitted(env).await;
    assert!(
        publication::current_head(
            &pool,
            source.project_id,
            source.repository_id,
            source.repository_binding_generation,
            "refs/heads/main",
        )
        .await
        .unwrap()
        .is_none()
    );
    // B4 never replaces it from the same immutable export: the conflict would recur.
    let mut tx = pool.begin().await.unwrap();
    let task = results::load_locked(&env.f.state, &mut tx, env.report.report_id)
        .await
        .unwrap()
        .spec
        .task
        .task_id;
    tx.rollback().await.unwrap();
    let assessed = aq_server::profiled_execution::replacement::assess_task(&env.f.state, task, NOW)
        .await
        .unwrap();
    assert!(
        matches!(
            assessed,
            aq_server::profiled_execution::replacement::Assessment::Blocked(
                "replacement_after_unpublished_delivery_needs_new_source"
            )
        ),
        "{assessed:?}"
    );
}
