//! Synthetic host proof, real PostgreSQL owners and authenticated HTTP routes.
mod common;
use aq_server::profiled_execution::{disposition, replacement, started, stopped};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use common::{
    native_started::{context, setup, state, stop_request},
    profiled::Fixture,
};
use serde_json::{Value, json};
use tower::ServiceExt;
use uuid::Uuid;

fn request() -> disposition::Request {
    disposition::Request {
        operation_id: Uuid::new_v4(),
        reason: "owner retires failed run".into(),
    }
}
async fn call(
    f: &Fixture,
    attempt: Uuid,
    method: &str,
    body: Value,
    email: &str,
) -> (StatusCode, Value) {
    let response = aq_server::build_admin_router(f.state.clone())
        .oneshot(
            Request::builder()
                .method(method)
                .uri(format!("/api/execution/attempts/{attempt}/disposition"))
                .header("content-type", "application/json")
                .header("x-aq-dev-email", email)
                .header("X-AQ-Request", "1")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 16384).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
}
async fn held(f: &Fixture, attempt: Uuid) -> bool {
    sqlx::query_scalar("SELECT holds_task FROM execution_attempts WHERE attempt_id=$1")
        .bind(attempt)
        .fetch_one(&f.db.pool().await)
        .await
        .unwrap()
}

#[tokio::test]
async fn owner_disposes_stopped_unsettled_attempt_and_preserves_unresolved_evidence() {
    let (mut f, start) = setup(true).await;
    let auth = context(&f).await;
    started::record(&f.state, &auth, &start).await.unwrap();
    let attempt = start.binding.attempt_id;
    let request = request();
    assert_eq!(
        call(&f, attempt, "POST", json!(request), "owner@localhost")
            .await
            .0,
        409
    );
    assert!(held(&f, attempt).await, "running attempt must stay held");
    let stop = stop_request(&start);
    let stopped_receipt = stopped::record(&f.state, &auth, &stop).await.unwrap();
    let before = state(&f, &start).await;
    assert!(held(&f, attempt).await);
    // New-launch configuration and current node availability are not needed to
    // retire historical stopped work.
    let mut config = (*f.state.config).clone();
    config.profiled_execution = None;
    f.state.config = std::sync::Arc::new(config);
    let (status, receipt) = call(&f, attempt, "POST", json!(request), "owner@localhost").await;
    assert_eq!(status, 200, "{receipt}");
    assert_eq!(receipt["disposition"], "abandoned");
    assert_eq!(receipt["actor_id"], json!(f.actor));
    assert_eq!(receipt["task_id"], json!(f.selection.lineage.task_id));
    assert_eq!(
        receipt["stop_receipt_digest"],
        json!(stopped_receipt.artifact().unwrap().digest)
    );
    assert!(!held(&f, attempt).await);
    assert_eq!(
        state(&f, &start).await,
        before,
        "source, effects and stop evidence remain owned"
    );
    let task: Uuid = serde_json::from_value(receipt["task_id"].clone()).unwrap();
    assert_eq!(
        replacement::assess_task(&f.state, task, common::profiled::NOW)
            .await
            .unwrap(),
        replacement::Assessment::Blocked("task_terminally_disposed")
    );
    assert_eq!(
        json!(stopped::record(&f.state, &auth, &stop).await.unwrap()),
        json!(stopped_receipt)
    );
    assert_eq!(
        call(&f, attempt, "GET", Value::Null, "owner@localhost").await,
        (StatusCode::OK, receipt.clone())
    );
    // Reconnect the store as a server restart would; replay is durable.
    f.state.store = aq_server::store::Store::connect_with(f.db.options())
        .await
        .unwrap();
    assert_eq!(
        call(&f, attempt, "POST", json!(request), "owner@localhost").await,
        (StatusCode::OK, receipt)
    );
}

#[tokio::test]
async fn disposition_is_owner_only_and_exactly_idempotent_under_concurrent_requests() {
    let (f, start) = setup(true).await;
    let auth = context(&f).await;
    started::record(&f.state, &auth, &start).await.unwrap();
    stopped::record(&f.state, &auth, &stop_request(&start))
        .await
        .unwrap();
    let attempt = start.binding.attempt_id;
    let request = request();
    assert_eq!(
        call(&f, attempt, "POST", json!(request), "viewer@localhost")
            .await
            .0,
        403
    );
    assert_eq!(
        call(&f, attempt, "GET", Value::Null, "viewer@localhost")
            .await
            .0,
        403
    );
    let (a, b) = tokio::join!(
        call(&f, attempt, "POST", json!(request), "owner@localhost"),
        call(&f, attempt, "POST", json!(request), "owner@localhost"),
    );
    assert_eq!(a.0, 200, "{a:?}");
    assert_eq!(a, b);
    let mut changed = request.clone();
    changed.reason = "different".into();
    assert_eq!(
        call(&f, attempt, "POST", json!(changed), "owner@localhost")
            .await
            .0,
        409
    );
    changed = request.clone();
    changed.operation_id = Uuid::new_v4();
    assert_eq!(
        call(&f, attempt, "POST", json!(changed), "owner@localhost")
            .await
            .0,
        409
    );
    changed.operation_id = Uuid::nil();
    assert_eq!(
        call(&f, attempt, "POST", json!(changed), "owner@localhost")
            .await
            .0,
        400
    );
}

#[tokio::test]
async fn disposition_and_claim_release_roll_back_together_and_sql_rejects_bypass() {
    let (f, start) = setup(true).await;
    let auth = context(&f).await;
    started::record(&f.state, &auth, &start).await.unwrap();
    stopped::record(&f.state, &auth, &stop_request(&start))
        .await
        .unwrap();
    let pool = f.db.pool().await;
    let attempt = start.binding.attempt_id;
    assert!(
        sqlx::query("UPDATE execution_attempts SET holds_task=false WHERE attempt_id=$1")
            .bind(attempt)
            .execute(&pool)
            .await
            .is_err()
    );
    let stop_digest: String = sqlx::query_scalar("SELECT encode(sha256(receipt_bytes),'hex') FROM profiled_stopped_receipts WHERE attempt_id=$1")
        .bind(attempt).fetch_one(&pool).await.unwrap();
    let receipt = json!({"attempt_id":attempt,"task_id":f.selection.lineage.task_id,
        "operation_id":Uuid::new_v4(),"actor_id":f.actor,"disposition":"abandoned",
        "reason":"incomplete transaction","stop_receipt_digest":stop_digest,"recorded_at_ms":1});
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('aq.stopped_disposition_owner','1',true)")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("INSERT INTO profiled_stopped_dispositions(attempt_id,task_id,operation_id,actor_id,receipt) VALUES($1,$2,$3,$4,$5)")
        .bind(attempt).bind(f.selection.lineage.task_id)
        .bind(receipt["operation_id"].as_str().unwrap().parse::<Uuid>().unwrap())
        .bind(f.actor).bind(receipt).execute(&mut *tx).await.unwrap();
    assert!(
        tx.commit().await.is_err(),
        "a receipt without claim release cannot commit"
    );
    sqlx::raw_sql("CREATE FUNCTION fail_disposition_release() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF OLD.holds_task AND NOT NEW.holds_task THEN RAISE EXCEPTION 'synthetic release failure'; END IF; RETURN NEW; END $$; CREATE TRIGGER fail_disposition_release BEFORE UPDATE ON execution_attempts FOR EACH ROW EXECUTE FUNCTION fail_disposition_release();")
        .execute(&pool).await.unwrap();
    let request = request();
    assert_eq!(
        call(&f, attempt, "POST", json!(request), "owner@localhost")
            .await
            .0,
        500
    );
    assert!(held(&f, attempt).await);
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM profiled_stopped_dispositions")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 0);
    sqlx::query("DROP TRIGGER fail_disposition_release ON execution_attempts")
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&f, attempt, "POST", json!(request), "owner@localhost")
            .await
            .0,
        200
    );
    for query in [
        "DELETE FROM profiled_stopped_dispositions WHERE attempt_id=$1",
        "UPDATE profiled_stopped_dispositions SET receipt=receipt WHERE attempt_id=$1",
        "UPDATE execution_attempts SET holds_task=true WHERE attempt_id=$1",
    ] {
        assert!(
            sqlx::query(query)
                .bind(attempt)
                .execute(&pool)
                .await
                .is_err(),
            "{query}"
        );
    }
}

#[tokio::test]
async fn admitted_automatic_completion_cannot_be_terminally_disposed() {
    let (f, start) = setup(true).await;
    let auth = context(&f).await;
    started::record(&f.state, &auth, &start).await.unwrap();
    stopped::record(&f.state, &auth, &stop_request(&start))
        .await
        .unwrap();
    let attempt = start.binding.attempt_id;
    let pool = f.db.pool().await;
    let report = Uuid::new_v4();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT set_config('aq.settlement_owner','aq.settlement-owner.auto.1',true)")
        .execute(&mut *tx)
        .await
        .unwrap();
    // Synthetic admitted completion state exercises the competing-owner fence;
    // this test does not qualify a Graph completion receipt.
    sqlx::query("INSERT INTO profiled_auto_settlements(report_id,attempt_id,policy_actor,disposition,evidence,operation_id,workspace_id,task_id,expected_version,expected_content_revision,payload_bytes,created_at_ms,updated_at_ms) VALUES($1,$2,'aq.settlement.default-policy@1','prepared','{}',$3,$4,$5,1,1,$6,1,1)")
        .bind(report).bind(attempt).bind(Uuid::new_v4()).bind(f.selection.lineage.workspace_id)
        .bind(f.selection.lineage.task_id).bind(b"{}".as_slice()).execute(&mut *tx).await.unwrap();
    tx.commit().await.unwrap();
    for phase in ["prepared", "possible", "confirmed"] {
        if phase != "prepared" {
            let mut tx = pool.begin().await.unwrap();
            sqlx::query(
                "SELECT set_config('aq.settlement_owner','aq.settlement-owner.auto.1',true)",
            )
            .execute(&mut *tx)
            .await
            .unwrap();
            sqlx::query(
                "UPDATE profiled_auto_settlements SET disposition=$2,receipt=$3 WHERE report_id=$1",
            )
            .bind(report)
            .bind(phase)
            .bind((phase == "confirmed").then(|| json!({"synthetic":true})))
            .execute(&mut *tx)
            .await
            .unwrap();
            tx.commit().await.unwrap();
        }
        assert_eq!(
            call(&f, attempt, "POST", json!(request()), "owner@localhost")
                .await
                .0,
            409,
            "{phase}"
        );
        assert!(held(&f, attempt).await);
    }
}
