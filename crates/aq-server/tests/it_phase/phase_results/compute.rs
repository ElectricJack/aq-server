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
    // The stop's own transaction recorded the release (migration 0136): occupancy is that
    // row, and the proof is not run again by any read.
    let recorded = "SELECT proof FROM execution_attempt_compute_releases WHERE attempt_id=$1";
    let holds = "SELECT native_attempt_holds_compute($1,$2)";
    let attempt = report.binding.attempt_id;
    let proof = |pool| sqlx::query_scalar::<_, String>(recorded).bind(attempt).fetch_optional(pool);
    let held = |pool| sqlx::query_scalar::<_, bool>(holds).bind(attempt).bind(auth.id()).fetch_one(pool);
    assert_eq!(proof(pool).await.unwrap().as_deref(), Some("stopped"));
    assert!(!held(pool).await.unwrap());
    // It is immutable and only its validating owner writes it.
    assert!(sqlx::query("DELETE FROM execution_attempt_compute_releases WHERE attempt_id=$1").bind(attempt).execute(pool).await.is_err());
    assert!(sqlx::query("INSERT INTO execution_attempt_compute_releases SELECT gen_random_uuid(),node_id,proof,stop_receipt_digest,witnesses FROM execution_attempt_compute_releases").execute(pool).await.is_err());
    // A format-135 database has the stopped attempt and no recorded release: the attempt
    // counts as occupying until the migration's backfill validates its proof once.
    for sql in [
        "ALTER TABLE execution_attempt_compute_releases DISABLE TRIGGER execution_attempt_compute_release_guard",
        "DELETE FROM execution_attempt_compute_releases",
        "ALTER TABLE execution_attempt_compute_releases ENABLE TRIGGER execution_attempt_compute_release_guard",
    ] {
        sqlx::query(sql).execute(pool).await.unwrap();
    }
    assert!(held(pool).await.unwrap());
    sqlx::query("SELECT native_compute_release_record(attempt_id) FROM execution_attempts WHERE holds_task AND kind='profiled_v1'")
        .execute(pool)
        .await
        .unwrap();
    assert_eq!(proof(pool).await.unwrap().as_deref(), Some("stopped"));
    assert!(!held(pool).await.unwrap());
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
    // No proof, no recorded release: the backfill records nothing and the slot stays occupied.
    let pool = &f.c.env.env.pool;
    let recorded: bool = sqlx::query_scalar("SELECT native_compute_release_record($1)")
        .bind(report.binding.attempt_id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert!(!recorded);
    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM execution_attempt_compute_releases")
        .fetch_one(pool)
        .await
        .unwrap();
    assert_eq!(rows, 0);
    assert!(
        sqlx::query_scalar::<_, bool>("SELECT native_attempt_holds_compute($1,$2)")
            .bind(report.binding.attempt_id)
            .bind(auth.id())
            .fetch_one(pool)
            .await
            .unwrap()
    );
}
