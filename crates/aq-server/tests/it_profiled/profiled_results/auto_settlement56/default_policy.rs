use super::*;

#[tokio::test]
async fn false_pass_result_does_not_complete_task() {
    let env = auto_env("not_required", axum::http::StatusCode::OK).await;
    // The worker claims success; with no server-owned evidence it only holds.
    let s = env.settle().await;
    env.assert_not_completed(&s, AutoDisposition::Held, "required_check_evidence_missing")
        .await;
    // Passing evidence for another source, checkpoint or validator never counts.
    let validator = env.validator.clone();
    let checkpoint = env.checkpoint.clone();
    env.check(
        CheckedSource {
            id: Uuid::new_v4(),
            checkpoint: &checkpoint,
        },
        &validator,
        "passed",
    )
    .await;
    env.check(
        CheckedSource {
            id: env.source,
            checkpoint: &"1".repeat(40),
        },
        &validator,
        "passed",
    )
    .await;
    let s = env.settle().await;
    env.assert_not_completed(&s, AutoDisposition::Held, "required_check_evidence_missing")
        .await;
    env.check(
        CheckedSource {
            id: env.source,
            checkpoint: &checkpoint,
        },
        &"c".repeat(64),
        "passed",
    )
    .await;
    let s = env.settle().await;
    env.assert_not_completed(&s, AutoDisposition::Held, "required_check_evidence_missing")
        .await;
    // Exactly bound evidence that FAILED refuses the result for rework.
    env.check(
        CheckedSource {
            id: env.source,
            checkpoint: &checkpoint,
        },
        &validator,
        "failed",
    )
    .await;
    let s = env.settle().await;
    env.assert_not_completed(&s, AutoDisposition::ReworkRequired, "required_check_failed")
        .await;
    assert_eq!(s.evidence["checks"][0]["verdict"], "failed", "{s:?}");
    // Rework is terminal for this report: later evidence cannot flip it.
    env.check(
        CheckedSource {
            id: env.source,
            checkpoint: &checkpoint,
        },
        &validator,
        "passed",
    )
    .await;
    let s = env.settle().await;
    env.assert_not_completed(&s, AutoDisposition::ReworkRequired, "required_check_failed")
        .await;
}

#[tokio::test]
async fn false_pass_result_does_not_complete_task_without_published_delivery() {
    let env = auto_env("required", axum::http::StatusCode::OK).await;
    let (checkpoint, validator) = (env.checkpoint.clone(), env.validator.clone());
    env.check(
        CheckedSource {
            id: env.source,
            checkpoint: &checkpoint,
        },
        &validator,
        "passed",
    )
    .await;
    let s = env.settle().await;
    env.assert_not_completed(&s, AutoDisposition::Held, "delivery_evidence_missing")
        .await;
    env.delivery("indeterminate").await;
    let s = env.settle().await;
    env.assert_not_completed(&s, AutoDisposition::Held, "delivery_indeterminate")
        .await;
    // Evidence that required delivery did not publish refuses the result.
    env.delivery("unpublished").await;
    let s = env.settle().await;
    env.assert_not_completed(&s, AutoDisposition::ReworkRequired, "delivery_unpublished")
        .await;
    assert_eq!(s.evidence["delivery"]["state"], "unpublished", "{s:?}");
    assert_eq!(s.evidence["checks"][0]["verdict"], "passed", "{s:?}");
}

#[tokio::test]
async fn accepted_result_completes_task_with_expected_version() {
    let env = auto_env("required", axum::http::StatusCode::OK).await;
    let (checkpoint, validator) = (env.checkpoint.clone(), env.validator.clone());
    env.check(
        CheckedSource {
            id: env.source,
            checkpoint: &checkpoint,
        },
        &validator,
        "passed",
    )
    .await;
    env.delivery("published").await;
    let s = env.settle().await;
    assert_eq!(s.disposition, AutoDisposition::Confirmed, "{s:?}");
    let op = s.operation_id.unwrap();
    assert_eq!(s.dispatch_count, 1);
    assert_eq!(env.writes.load(Ordering::SeqCst), 1);
    {
        let bodies = env.bodies.lock().unwrap();
        let body = &bodies[0];
        assert_eq!(
            body["idempotency_key"],
            format!("aq-auto-settlement-v1:{op}")
        );
        assert_eq!(body["operations"][0]["op"], "update_node");
        assert_eq!(
            body["operations"][0]["id"],
            json!(env.f.selection.lineage.task_id)
        );
        assert_eq!(
            body["operations"][0]["expected_version"],
            json!(env.expected_version)
        );
        assert_eq!(
            body["operations"][0]["patch"],
            json!({"task_state":"completed"})
        );
    }
    assert!(
        !env.holds_task().await,
        "confirmed receipt releases the claim"
    );
    // Replay and a duplicate worker re-POST keep the same operation, no new write.
    let again = env.settle().await;
    assert_eq!(again.disposition, AutoDisposition::Confirmed);
    assert_eq!(again.operation_id, Some(op));
    let auth = context(&env.f).await;
    results::record(&env.f.state, &auth, &env.report)
        .await
        .unwrap();
    assert_eq!(env.writes.load(Ordering::SeqCst), 1);
    assert_eq!(
        settlement_row(&env.f, env.report.report_id).await.0,
        "confirmed"
    );
}

#[tokio::test]
async fn version_conflict_holds_and_never_overwrites() {
    let env = auto_env("not_required", axum::http::StatusCode::CONFLICT).await;
    let (checkpoint, validator) = (env.checkpoint.clone(), env.validator.clone());
    env.check(
        CheckedSource {
            id: env.source,
            checkpoint: &checkpoint,
        },
        &validator,
        "passed",
    )
    .await;
    let s = env.settle().await;
    assert_eq!(s.disposition, AutoDisposition::Held, "{s:?}");
    assert_eq!(s.reason.as_deref(), Some("graph_version_conflict"));
    assert_eq!(env.writes.load(Ordering::SeqCst), 1);
    // Re-evaluation never re-dispatches the stale expected version.
    let s = env.settle().await;
    assert_eq!(s.reason.as_deref(), Some("graph_version_conflict"));
    assert_eq!(env.writes.load(Ordering::SeqCst), 1);
    assert!(env.holds_task().await);
}

#[tokio::test]
async fn parked_suspension_holds_as_suspended_until_successor_ack() {
    let env = auto_env("required", axum::http::StatusCode::OK).await;
    let (checkpoint, validator) = (env.checkpoint.clone(), env.validator.clone());
    env.check(
        CheckedSource {
            id: env.source,
            checkpoint: &checkpoint,
        },
        &validator,
        "passed",
    )
    .await;
    env.delivery("published").await;
    // H5: a recorded RetainedSuspended/Held parking report is neither success
    // nor failure, even with otherwise complete evidence.
    let pool = env.f.db.pool().await;
    let continuation = Uuid::new_v4();
    sqlx::query("INSERT INTO hibernation_suspensions(continuation_id,wait_id,predecessor_attempt_id,predecessor_node_id,intent,directive_packet,phase,parking,created_at_ms,updated_at_ms) VALUES($1,$2,$3,$4,'{}','{}','parked_retained','{}',$5,$5)")
        .bind(continuation).bind(Uuid::new_v4()).bind(env.report.binding.attempt_id)
        .bind(env.f.node.node_id).bind(NOW as i64)
        .execute(&pool).await.unwrap();
    let s = env.settle().await;
    env.assert_not_completed(&s, AutoDisposition::Held, "suspended")
        .await;
    assert_eq!(s.evidence["suspension"]["phase"], "parked_retained");
    for phase in ["parked_held", "continued"] {
        sqlx::query("UPDATE hibernation_suspensions SET phase=$2 WHERE continuation_id=$1")
            .bind(continuation)
            .bind(phase)
            .execute(&pool)
            .await
            .unwrap();
        let s = env.settle().await;
        env.assert_not_completed(&s, AutoDisposition::Held, "suspended")
            .await;
    }
    // After the successor's exact BootstrapAck the successor owns the task;
    // the predecessor's report still never settles it.
    sqlx::query(
        "UPDATE hibernation_suspensions SET phase='acknowledged',ack='{}' WHERE continuation_id=$1",
    )
    .bind(continuation)
    .execute(&pool)
    .await
    .unwrap();
    let s = env.settle().await;
    env.assert_not_completed(&s, AutoDisposition::Held, "continued_by_successor")
        .await;
}

/// MS1 review B1: a production-shaped offer (policy actor, no required checks,
/// delivery not required) completes only on a Success report. A failed or an
/// unproven (Unknown) run never completes the task or releases dependents.
#[tokio::test]
async fn failed_or_unknown_report_never_completes_a_task_without_required_checks() {
    use aq_admission::transport::ProcessOutcome;
    for outcome in [ProcessOutcome::Failed, ProcessOutcome::Unknown] {
        let env = auto_env_with(
            "not_required",
            axum::http::StatusCode::OK,
            false,
            Some(outcome),
        )
        .await;
        let s = env.settle().await;
        env.assert_not_completed(&s, AutoDisposition::ReworkRequired, "not_accepted")
            .await;
    }
    let env = auto_env_with(
        "not_required",
        axum::http::StatusCode::OK,
        false,
        Some(ProcessOutcome::Success),
    )
    .await;
    let s = env.settle().await;
    assert_eq!(s.disposition, AutoDisposition::Confirmed, "{s:?}");
    assert_eq!(env.writes.load(Ordering::SeqCst), 1);
    assert!(
        !env.holds_task().await,
        "confirmed receipt releases the claim"
    );
}
