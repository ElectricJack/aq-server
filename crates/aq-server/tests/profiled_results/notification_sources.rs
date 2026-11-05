//! Notification observations of real result and review owners; no delivery HTTP.
use super::*;
use aq_server::notifications::{api, policy::Policy};

async fn policy(f: &Fixture) -> Policy {
    let pool = f.db.pool().await;
    sqlx::query("INSERT INTO project_execution_bindings(project_id,revision,binding,validated_at_ms,binding_observation) VALUES($1,1,$2,1,'{}') ON CONFLICT(project_id) DO UPDATE SET binding=project_execution_bindings.binding || EXCLUDED.binding")
        .bind(f.selection.lineage.project_id)
        .bind(json!({"project_id":f.selection.lineage.project_id,"enabled":true,"graph_workspace_id":f.selection.lineage.workspace_id}))
        .execute(&pool).await.unwrap();
    let service = api::service(&f.state);
    service.install(Uuid::new_v4()).await.unwrap();
    Policy::new(service)
}

#[tokio::test]
async fn retained_failure_and_success_correction_open_and_resolve_task_episode() {
    let (f, _, mut report) = prepare(true, true).await;
    let policy = policy(&f).await;
    let auth = context(&f).await;
    report.outcome = transport::ProcessOutcome::Failed;
    results::record(&f.state, &auth, &report).await.unwrap();
    assert_eq!(policy.reconcile_tasks().await.unwrap(), 1);
    assert_eq!(policy.reconcile_tasks().await.unwrap(), 0);
    let pool = f.db.pool().await;
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM notification_messages WHERE definition->>'summary' LIKE 'AQ attention: Task failed or blocked%'").fetch_one(&pool).await.unwrap(),1);
    report.supersedes = Some(report.report_id);
    report.report_id = Uuid::new_v4();
    report.operation_id = Uuid::new_v4();
    report.outcome = transport::ProcessOutcome::Success;
    results::record(&f.state, &auth, &report).await.unwrap();
    assert_eq!(policy.reconcile_tasks().await.unwrap(), 1);
    assert_eq!(policy.reconcile_tasks().await.unwrap(), 0);
    assert!(
        !sqlx::query_scalar::<_, bool>(
            "SELECT open FROM notification_episodes WHERE kind='task.failed_or_blocked'"
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    );
}

#[tokio::test]
async fn successful_process_with_held_default_settlement_needs_task_attention() {
    let body =
        r#"{"schema_version":1,"kind":"aq.default-completion.v1","delivery":"not_required"}"#;
    let definition = aq_server::profiled_execution::definitions::Definition::new(
        artifact("default-completion-policy", body.as_bytes()),
        ArtifactKind::Definition,
        body.into(),
    )
    .unwrap();
    let (f, _, report) = Box::pin(prepare_with_policy(
        true,
        true,
        None,
        false,
        Some(definition),
    ))
    .await;
    let policy = policy(&f).await;
    assert!(matches!(report.outcome, transport::ProcessOutcome::Success));
    results::record(&f.state, &context(&f).await, &report)
        .await
        .unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>(
            "SELECT disposition FROM profiled_auto_settlements WHERE report_id=$1"
        )
        .bind(report.report_id)
        .fetch_one(&f.db.pool().await)
        .await
        .unwrap(),
        "held"
    );
    assert_eq!(policy.reconcile_tasks().await.unwrap(), 1);
    assert_eq!(policy.reconcile_tasks().await.unwrap(), 0);
}

#[tokio::test]
async fn pending_review_owner_opens_once_and_cancel_resolves() {
    let env = Box::pin(review_env(2)).await;
    let policy = policy(&env.f).await;
    let request = aq_review_protocol::DecisionRequest {
        schema_version: 1,
        operation_id: Uuid::new_v4(),
        decision_id: Uuid::new_v4(),
        subject: env.prepared.subject().artifact().unwrap(),
        eligible_owners: vec![env.f.actor],
        eligibility_project_revision: 1,
        review_policy: env.prepared.subject().validation_policy.clone(),
        expires_at_ms: NOW + 30000,
    };
    Box::pin(env.service.admit(env.f.actor, &env.prepared, &request))
        .await
        .unwrap();
    assert_eq!(policy.reconcile_reviews().await.unwrap(), 1);
    assert_eq!(policy.reconcile_reviews().await.unwrap(), 0);
    Box::pin(
        env.service
            .cancel(env.f.actor, Uuid::new_v4(), request.decision_id, 1),
    )
    .await
    .unwrap();
    assert_eq!(policy.reconcile_reviews().await.unwrap(), 1);
    assert_eq!(policy.reconcile_reviews().await.unwrap(), 0);
    let pool = env.f.db.pool().await;
    assert_eq!(sqlx::query_scalar::<_, i64>("SELECT count(*) FROM notification_messages WHERE definition->>'summary' LIKE 'AQ attention: Decision needed%'").fetch_one(&pool).await.unwrap(),1);
    assert!(
        !sqlx::query_scalar::<_, bool>(
            "SELECT open FROM notification_episodes WHERE kind='decision.needed'"
        )
        .fetch_one(&pool)
        .await
        .unwrap()
    );
    env.peer.abort();
}
