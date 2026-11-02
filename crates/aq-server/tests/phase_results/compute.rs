// Actual registration/SQL owners over the existing explicitly synthetic host
// proof fixtures. This is compute reuse, not physical runtime qualification.
use aq_server::profiled_execution::registration::{self, NativeRegistration};

async fn replacement(f: &terminal::Prepared) -> aq_admission::transport::Register {
    let row: Value = sqlx::query_scalar("SELECT state FROM execution_nodes WHERE node_id=$1")
        .bind(f.c.request.identity.node_id)
        .fetch_one(&f.c.env.env.pool)
        .await
        .unwrap();
    let mut request = serde_json::from_value::<NativeRegistration>(row["native"].clone())
        .unwrap()
        .request;
    request.operation_id = Uuid::new_v4();
    request.identity.boot_id = Uuid::new_v4();
    request
}

#[tokio::test]
async fn verified_stopped_frees_node_compute_without_releasing_task_or_requiring_result() {
    let (f, report) = Box::pin(ready()).await;
    let before = invariant(&f).await;
    let auth = terminal::auth(&f).await;
    let pool = &f.c.env.env.pool;
    let request = replacement(&f).await;
    let valid: bool = sqlx::query_scalar("SELECT profiled_compute_stopped_valid($1,$2)")
        .bind(report.binding.attempt_id)
        .bind(auth.id())
        .fetch_one(pool)
        .await
        .unwrap();
    assert!(valid);
    // A direct incarnation write cannot bypass the typed Rust validator just
    // by pointing at a plausible stopped projection or actual existing rows.
    assert!(sqlx::query("UPDATE execution_nodes SET state=jsonb_set(jsonb_set(state,'{identity}', $2),'{native,request,identity}',$2) WHERE node_id=$1")
        .bind(auth.id()).bind(json!(request.identity)).execute(pool).await.is_err());
    registration::register(&f.c.env.env.state, &auth, &request)
        .await
        .unwrap();
    assert_eq!(
        before,
        invariant(&f).await,
        "all task/source/result holds survive compute reuse"
    );
    assert!(before["held"].as_bool().unwrap());
    assert_eq!(before["release"], 0);
    assert_eq!(
        sqlx::query_scalar::<_, i64>(
            "SELECT count(*) FROM profiled_result_reports WHERE attempt_id=$1"
        )
        .bind(report.binding.attempt_id)
        .fetch_one(pool)
        .await
        .unwrap(),
        0
    );
    // The stop is historical: a later current incarnation must not invalidate
    // its exact original registration/phase/terminal provenance.
    let next = replacement(&f).await;
    registration::register(&f.c.env.env.state, &auth, &next)
        .await
        .unwrap();
    assert_eq!(before, invariant(&f).await);
    let receipt = results::record(&f.c.env.env.state, &auth, &report)
        .await
        .unwrap();
    assert_eq!(
        receipt.binding, report.binding,
        "the old stopped attempt can still record its exact result"
    );
    assert_eq!(before, invariant(&f).await, "recording is not task release");
}

#[tokio::test]
async fn running_or_missing_stopped_owner_keeps_node_compute_busy() {
    let (f, report) = Box::pin(ready_with_stop(false)).await;
    let before = invariant(&f).await;
    let auth = terminal::auth(&f).await;
    let request = replacement(&f).await;
    assert!(
        !sqlx::query_scalar::<_, bool>("SELECT profiled_compute_stopped_valid($1,$2)")
            .bind(report.binding.attempt_id)
            .bind(auth.id())
            .fetch_one(&f.c.env.env.pool)
            .await
            .unwrap()
    );
    assert!(
        registration::register(&f.c.env.env.state, &auth, &request)
            .await
            .is_err()
    );
    assert_eq!(before, invariant(&f).await);
}
