use super::*;

#[tokio::test]
async fn persistent_scheduling_hold_notifies_its_project_once_after_settling_time() {
    use aq_playbook_domain::scheduling_notices;
    let env = Box::pin(auto_env("not_required", axum::http::StatusCode::OK)).await;
    let pool = env.f.db.pool().await;
    let task = env.f.selection.lineage.task_id;
    let project = env.f.selection.lineage.project_id;
    let attempt = env.report.binding.attempt_id;
    sqlx::query("UPDATE supervisor_controls c SET enabled=true, profile='{}',approved_by=$2 FROM supervisor_conversation_ids i WHERE i.conversation_id=c.conversation_id AND i.project_id=$1")
        .bind(project).bind(env.f.actor).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO task_holds(comment_id,task_id,attempts,project_id,reason,attempt_id,comment_body,held_at) VALUES($1,$2,1,$3,'lineage_retry_budget_exhausted',$4,'budget exhausted',to_timestamp($5))")
        .bind(Uuid::new_v4()).bind(task).bind(project).bind(attempt).bind(NOW as f64/1000.0).execute(&pool).await.unwrap();
    assert_eq!(
        aq_machine_control::holds::unclaimed_count(&pool, &[task, Uuid::new_v4()])
            .await
            .unwrap(),
        1
    );
    let count = AtomicUsize::new(0);
    let send = async |_: &mut sqlx::Transaction<'_, sqlx::Postgres>,
                      scope: Option<Uuid>,
                      body: &str,
                      _: u64| {
        assert_eq!(scope, Some(project));
        for id in [task, attempt, env.report.report_id] {
            assert!(body.contains(&id.to_string()));
        }
        assert!(body.contains("lineage_retry_budget_exhausted"));
        count.fetch_add(1, Ordering::SeqCst);
        Ok::<_, ()>(Uuid::new_v4())
    };
    scheduling_notices::notify(&pool, NOW as i64, &send)
        .await
        .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 0);
    for _ in 0..2 {
        scheduling_notices::notify(&pool, NOW as i64 + 600_001, &send)
            .await
            .unwrap();
    }
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_physically_stopped_task_without_a_result_escalates_once_after_contention_clears() {
    let (f, _, report) = Box::pin(prepare(true, true)).await;
    let pool = f.db.pool().await;
    let project = f.selection.lineage.project_id;
    let attempt = report.binding.attempt_id;
    sqlx::query("UPDATE supervisor_controls SET enabled=true, profile='{}',approved_by=$2 WHERE conversation_id=(SELECT conversation_id FROM supervisor_conversation_ids WHERE project_id=$1)")
        .bind(project).bind(f.actor).execute(&pool).await.unwrap();
    let stopped: i64 = sqlx::query_scalar("SELECT (convert_from(receipt_bytes,'UTF8')::jsonb->>'recorded_at_ms')::bigint FROM profiled_stopped_receipts WHERE attempt_id=$1")
        .bind(attempt).fetch_one(&pool).await.unwrap();
    let count = AtomicUsize::new(0);
    let send = async |_: &mut sqlx::Transaction<'_, sqlx::Postgres>,
                      scope: Option<Uuid>,
                      body: &str,
                      _: u64| {
        assert_eq!(scope, Some(project));
        assert!(body.contains("stopped_without_result") && body.contains(&attempt.to_string()));
        assert!(!body.contains("submit task.retry"));
        count.fetch_add(1, Ordering::SeqCst);
        Ok::<_, ()>(Uuid::new_v4())
    };
    use aq_playbook_domain::scheduling_notices::notify;
    notify(&pool, stopped + 599_999, &send).await.unwrap();
    let mut busy = pool.begin().await.unwrap();
    sqlx::query("SELECT attempt_id FROM execution_attempts WHERE attempt_id=$1 FOR UPDATE")
        .bind(attempt)
        .execute(&mut *busy)
        .await
        .unwrap();
    notify(&pool, stopped + 600_000, &send).await.unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 0);
    busy.rollback().await.unwrap();
    for _ in 0..2 {
        notify(&pool, stopped + 660_000, &send).await.unwrap();
    }
    assert_eq!(count.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn repeated_publication_refusals_notify_once_for_the_current_unpublished_head() {
    use aq_playbook_domain::scheduling_notices::notify;
    let env = Box::pin(auto_env("not_required", axum::http::StatusCode::OK)).await;
    let pool = env.f.db.pool().await;
    let lineage = &env.f.selection.lineage;
    let task = lineage.task_id;
    let attempt = env.report.binding.attempt_id;
    let result = env.report.report_id;
    let head = "4".repeat(40);
    sqlx::query("UPDATE supervisor_controls c SET enabled=true,profile='{}',approved_by=$2 FROM supervisor_conversation_ids i WHERE i.conversation_id=c.conversation_id AND i.project_id=$1")
        .bind(lineage.project_id).bind(env.f.actor).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO delivery_pushed_results(report_id,attempt_id,task_ref,head) VALUES($1,$2,$3,$4) ON CONFLICT(report_id) DO UPDATE SET head=EXCLUDED.head")
        .bind(result).bind(attempt).bind(format!("refs/heads/aq/{task}")).bind(&head).execute(&pool).await.unwrap();
    let ns = Uuid::new_v4();
    sqlx::query("INSERT INTO playbook_namespaces(namespace_id,generation,name,workspace_id,project_id,owner_user_id,catalog_digest,source_digest) VALUES($1,1,'publish',$2,$3,$4,repeat('a',64),repeat('b',64))")
        .bind(ns).bind(lineage.workspace_id).bind(lineage.project_id).bind(env.f.actor).execute(&pool).await.unwrap();
    let history = PublicationHistory {
        pool: &pool,
        ns,
        task,
        attempt,
    };
    let calls = AtomicUsize::new(0);
    let send = async |_: &mut sqlx::Transaction<'_, sqlx::Postgres>, scope, body: &str, _: u64| {
        assert_eq!(scope, Some(lineage.project_id));
        assert!(
            body.contains("branch_publication_refused")
                && body.contains(&result.to_string())
                && body.contains(&attempt.to_string())
                && body.contains(&head)
                && body.contains("rejection from the remote")
        );
        calls.fetch_add(1, Ordering::SeqCst);
        Ok::<_, ()>(Uuid::new_v4())
    };
    let due = NOW as i64 + 600_000;
    history.record(&head, result, "refused").await;
    history.record(&"5".repeat(40), result, "refused").await;
    history.record(&head, Uuid::new_v4(), "refused").await;
    notify(&pool, due, &send).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    history.check_remote_refusal(&head, result).await;
    notify(&pool, due - 1, &send).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    for _ in 0..2 {
        notify(&pool, due, &send).await.unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    sqlx::query("DELETE FROM delivery_notifications WHERE project_id=$1")
        .bind(lineage.project_id)
        .execute(&pool)
        .await
        .unwrap();
    history.record(&head, result, "published").await;
    notify(&pool, due, &send).await.unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        aq_playbook_domain::delivery_store::publication_refusal(
            &mut pool.acquire().await.unwrap(),
            result
        )
        .await
        .unwrap()
        .is_none()
    );
    assert!(env.holds_task().await);
}

#[tokio::test]
async fn expired_delivery_notice_reaches_the_project_once_for_the_current_held_result() {
    use aq_playbook_domain::{delivery_decisions, scheduling_notices};
    let env = Box::pin(auto_env("not_required", axum::http::StatusCode::OK)).await;
    let pool = env.f.db.pool().await;
    let task = env.f.selection.lineage.task_id;
    let project = env.f.selection.lineage.project_id;
    let result = env.report.report_id;
    sqlx::query("UPDATE supervisor_controls c SET enabled=true,profile='{}',approved_by=$2 FROM supervisor_conversation_ids i WHERE i.conversation_id=c.conversation_id AND i.project_id=$1")
        .bind(project).bind(env.f.actor).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO delivery_decisions(result_id,task_id,decision,reason,decided_at_ms) VALUES ($1,$2,'held','returns_exhausted',$3)")
        .bind(result).bind(task).bind(NOW as i64).execute(&pool).await.unwrap();
    let reasons = work_reasons(&env).await;
    assert!(reasons.iter().any(|r| r["code"] == "task_held"
        && r["hold_reason"] == "returns_exhausted"
        && r["result_id"] == json!(result)
        && r["blocker_owner"] == "project_owner"
        && r["wake_condition"] == "owner_retry_authorized"
        && r["message"].as_str().unwrap().contains(&result.to_string())));
    let key = delivery_decisions::notify_key(&format!("{result}:returns_exhausted")).unwrap();
    let calls = AtomicUsize::new(0);
    let send = async |_: &mut sqlx::Transaction<'_, sqlx::Postgres>,
                      scope: Option<Uuid>,
                      body: &str,
                      _: u64| {
        assert_eq!(scope, Some(project));
        assert!(
            body.contains(&task.to_string())
                && body.contains(&result.to_string())
                && body.contains("task.retry")
        );
        calls.fetch_add(1, Ordering::SeqCst);
        Ok::<_, ()>(Uuid::new_v4())
    };
    scheduling_notices::notify(&pool, NOW as i64, &send)
        .await
        .unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    for _ in 0..2 {
        scheduling_notices::notify(&pool, NOW as i64 + 600_001, &send)
            .await
            .unwrap();
    }
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(
        delivery_decisions::told(&mut pool.acquire().await.unwrap(), project, &key)
            .await
            .unwrap()
    );
}

/// Spec 0021 section 5.2: whatever its source, a hold tells the project's
/// supervisor once, with the facts and commands it needs.
fn assert_hold_facts(body: &str, ids: &[Uuid], reason: &str, attempt: (Uuid, &str)) {
    let (attempt, outcome) = attempt;
    for id in ids {
        assert!(body.contains(&id.to_string()), "{id} in {body}");
    }
    assert!(body.contains(reason), "{reason} in {body}");
    assert!(
        body.contains(&format!("attempt {attempt}: outcome {outcome}")),
        "the attempt and its outcome in {body}"
    );
    for command in [
        "task.work",
        "task.history",
        "task.retry",
        "attempt.abandon",
        "task.edit",
        "task.cancel",
    ] {
        assert!(body.contains(command), "{command} in {body}");
    }
}

#[tokio::test]
async fn every_kind_of_task_hold_notifies_its_project_once_and_a_new_hold_notifies_again() {
    use aq_playbook_domain::scheduling_notices::notify;
    let env = Box::pin(auto_env("not_required", axum::http::StatusCode::OK)).await;
    let pool = env.f.db.pool().await;
    let task = env.f.selection.lineage.task_id;
    let project = env.f.selection.lineage.project_id;
    let attempt = env.report.binding.attempt_id;
    sqlx::query("UPDATE supervisor_controls c SET enabled=true,profile='{}',approved_by=$2 FROM supervisor_conversation_ids i WHERE i.conversation_id=c.conversation_id AND i.project_id=$1")
        .bind(project).bind(env.f.actor).execute(&pool).await.unwrap();
    let outcome: String = sqlx::query_scalar("SELECT convert_from(report_bytes,'UTF8')::jsonb->>'outcome' FROM profiled_result_reports WHERE report_id=$1")
        .bind(env.report.report_id).fetch_one(&pool).await.unwrap();
    let bodies = std::sync::Mutex::new(Vec::<String>::new());
    let send = async |_: &mut sqlx::Transaction<'_, sqlx::Postgres>,
                      scope: Option<Uuid>,
                      body: &str,
                      _: u64| {
        assert_eq!(scope, Some(project));
        bodies.lock().unwrap().push(body.to_owned());
        Ok::<_, ()>(Uuid::new_v4())
    };
    let due = NOW as i64 + 600_001;
    // A budget hold, a never-granted hold, a policy hold, and the first
    // reason held again after its release.
    let reasons = [
        "lineage_retry_budget_exhausted",
        "never_granted_retry_bound_exhausted",
        "replacement_project_revision_changed",
        "lineage_retry_budget_exhausted",
    ];
    for (n, reason) in reasons.into_iter().enumerate() {
        sqlx::query("INSERT INTO task_holds(comment_id,task_id,attempts,project_id,reason,attempt_id,comment_body,held_at) VALUES($1,$2,1,$3,$4,$5,'held',to_timestamp($6))")
            .bind(Uuid::new_v4()).bind(task).bind(project).bind(reason).bind(attempt).bind(NOW as f64 / 1000.0)
            .execute(&pool).await.unwrap();
        // Reading the task, and every later sweep, tells nobody again.
        work_reasons(&env).await;
        for _ in 0..3 {
            notify(&pool, due, &send).await.unwrap();
        }
        let sent = bodies.lock().unwrap().clone();
        assert_eq!(sent.len(), n + 1, "one notice per hold: {reason}");
        assert_hold_facts(
            &sent[n],
            &[project, task, attempt],
            reason,
            (attempt, &outcome),
        );
        sqlx::query("UPDATE task_holds SET cleared_at=now(),cleared_by='claim_released' WHERE task_id=$1 AND cleared_at IS NULL")
            .bind(task).execute(&pool).await.unwrap();
        notify(&pool, due, &send).await.unwrap();
        assert_eq!(
            bodies.lock().unwrap().len(),
            n + 1,
            "a released hold is quiet"
        );
    }
}

#[tokio::test]
async fn a_delivery_hold_of_any_reason_notifies_once_and_never_after_the_playbook_did() {
    use aq_playbook_domain::{delivery_decisions, scheduling_notices::notify};
    let env = Box::pin(auto_env("not_required", axum::http::StatusCode::OK)).await;
    let pool = env.f.db.pool().await;
    let task = env.f.selection.lineage.task_id;
    let project = env.f.selection.lineage.project_id;
    let attempt = env.report.binding.attempt_id;
    let result = env.report.report_id;
    sqlx::query("UPDATE supervisor_controls c SET enabled=true,profile='{}',approved_by=$2 FROM supervisor_conversation_ids i WHERE i.conversation_id=c.conversation_id AND i.project_id=$1")
        .bind(project).bind(env.f.actor).execute(&pool).await.unwrap();
    let outcome: String = sqlx::query_scalar("SELECT convert_from(report_bytes,'UTF8')::jsonb->>'outcome' FROM profiled_result_reports WHERE report_id=$1")
        .bind(result).fetch_one(&pool).await.unwrap();
    let reason = "policy_hold";
    sqlx::query("INSERT INTO delivery_decisions(result_id,task_id,decision,reason,decided_at_ms) VALUES ($1,$2,'held',$3,$4)")
        .bind(result).bind(task).bind(reason).bind(NOW as i64).execute(&pool).await.unwrap();
    let key = delivery_decisions::notify_key(&format!("{result}:{reason}")).unwrap();
    let bodies = std::sync::Mutex::new(Vec::<String>::new());
    let send = async |_: &mut sqlx::Transaction<'_, sqlx::Postgres>,
                      scope: Option<Uuid>,
                      body: &str,
                      _: u64| {
        assert_eq!(scope, Some(project));
        bodies.lock().unwrap().push(body.to_owned());
        Ok::<_, ()>(Uuid::new_v4())
    };
    let due = NOW as i64 + 600_001;
    // The playbook's own notice for this hold is the one notice.
    delivery_decisions::record_told(
        &mut pool.acquire().await.unwrap(),
        project,
        &key,
        Uuid::new_v4(),
        NOW as i64,
    )
    .await
    .unwrap();
    notify(&pool, due, &send).await.unwrap();
    assert!(bodies.lock().unwrap().is_empty(), "no duplicate notice");
    // Without it, the hold still reaches the supervisor, once.
    sqlx::query("DELETE FROM delivery_notifications WHERE project_id=$1")
        .bind(project)
        .execute(&pool)
        .await
        .unwrap();
    for _ in 0..3 {
        notify(&pool, due, &send).await.unwrap();
    }
    let sent = bodies.lock().unwrap().clone();
    assert_eq!(sent.len(), 1);
    assert_hold_facts(
        &sent[0],
        &[project, task, attempt, result],
        reason,
        (attempt, &outcome),
    );
    assert!(
        delivery_decisions::told(&mut pool.acquire().await.unwrap(), project, &key)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn the_current_hold_followup_survives_concurrent_sweeps_and_respects_graph_closure() {
    use aq_playbook_domain::{delivery_decisions, scheduling_notices};
    let env = Box::pin(auto_env("not_required", axum::http::StatusCode::OK)).await;
    let pool = env.f.db.pool().await;
    let task = env.f.selection.lineage.task_id;
    let project = env.f.selection.lineage.project_id;
    let attempt = env.report.binding.attempt_id;
    let result = env.report.report_id;
    sqlx::query("UPDATE supervisor_controls c SET enabled=true,profile='{}',approved_by=$2 FROM supervisor_conversation_ids i WHERE i.conversation_id=c.conversation_id AND i.project_id=$1")
        .bind(project).bind(env.f.actor).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO delivery_decisions(result_id,task_id,decision,reason,decided_at_ms) VALUES ($1,$2,'held','returns_exhausted',$3)")
        .bind(result).bind(task).bind(NOW as i64).execute(&pool).await.unwrap();
    // Start from an existing playbook notice, as installations do at upgrade.
    let key = delivery_decisions::notify_key(&format!("{result}:returns_exhausted")).unwrap();
    let initial = Uuid::new_v4();
    let now = NOW as i64;
    delivery_decisions::record_told(
        &mut pool.acquire().await.unwrap(),
        project,
        &key,
        initial,
        now,
    )
    .await
    .unwrap();
    let interval = 120_000;
    let limits = aq_role_settings::delivery::DeliveryLimits {
        hold_followup_interval_ms: interval as u64,
        ..Default::default()
    };
    sqlx::query("UPDATE role_settings SET delivery_limits=$1 WHERE singleton")
        .bind(serde_json::to_value(limits).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let count = AtomicUsize::new(0);
    let send = async |_: &mut sqlx::Transaction<'_, sqlx::Postgres>,
                      scope: Option<Uuid>,
                      body: &str,
                      _: u64| {
        assert_eq!(scope, Some(project));
        assert!(body.contains(
            "Verify retry authorization against the current hold before submitting a retry."
        ));
        let number = count.fetch_add(1, Ordering::SeqCst) + 1;
        assert!(body.contains(&format!("reminder {number} of 3")));
        Ok::<_, ()>(Uuid::new_v4())
    };
    scheduling_notices::notify(&pool, now + interval - 1, &send)
        .await
        .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 0);
    let reasons = work_reasons(&env).await;
    assert!(reasons.iter().any(|reason| {
        reason["code"] == "supervisor_hold_followup"
            && reason["attempt_id"] == json!(attempt)
            && reason["result_id"] == json!(result)
            && reason["blocker_owner"] == "project_supervisor"
            && reason["next_check_at_ms"] == now + interval
    }));
    for step in 1..=4 {
        let at = now + step * interval;
        let (one, two) = tokio::join!(
            scheduling_notices::notify(&pool, at, &send),
            scheduling_notices::notify(&pool, at, &send),
        );
        one.unwrap();
        two.unwrap();
        assert_eq!(count.load(Ordering::SeqCst), step.min(3) as usize);
    }
    let reasons = work_reasons(&env).await;
    assert!(reasons.iter().any(|reason| {
        reason["code"] == "supervisor_hold_followup"
            && reason["status"] == "owner_attention"
            && reason["blocker_owner"] == "project_owner"
            && reason["next_check_at_ms"] == now + 5 * interval
    }));
    // A current Graph closure ends the follow-up without a retry or a fourth mail.
    scheduling_notices::notify_with_graph(
        &pool,
        now + 5 * interval,
        async |workspace, seen| {
            assert_eq!(workspace, env.f.selection.lineage.workspace_id);
            assert_eq!(seen, task);
            Ok::<_, ()>(true)
        },
        &send,
    )
    .await
    .unwrap();
    assert_eq!(count.load(Ordering::SeqCst), 3);
    assert!(
        scheduling_notices::followup(&pool, project, task)
            .await
            .is_none()
    );
    let still_held: bool =
        sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(still_held, "the supervisor sweep does not release a claim");
}

struct PublicationHistory<'a> {
    pool: &'a sqlx::PgPool,
    ns: Uuid,
    task: Uuid,
    attempt: Uuid,
}
impl PublicationHistory<'_> {
    async fn check_remote_refusal(&self, head: &str, result: Uuid) {
        let known_refusal = self.record(head, result, "refused").await;
        // Simulate a newer publisher alongside the older, catalog-only receipt.
        sqlx::query("UPDATE playbook_host_operations SET request=request || '{\"answer_note\":\"remote_rejected\"}'::jsonb,completed_at_ms=completed_at_ms+1 WHERE operation_id=$1")
        .bind(known_refusal).execute(self.pool).await.unwrap();
        let detail = aq_playbook_domain::delivery_store::publication_refusal(
            &mut self.pool.acquire().await.unwrap(),
            result,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(detail.contains(&known_refusal.to_string()));
    }
    async fn record(&self, sha: &str, report: Uuid, status: &str) -> Uuid {
        let id = Uuid::new_v4();
        let request = json!({"result_ready":{"result_id":report,"attempt_id":self.attempt},"result_head":sha});
        let output = if status == "published" {
            json!({"head":sha})
        } else {
            json!({"reason":"unreachable"})
        };
        sqlx::query("INSERT INTO playbook_host_operations(operation_id,namespace_id,generation,intent_id,command_name,subject_id,subject_revision,request,status,output,created_at_ms,completed_at_ms) VALUES($1,$2,1,$3,'branch.publish',$4,0,$5,$6,$7,$8,$8)")
                .bind(id).bind(self.ns).bind(id.to_string()).bind(self.task.to_string()).bind(request).bind(status).bind(output).bind(NOW as i64).execute(self.pool).await.unwrap();
        id
    }
}
