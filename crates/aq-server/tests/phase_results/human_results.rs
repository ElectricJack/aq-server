// Actual SQL/router origins with synthetic fixture proof DTOs, not a harness run.
use aq_server::profiled_execution::human_results as api;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use tower::ServiceExt;

async fn get(f: &terminal::Prepared, path: &str) -> (StatusCode, Value) {
    let response = aq_server::build_admin_router(f.c.env.env.state.clone())
        .oneshot(
            Request::builder()
                .uri(path)
                .header("x-aq-dev-email", "owner@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    if status == StatusCode::OK {
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
    let bytes = to_bytes(response.into_body(), 262144).await.unwrap();
    let body = if status == StatusCode::OK {
        serde_json::from_slice(&bytes).unwrap()
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, body)
}
fn detail(report: &ResultReport, revision: &str, profile: bool) -> String {
    format!(
        "/api/human/results/{}/revisions/{revision}{}",
        report.report_id,
        if profile {
            "?profile=recorded_native_v2"
        } else {
            ""
        }
    )
}
#[tokio::test]
async fn opt_in_phase_metadata_lists_exact_sources_heads_and_current_owner_only() {
    let (f, report) = Box::pin(ready()).await;
    let (status, empty) = get(&f, "/api/human/results?profile=recorded_native_v2").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(empty["coverage"], api::NATIVE_V2_COVERAGE);
    assert!(empty["items"].as_array().unwrap().is_empty());
    let auth = terminal::auth(&f).await;
    let receipt = results::record(&f.c.env.env.state, &auth, &report)
        .await
        .unwrap();
    let mut correction = report.clone();
    correction.report_id = Uuid::new_v4();
    correction.operation_id = Uuid::new_v4();
    correction.supersedes = Some(report.report_id);
    correction.conclusion =
        Label::new("Corrected synthetic phase B1 claim; not accepted or settled").unwrap();
    results::record(&f.c.env.env.state, &auth, &correction)
        .await
        .unwrap();
    let invariants = invariant(&f).await;
    let retained = results::test_load_retained(&f.c.env.env.state, report.report_id)
        .await
        .unwrap();
    let (status, old) = get(&f, "/api/human/results").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(old["coverage"], api::COVERAGE);
    assert!(old["items"].as_array().unwrap().is_empty());
    assert_eq!(
        get(&f, &detail(&report, receipt.report_digest.as_str(), false))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let (status, first) = get(&f, "/api/human/results?profile=recorded_native_v2&limit=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(first["coverage"], api::NATIVE_V2_COVERAGE);
    assert_eq!(first["items"].as_array().unwrap().len(), 1);
    let cursor = first["next_after"].as_str().unwrap();
    let (status, second) = get(
        &f,
        &format!("/api/human/results?profile=recorded_native_v2&limit=1&after={cursor}"),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(second["next_after"].is_null());
    assert_eq!(second["items"].as_array().unwrap().len(), 1);
    assert!(
        first["items"][0]["report_id"].as_str().unwrap()
            < second["items"][0]["report_id"].as_str().unwrap()
    );
    for item in [&first["items"][0], &second["items"][0]] {
        assert_eq!(item["origin"], ORIGIN);
        assert_eq!(
            item["project_id"],
            retained["spec"]["lifecycle"]["project_id"]
        );
        assert_eq!(item["task_id"], retained["spec"]["task"]["task_id"]);
        assert_eq!(
            item["repository_id"],
            retained["spec"]["source"]["repository_id"]
        );
        assert_eq!(
            item["source_base"],
            retained["spec"]["source"]["resolved_object"]
        );
        assert_eq!(item["source_receipt_ids"], json!(report.source_receipt_ids));
        assert_eq!(item["pending_effect_ids"], json!(report.pending_effect_ids));
        for forbidden in [
            "credential",
            "execution_spec",
            "launch_manifest",
            "accepted",
            "settled",
            "eligible",
            "stop",
            "identity",
        ] {
            assert!(item.get(forbidden).is_none(), "unexpected {forbidden}");
        }
    }
    let path = detail(&report, receipt.report_digest.as_str(), true);
    let (status, exact) = get(&f, &path).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(exact["items"].as_array().unwrap().len(), 1);
    assert!(exact["next_after"].is_null());
    assert_eq!(exact["items"][0]["origin"], ORIGIN);
    assert_eq!(exact["items"][0]["current_report_head"], false);
    assert_eq!(
        exact["items"][0]["head_report_id"],
        json!(correction.report_id)
    );
    assert_eq!(
        get(&f, &detail(&report, &"f".repeat(64), true)).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&f, &format!("{path}&project_id={}", Uuid::new_v4()))
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        get(&f, "/api/human/results?profile=unknown").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get(
            &f,
            "/api/human/results?profile=recorded_native_v2&unknown=1"
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let (_, scoped) = get(
        &f,
        &format!(
            "/api/human/results?profile=recorded_native_v2&project_id={}",
            Uuid::new_v4()
        ),
    )
    .await;
    assert!(scoped["items"].as_array().unwrap().is_empty());
    assert_eq!(
        invariants,
        invariant(&f).await,
        "human reads must not settle or release"
    );
    sqlx::query("UPDATE users SET role='viewer' WHERE email='owner@localhost'")
        .execute(&f.c.env.env.pool)
        .await
        .unwrap();
    assert_eq!(get(&f, &path).await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        get(&f, "/api/human/results?profile=recorded_native_v2")
            .await
            .0,
        StatusCode::FORBIDDEN
    );
}
#[tokio::test]
async fn opt_in_phase_metadata_refuses_changed_nominal_terminal_origin() {
    let (f, report) = Box::pin(ready()).await;
    let auth = terminal::auth(&f).await;
    let receipt = results::record(&f.c.env.env.state, &auth, &report)
        .await
        .unwrap();
    let path = detail(&report, receipt.report_digest.as_str(), true);
    assert_eq!(get(&f, &path).await.0, StatusCode::OK);
    let pool = &f.c.env.env.pool;
    let original: Value = sqlx::query_scalar(
        "SELECT phase_terminal_qualification FROM profiled_stopped_receipts WHERE attempt_id=$1",
    )
    .bind(report.binding.attempt_id)
    .fetch_one(pool)
    .await
    .unwrap();
    // Deliberate privileged disposable corruption: never a supported writer or proof.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("ALTER TABLE profiled_stopped_receipts DISABLE TRIGGER profiled_stopped_immutable")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE profiled_stopped_receipts SET phase_terminal_qualification=jsonb_set(phase_terminal_qualification,'{id}','\"aq.foreign-terminal/test\"'::jsonb) WHERE attempt_id=$1").bind(report.binding.attempt_id).execute(&mut *tx).await.unwrap();
    sqlx::query("ALTER TABLE profiled_stopped_receipts ENABLE TRIGGER profiled_stopped_immutable")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(get(&f, &path).await.0, StatusCode::CONFLICT);
    assert_eq!(
        get(&f, "/api/human/results?profile=recorded_native_v2")
            .await
            .0,
        StatusCode::CONFLICT
    );
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("ALTER TABLE profiled_stopped_receipts DISABLE TRIGGER profiled_stopped_immutable")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE profiled_stopped_receipts SET phase_terminal_qualification=$2 WHERE attempt_id=$1",
    )
    .bind(report.binding.attempt_id)
    .bind(original)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("ALTER TABLE profiled_stopped_receipts ENABLE TRIGGER profiled_stopped_immutable")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(get(&f, &path).await.0, StatusCode::OK);
    // Even a valid enum/fk shape cannot relabel the nominal issued family.
    let foreign = if ORIGIN == "phase_execution_v2" {
        "phase_refresh_execution_v1"
    } else {
        "phase_execution_v2"
    };
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("ALTER TABLE profiled_result_reports DISABLE TRIGGER profiled_result_immutable")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE profiled_result_reports SET result_origin_kind=$2 WHERE report_id=$1")
        .bind(report.report_id)
        .bind(foreign)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE profiled_result_reports ENABLE TRIGGER profiled_result_immutable")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(get(&f, &path).await.0, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        get(&f, "/api/human/results?profile=recorded_native_v2")
            .await
            .0,
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("ALTER TABLE profiled_result_reports DISABLE TRIGGER profiled_result_immutable")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("UPDATE profiled_result_reports SET result_origin_kind=$2 WHERE report_id=$1")
        .bind(report.report_id)
        .bind(ORIGIN)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query("ALTER TABLE profiled_result_reports ENABLE TRIGGER profiled_result_immutable")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(get(&f, &path).await.0, StatusCode::OK);
}
