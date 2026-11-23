use super::*;

// The synthetic result fixtures do not pass through native offer publication.
pub(super) async fn seed_native_offer_clock(pool: &sqlx::PgPool, attempt: Uuid) {
    sqlx::query("ALTER TABLE profiled_launches DISABLE TRIGGER profiled_launch_guard")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE profiled_launches SET graph_task_observation=jsonb_set(graph_task_observation,'{native_offer}',jsonb_build_object('offered',jsonb_build_object('clock',jsonb_build_object('server_time_ms',$2::BIGINT)))) WHERE attempt_id=$1")
        .bind(attempt).bind(NOW as i64).execute(pool).await.unwrap();
    sqlx::query("ALTER TABLE profiled_launches ENABLE TRIGGER profiled_launch_guard")
        .execute(pool)
        .await
        .unwrap();
}

/// MS1 retry (B4): after a final rework, the D3 policy actor records the
/// replacement facts that only owning domains prove (the central retention
/// receipt pinned by the report; a settled read-only AQ effect ledger), so the
/// failed attempt becomes replaceable. It never completes the task, never
/// releases the claim, and never touches a confirmed (Success) settlement.
#[tokio::test]
async fn reworked_failed_attempt_records_replacement_facts_and_becomes_replaceable() {
    use aq_admission::transport::ProcessOutcome;
    use aq_server::{
        execution::profiled::{Effects, Retention},
        profiled_execution::replacement::{self, Assessment},
    };
    let mut profile = writer_profile();
    profile["capabilities"] = json!(["read.get", "read.context"]);
    let env = Box::pin(auto_env_source(
        "not_required",
        axum::http::StatusCode::OK,
        ReportSetup {
            keep_checks: false,
            process_outcome: Some(ProcessOutcome::Failed),
            graph_profile: Some(profile.clone()),
            ..ReportSetup::default()
        },
    ))
    .await;
    let pool = env.f.db.pool().await;
    let attempt = env.report.binding.attempt_id;
    check_unsealed_facts(&env, &pool, attempt).await;
    // Synthetic sealed admission history, as settlement_effects.rs.
    seed_agent_binding(&env, &profile).await;

    // The replay of the final rework now records both facts; the verdict,
    // the claim and the Graph are unchanged.
    let s = env.settle().await;
    env.assert_not_completed(&s, AutoDisposition::ReworkRequired, "not_accepted")
        .await;
    let after = hot(&pool, attempt).await;
    let Retention::Retained { receipt } = &after.retention else {
        panic!("retained source fact expected: {:?}", after.retention);
    };
    assert_eq!(
        receipt.id.as_str(),
        format!("aq.git-import-receipt/{}", env.source)
    );
    assert!(matches!(after.effects, Effects::Quiescent { .. }));
    // Idempotent: a second replay changes nothing.
    let again = settlement::record_replacement_facts(&env.f.state, env.report.report_id)
        .await
        .unwrap();
    assert!(again.source_retained && again.effects_quiescent && !again.changed);
    assert_eq!(
        serde_json::to_value(hot(&pool, attempt).await).unwrap(),
        serde_json::to_value(&after).unwrap()
    );
    // The backfill cursor no longer lists it.
    assert!(
        settlement::reworked_without_facts(&env.f.state, None, 8)
            .await
            .unwrap()
            .is_empty()
    );

    // B4 now admits a replacement of the failed predecessor, which still holds
    // the task until the replacement owner releases it.
    seed_native_offer_clock(&pool, attempt).await;
    let task = env.f.selection.lineage.task_id;
    match replacement::assess_task(&env.f.state, task, NOW + 1)
        .await
        .unwrap()
    {
        Assessment::Replace(r) => {
            assert_eq!(r.predecessor, attempt);
            assert_eq!(r.attempts_started, 1);
            assert!(r.needs_release);
            assert_eq!(r.retained_source_receipt.id, receipt.id);
        }
        other => panic!("reworked failed attempt must be replaceable: {other:?}"),
    }
    assert!(env.holds_task().await);
}

/// 2026-11-21 (attachment task 8c6ba979, owner retry refused
/// `replacement_effects_unreconciled`): a returned attempt whose profile may write (here the
/// delivery fixture's `write.*` profile; `message.*` is the same verdict) has a final ledger,
/// so B4 records it quiescent and the attempt is replaceable once. Its acceptance verdict is
/// unchanged, `attempt.show`'s owner read names the missing fact before and none after, and
/// a replay changes nothing.
#[tokio::test]
async fn reworked_writer_attempt_with_final_ledger_becomes_replaceable() {
    use aq_admission::transport::ProcessOutcome;
    use aq_profiled_state::replacement_sql::inspect;
    use aq_server::{
        execution::profiled::Effects,
        profiled_execution::replacement::{self, Assessment},
    };
    let profile = writer_profile();
    assert!(profile["capabilities"].to_string().contains("write."));
    let env = Box::pin(auto_env_source(
        "not_required",
        axum::http::StatusCode::OK,
        ReportSetup {
            keep_checks: false,
            process_outcome: Some(ProcessOutcome::Failed),
            graph_profile: Some(profile.clone()),
            ..ReportSetup::default()
        },
    ))
    .await;
    let pool = env.f.db.pool().await;
    let attempt = env.report.binding.attempt_id;
    // B4 needs the original offer instant (as the read-only test sets it).
    seed_native_offer_clock(&pool, attempt).await;
    let shown = || async {
        let mut conn = pool.acquire().await.unwrap();
        inspect(&mut conn, attempt, NOW + 1).await.unwrap().unwrap()
    };
    let before = shown().await;
    assert_eq!(before["effects"]["kind"], "unknown", "{before}");
    let blockers = before["replacement"]["blockers"].to_string();
    assert!(blockers.contains("effects_unknown"), "{before}");
    seed_agent_binding(&env, &profile).await;
    let s = env.settle().await;
    env.assert_not_completed(&s, AutoDisposition::ReworkRequired, "not_accepted")
        .await;
    let after = shown().await;
    assert_eq!(after["effects"]["kind"], "quiescent", "{after}");
    assert_eq!(after["retention"]["kind"], "retained", "{after}");
    assert_eq!(after["replacement"]["blockers"], json!([]), "{after}");
    let again = settlement::record_replacement_facts(&env.f.state, env.report.report_id)
        .await
        .unwrap();
    assert!(again.effects_quiescent && !again.changed);
    assert_eq!(shown().await["effects"], after["effects"]);
    let v: Value =
        sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(&pool)
            .await
            .unwrap();
    let hot: aq_server::execution::authority::hot::HotState = serde_json::from_value(v).unwrap();
    assert!(matches!(hot.effects, Effects::Quiescent { .. }));
    let task = env.f.selection.lineage.task_id;
    let assessment = replacement::assess_task(&env.f.state, task, NOW + 1).await;
    let Assessment::Replace(r) = assessment.unwrap() else {
        panic!("a returned writer attempt with a final ledger is replaceable");
    };
    assert_eq!((r.predecessor, r.needs_release), (attempt, true));
    assert!(
        env.holds_task().await,
        "only the replacement owner releases"
    );
}

pub(super) async fn seed_agent_binding(env: &AutoEnv, profile: &Value) {
    let pool = env.f.db.pool().await;
    let attempt = env.report.binding.attempt_id;
    let mut tx = pool.begin().await.unwrap();
    let recorded = results::load_locked(&env.f.state, &mut tx, env.report.report_id)
        .await
        .unwrap();
    let SessionIdentity::Remote {
        node_id,
        boot_id,
        session_id,
        ..
    } = env.report.binding.session
    else {
        unreachable!()
    };
    let binding = json!({"task_id":recorded.spec.task.task_id,"attempt_id":attempt,"session_id":session_id,"generation":env.report.binding.assignment_generation,"node_id":node_id,"boot_id":boot_id,"project_id":recorded.spec.lifecycle.project_id,"workspace_id":recorded.spec.task.workspace_id,"resolved_spec_digest":env.report.binding.execution_spec_digest,"profile_digest":aq_agent_api::digest(profile).unwrap(),"context_digest":env.f.profiled.graph.context_digest});
    assert!(aq_agent_api::valid("Binding", &binding));
    sqlx::query("INSERT INTO agent_launch_bindings(attempt_id,node_id,binding,profile,context) VALUES($1,$2,$3,$4,'{}')").bind(attempt).bind(node_id).bind(&binding).bind(profile).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
}

/// The facts writer never acts on a confirmed (Success) settlement or on a
/// report that is not finally reworked.
#[tokio::test]
async fn replacement_facts_are_never_recorded_for_a_confirmed_result() {
    use aq_admission::transport::ProcessOutcome;
    use aq_server::execution::profiled::{Effects, Retention};
    let env = auto_env_with(
        "not_required",
        axum::http::StatusCode::OK,
        false,
        Some(ProcessOutcome::Success),
    )
    .await;
    let s = env.settle().await;
    assert_eq!(s.disposition, AutoDisposition::Confirmed, "{s:?}");
    let facts = settlement::record_replacement_facts(&env.f.state, env.report.report_id)
        .await
        .unwrap();
    assert!(!facts.changed && !facts.source_retained && !facts.effects_quiescent);
    let v: Value =
        sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(env.report.binding.attempt_id)
            .fetch_one(&env.f.db.pool().await)
            .await
            .unwrap();
    let hot: aq_server::execution::authority::hot::HotState = serde_json::from_value(v).unwrap();
    assert!(matches!(hot.retention, Retention::Pending));
    assert!(matches!(hot.effects, Effects::Unknown));
}

fn writer_profile() -> Value {
    let fixture: Value = serde_json::from_str(include_str!(
        "../../../../../../protocol/agent-access/v1/fixtures/mutation-delivery.json"
    ))
    .unwrap();
    fixture["proposal_profiles"][0]["profile"].clone()
}

async fn hot(pool: &sqlx::PgPool, attempt: Uuid) -> aq_server::execution::authority::hot::HotState {
    let v: Value =
        sqlx::query_scalar("SELECT state FROM profiled_authority_hot WHERE attempt_id=$1")
            .bind(attempt)
            .fetch_one(pool)
            .await
            .unwrap();
    serde_json::from_value::<aq_server::execution::authority::hot::HotState>(v).unwrap()
}

async fn check_unsealed_facts(env: &AutoEnv, pool: &sqlx::PgPool, attempt: Uuid) {
    use aq_server::execution::profiled::{Effects, Retention};
    // The rework was recorded before any source receipt or agent binding
    // existed: no fact could be proven, so none was written.
    let before = hot(pool, attempt).await;
    assert!(matches!(before.retention, Retention::Pending));
    assert!(matches!(before.effects, Effects::Unknown));
    let facts = settlement::record_replacement_facts(&env.f.state, env.report.report_id)
        .await
        .err();
    assert!(
        facts.is_some(),
        "no sealed agent binding: the ledger is unavailable"
    );
    assert!(
        matches!(hot(pool, attempt).await.retention, Retention::Pending),
        "all or nothing per call"
    );
}
