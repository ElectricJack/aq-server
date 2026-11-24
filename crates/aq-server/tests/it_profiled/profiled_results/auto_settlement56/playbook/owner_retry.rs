use super::super::replacement::seed_agent_binding;
use super::*;

pub(super) async fn held_retry_env(outcome: aq_admission::transport::ProcessOutcome) -> AutoEnv {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../../../../protocol/agent-access/v1/fixtures/mutation-delivery.json"
    ))
    .unwrap();
    let profile = fixture["proposal_profiles"][0]["profile"].clone();
    let (f, _, mut report) = Box::pin(prepare_with_policy_checks(
        true,
        true,
        Some(profile.clone()),
        false,
        Some(default_policy("not_required")),
        false,
    ))
    .await;
    report.outcome = outcome;
    let env = finish_auto_env(
        f,
        report,
        axum::http::StatusCode::OK,
        ReportSource::Pushed { changed: true },
    )
    .await;
    seed_agent_binding(&env, &profile).await;
    env
}

#[tokio::test]
async fn owner_retry_proves_the_pushed_source_and_effects_of_a_failed_policy_hold() {
    let env = Box::pin(held_retry_env(
        aq_admission::transport::ProcessOutcome::Failed,
    ))
    .await;
    let task = env.f.selection.lineage.task_id;
    let pool = env.f.db.pool().await;
    let facts = || async {
        sqlx::query_as::<_,(String,String)>("SELECT state#>>'{retention,kind}',state#>>'{effects,kind}' FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(env.report.binding.attempt_id).fetch_one(&pool).await.unwrap()
    };
    settlement::recover_held_for_retry(&env.f.state, task)
        .await
        .unwrap();
    assert_eq!(facts().await, ("pending".into(), "unknown".into()));
    sqlx::query("INSERT INTO delivery_decisions(result_id,task_id,decision,reason,decided_at_ms) VALUES($1,$2,'held','policy',$3)")
        .bind(env.report.report_id).bind(task).bind(NOW as i64).execute(&pool).await.unwrap();
    for _ in 0..2 {
        settlement::recover_held_for_retry(&env.f.state, task)
            .await
            .unwrap();
        assert_eq!(facts().await, ("retained".into(), "quiescent".into()));
    }
    assert_eq!(env.writes.load(Ordering::SeqCst), 0);
    assert!(env.holds_task().await);
}

struct RetryHistory {
    env: AutoEnv,
    pool: sqlx::PgPool,
    task: Uuid,
    attempt: Uuid,
    result: Uuid,
    namespace: Uuid,
}
impl RetryHistory {
    async fn new() -> Self {
        let env = Box::pin(held_retry_env(
            aq_admission::transport::ProcessOutcome::Success,
        ))
        .await;
        let pool = env.f.db.pool().await;
        let lineage = &env.f.selection.lineage;
        let (task, attempt, result) = (
            lineage.task_id,
            env.report.binding.attempt_id,
            env.report.report_id,
        );
        let ns = Uuid::new_v4();
        sqlx::query("INSERT INTO playbook_namespaces(namespace_id,generation,name,workspace_id,project_id,owner_user_id,catalog_digest,source_digest) VALUES($1,1,'retry',$2,$3,$4,repeat('a',64),repeat('b',64))")
        .bind(ns).bind(lineage.workspace_id).bind(lineage.project_id).bind(env.f.actor).execute(&pool).await.unwrap();
        Self {
            env,
            pool,
            task,
            attempt,
            result,
            namespace: ns,
        }
    }
    async fn record(&self, command: &str, status: &str, report: Uuid) -> Uuid {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO playbook_host_operations(operation_id,namespace_id,generation,intent_id,command_name,subject_id,subject_revision,request,status,output,created_at_ms) VALUES($1,$2,1,$3,$4,$5,0,$6,$7,CASE WHEN $7='running' THEN NULL ELSE '{}'::jsonb END,$8)")
            .bind(id).bind(self.namespace).bind(id.to_string()).bind(command).bind(self.task.to_string())
            .bind(json!({"result_ready":{"result_id":report,"attempt_id":self.attempt},"result_head":"4".repeat(40),"pull_request":228}))
            .bind(status).bind(NOW as i64).execute(&self.pool).await.unwrap();
        id
    }
    async fn check_running_pr(&self) {
        let result = self.result;
        let check = self
            .record("pull_request.await_checks", "running", result)
            .await;
        let reasons = work_reasons(&self.env).await;
        assert!(reasons.iter().any(|r| {
            r["code"] == "result_ready_no_delivery_policy"
                && r["message"]
                    .as_str()
                    .is_some_and(|m| m.contains("PR #228") && m.contains(&"4".repeat(40)))
        }));
        sqlx::query("UPDATE playbook_host_operations SET status='passed',output='{}',completed_at_ms=$2 WHERE operation_id=$1")
        .bind(check).bind(NOW as i64).execute(&self.pool).await.unwrap();
        assert!(
            !work_reasons(&self.env)
                .await
                .iter()
                .any(|r| r["message"].as_str().is_some_and(|m| m.contains("PR #228")))
        );
    }
    fn notification(
        &self,
    ) -> (
        aq_playbook_domain::delivery_store::PgDelivery,
        aq_playbook_domain::host10::ResultReady,
        Value,
    ) {
        let port = aq_playbook_domain::delivery_store::PgDelivery(self.pool.clone());
        let origin = aq_playbook_domain::host10::ResultReady {
            task_id: self.task,
            result_id: self.result,
            project_id: self.env.f.selection.lineage.project_id,
            attempt_id: self.attempt,
            version: 1,
        };
        let notice = json!({"text":"Inspect this held result."});
        (port, origin, notice)
    }
    async fn check_behind_base(&self) {
        let task = self.task;
        let result = self.result;
        let lineage = &self.env.f.selection.lineage;
        sqlx::query("INSERT INTO delivery_decisions(result_id,task_id,decision,reason,decided_at_ms) VALUES($1,$2,'held','returns_exhausted',$3)")
        .bind(result).bind(task).bind(NOW as i64).execute(&self.pool).await.unwrap();
        self.record("pull_request.await_checks", "conflict", Uuid::new_v4())
            .await;
        assert_eq!(
            aq_delivery::git_grant::held_report(&self.pool, self.task)
                .await
                .unwrap(),
            None
        );
        let behind = self
            .record("pull_request.await_checks", "conflict", result)
            .await;
        sqlx::query("UPDATE playbook_host_operations SET request=request || '{\"answer_note\":\"behind_base\"}'::jsonb WHERE operation_id=$1")
        .bind(behind).execute(&self.pool).await.unwrap();
        let reasons = work_reasons(&self.env).await;
        assert!(
            reasons
                .iter()
                .any(|r| r["message"].as_str().is_some_and(|text| text
                    .contains("pull_request.await_checks")
                    && text.contains("behind its base")
                    && text.contains("qualifies for repair")
                    && text.contains(&"4".repeat(40))))
        );
        let (port, origin, notice) = self.notification();
        let mut tx = self.pool.begin().await.unwrap();
        let (_, body) = aq_playbook_domain::delivery_decisions::notification(
            &mut tx,
            &port,
            Some(origin),
            lineage.project_id,
            &notice,
        )
        .await
        .unwrap();
        assert!(body.contains("behind its base") && body.contains("qualifies for repair"));
        assert!(
            aq_playbook_domain::delivery_decisions::notification(
                &mut tx,
                &port,
                Some(origin),
                Uuid::new_v4(),
                &notice,
            )
            .await
            .is_err()
        );
        tx.rollback().await.unwrap();
    }
    async fn failed_log(&self, head: String, log: String) {
        sqlx::query("INSERT INTO delivery_pull_requests(task_id,number,url,state,mergeable,checks_state,checks_failed,checks_log_head,checks_log_tail,updated_at_ms) VALUES($1,228,'https://example.test/pr/228','open','mergeable','failed','[\"unit\"]',$2,$3,$4) ON CONFLICT(task_id) DO UPDATE SET checks_log_head=$2,checks_log_tail=$3")
                .bind(self.task).bind(head).bind(log).bind(NOW as i64)
                .execute(&self.pool).await.unwrap();
    }
    async fn check_failed_log(&self) {
        let task = self.task;
        let result = self.result;
        let lineage = &self.env.f.selection.lineage;
        // A failed required check also carries the redacted tail of its job
        // log, for the exact head it was read at (spec 0021 section 5.3).
        let failed = self
            .record("pull_request.await_checks", "failed", result)
            .await;
        sqlx::query("UPDATE playbook_host_operations SET completed_at_ms=$2 WHERE operation_id=$1")
            .bind(failed)
            .bind(NOW as i64 + 1)
            .execute(&self.pool)
            .await
            .unwrap();
        let log: String = (0..2000)
            .map(|i| format!("== unit ==\nline {i:04} of the failed job\n"))
            .chain(["FAILED test_flaky\n".to_owned()])
            .collect();
        self.failed_log("4".repeat(40), log.clone()).await;
        let shown = |reasons: &[Value]| {
            reasons.iter().any(|r| {
                r["message"].as_str().is_some_and(|m| {
                    m.contains("recorded failed from pull_request.await_checks")
                        && m.contains("untrusted")
                        && m.contains("FAILED test_flaky")
                })
            })
        };
        assert!(
            shown(&work_reasons(&self.env).await),
            "task.work shows the tail"
        );
        let (port, origin, notice) = self.notification();
        let mut tx = self.pool.begin().await.unwrap();
        let (_, body) = aq_playbook_domain::delivery_decisions::notification(
            &mut tx,
            &port,
            Some(origin),
            lineage.project_id,
            &notice,
        )
        .await
        .unwrap();
        tx.rollback().await.unwrap();
        assert!(
            body.contains("FAILED test_flaky"),
            "the notice shows the tail"
        );
        assert!(
            body.len() <= aq_playbook_domain::delivery_store::SUPERVISOR_MESSAGE_BYTES,
            "a long tail is cut to fit the notice, not the notice dropped: {}",
            body.len()
        );
        assert!(body.contains("Use task.retry"), "the instructions stay");
        // A tail read for another head is not this result's.
        self.failed_log("5".repeat(40), log).await;
        assert!(!shown(&work_reasons(&self.env).await));
        sqlx::query("DELETE FROM delivery_pull_requests WHERE task_id=$1")
            .bind(task)
            .execute(&self.pool)
            .await
            .unwrap();
        sqlx::query("DELETE FROM playbook_host_operations WHERE operation_id=$1")
            .bind(failed)
            .execute(&self.pool)
            .await
            .unwrap();
    }
    async fn check_merge_conflict(&self) {
        let task = self.task;
        let attempt = self.attempt;
        let result = self.result;
        let merge = self.record("pull_request.merge", "running", result).await;
        assert_eq!(
            aq_delivery::git_grant::held_report(&self.pool, self.task)
                .await
                .unwrap(),
            None
        );
        assert!(!work_reasons(&self.env).await.iter().any(|r| {
            r["message"]
                .as_str()
                .is_some_and(|s| s.contains("qualifies for repair"))
        }));
        sqlx::query("UPDATE playbook_host_operations SET status='conflict',output='{}',completed_at_ms=$2 WHERE operation_id=$1")
        .bind(merge).bind(NOW as i64).execute(&self.pool).await.unwrap();
        assert_eq!(
            aq_delivery::git_grant::held_report(&self.pool, self.task)
                .await
                .unwrap(),
            Some(result)
        );
        settlement::recover_held_for_retry(&self.env.f.state, task)
            .await
            .unwrap();
        let facts: (String,String)=sqlx::query_as("SELECT state#>>'{retention,kind}',state#>>'{effects,kind}' FROM profiled_authority_hot WHERE attempt_id=$1")
        .bind(attempt).fetch_one(&self.pool).await.unwrap();
        assert_eq!(facts, ("retained".into(), "quiescent".into()));
        assert!(self.env.holds_task().await);
        assert_eq!(self.env.writes.load(Ordering::SeqCst), 0);
    }
    async fn check_returned_budget_hold(&self) {
        let task = self.task;
        let attempt = self.attempt;
        let result = self.result;
        let lineage = &self.env.f.selection.lineage;
        // A return followed by a budget hold also carries the original PR
        // evidence, allowing a supervisor to compare independently recorded heads.
        sqlx::query("UPDATE delivery_decisions SET decision='returned',reason='merge_conflict',update_from_main=true WHERE result_id=$1")
        .bind(result).execute(&self.pool).await.unwrap();
        sqlx::query("UPDATE supervisor_controls c SET enabled=true,profile='{}',approved_by=$2 FROM supervisor_conversation_ids i WHERE i.conversation_id=c.conversation_id AND i.project_id=$1")
        .bind(lineage.project_id).bind(self.env.f.actor).execute(&self.pool).await.unwrap();
        sqlx::query("INSERT INTO task_holds(comment_id,task_id,attempts,project_id,reason,attempt_id,comment_body,held_at) VALUES($1,$2,1,$3,'lineage_cost_budget_exhausted',$4,'returned',to_timestamp($5))")
        .bind(Uuid::new_v4()).bind(task).bind(lineage.project_id).bind(attempt).bind(NOW as f64 / 1000.0).execute(&self.pool).await.unwrap();
        let reasons = work_reasons(&self.env).await;
        let evidence = format!("returned result {result}. Worker outcome: success.");
        assert!(reasons.iter().any(|r| {
            r["blocker_owner"] == "project_owner"
                && r["wake_condition"] == "owner_retry_authorized"
                && r["message"]
                    .as_str()
                    .is_some_and(|text| text.contains(&evidence) && text.contains(&"4".repeat(40)))
        }));
        assert!(
            aq_playbook_domain::delivery_store::returned_result_evidence(
                &mut self.pool.acquire().await.unwrap(),
                Uuid::new_v4(),
                task,
                attempt
            )
            .await
            .unwrap()
            .is_none()
        );
        sqlx::query("UPDATE delivery_decisions SET reason='checks_failed',detail='tests (shard 3/6)',update_from_main=false WHERE result_id=$1")
        .bind(result).execute(&self.pool).await.unwrap();
        assert!(work_reasons(&self.env).await.iter().any(|r| {
            r["message"]
                .as_str()
                .is_some_and(|m| m.contains("tests (shard 3/6)"))
        }));
        let calls = AtomicUsize::new(0);
        aq_playbook_domain::scheduling_notices::notify(
            &self.pool,
            NOW as i64 + 600_001,
            async |_, scope, body: &str, _| {
                assert_eq!(scope, Some(lineage.project_id));
                assert!(
                    body.contains("lineage_cost_budget_exhausted")
                        && body.contains(&"4".repeat(40))
                        && body.contains("conflict")
                        && body.contains(&evidence)
                        && body.contains("tests (shard 3/6)")
                );
                calls.fetch_add(1, Ordering::SeqCst);
                Ok::<_, ()>(Uuid::new_v4())
            },
        )
        .await
        .unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn liveness_owner_retry_reconciles_a_stopped_cancellation_with_a_pushed_failure() {
    use aq_delivery::git_grant::cancelled_failed_report;
    use aq_server::profiled_execution::{cancellation, replacement};

    let env = Box::pin(held_retry_env(
        aq_admission::transport::ProcessOutcome::Failed,
    ))
    .await;
    let task = env.f.selection.lineage.task_id;
    let attempt = env.report.binding.attempt_id;
    let pool = env.f.db.pool().await;
    super::super::replacement::seed_native_offer_clock(&pool, attempt).await;
    assert_eq!(cancelled_failed_report(&pool, task).await.unwrap(), None);
    let cancelled = cancellation::cancel(
        &env.f.state,
        attempt,
        Uuid::new_v4(),
        "Stop after the stored credential expired",
    )
    .await
    .unwrap();
    assert!(matches!(
        cancelled.disposition,
        cancellation::Disposition::Requested
    ));
    assert_eq!(
        cancelled_failed_report(&pool, task).await.unwrap(),
        Some(env.report.report_id)
    );
    assert_eq!(send_back(&env, "policy").await, Settled::Stale);
    assert_eq!(
        replacement::assess_task(&env.f.state, task, NOW + 1)
            .await
            .unwrap(),
        replacement::Assessment::Blocked("replacement_effects_unreconciled")
    );
    for _ in 0..2 {
        settlement::recover_held_for_retry(&env.f.state, task)
            .await
            .unwrap();
    }
    let facts: (String, String) = sqlx::query_as(
        "SELECT state#>>'{retention,kind}',state#>>'{effects,kind}'
         FROM profiled_authority_hot WHERE attempt_id=$1",
    )
    .bind(attempt)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(facts, ("retained".into(), "quiescent".into()));
    assert!(matches!(
        replacement::assess_task(&env.f.state, task, NOW + 1)
            .await
            .unwrap(),
        replacement::Assessment::Replace(_)
    ));
    assert!(env.holds_task().await);
    assert_eq!(env.writes.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn exhausted_success_retry_requires_a_final_pr_failure_for_this_exact_result() {
    let history = RetryHistory::new().await;
    history.check_running_pr().await;
    history.check_behind_base().await;
    history.check_failed_log().await;
    history.check_merge_conflict().await;
    history.check_returned_budget_hold().await;
}
