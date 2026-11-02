//! Actual SQL result recording and mounted human route, no browser/API fixtures.
use super::*;
use aq_server::profiled_execution::human_results::{self, Page};
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use tower::ServiceExt;
async fn get(f: &Fixture, path: &str) -> (StatusCode, serde_json::Value) {
    let response = human_results::routes()
        .with_state(f.state.clone())
        .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    if status == StatusCode::OK {
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
    let bytes = to_bytes(response.into_body(), 262144).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}
#[tokio::test]
async fn human_metadata_projects_exact_revisions_corrections_and_bounded_pages() {
    let (f, _, report) = prepare(true, true).await;
    let auth = context(&f).await;
    let receipt = results::record(&f.state, &auth, &report).await.unwrap();
    let mut correction = report.clone();
    correction.report_id = Uuid::new_v4();
    correction.operation_id = Uuid::new_v4();
    correction.supersedes = Some(report.report_id);
    correction.conclusion = Label::new("corrected retained claim").unwrap();
    results::record(&f.state, &auth, &correction).await.unwrap();
    let (status, page) = get(&f, "/api/human/results?limit=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page["coverage"], human_results::COVERAGE);
    assert_eq!(page["items"].as_array().unwrap().len(), 1);
    let cursor = page["next_after"].as_str().unwrap();
    let (_, next) = get(&f, &format!("/api/human/results?limit=1&after={cursor}")).await;
    assert!(next["next_after"].is_null());
    assert_ne!(page["items"][0]["report_id"], next["items"][0]["report_id"]);
    let path = format!(
        "/api/human/results/{}/revisions/{}",
        report.report_id,
        receipt.report_digest.as_str()
    );
    let (status, detail) = get(&f, &path).await;
    assert_eq!(status, StatusCode::OK);
    let old = &detail["items"][0];
    assert_eq!(old["current_report_head"], false);
    assert_eq!(old["head_report_id"], json!(correction.report_id));
    assert_eq!(old["conclusion"], report.conclusion.as_str());
    assert_eq!(old["project_id"], json!(f.selection.lineage.project_id));
    assert_eq!(old["task_id"], json!(f.selection.lineage.task_id));
    assert!(old.get("origin").is_none(), "default v1 field set changed");
    let (status, mut v2) = get(&f, &format!("{path}?profile=recorded_native_v2")).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v2["coverage"], human_results::NATIVE_V2_COVERAGE);
    assert_eq!(v2["items"][0]["origin"], "legacy_prepare_v1");
    v2["items"][0].as_object_mut().unwrap().remove("origin");
    v2["coverage"] = json!(human_results::COVERAGE);
    assert_eq!(
        serde_json::to_vec(&detail).unwrap(),
        serde_json::to_vec(&v2).unwrap(),
        "opt-in origin must not alter legacy metadata bytes/fields"
    );
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
        assert!(old.get(forbidden).is_none(), "unexpected {forbidden}");
    }
    assert_eq!(
        get(
            &f,
            &format!(
                "/api/human/results/{}/revisions/{}",
                report.report_id,
                "f".repeat(64)
            )
        )
        .await
        .0,
        StatusCode::NOT_FOUND
    );
    let (_, empty) = get(
        &f,
        &format!("/api/human/results?project_id={}", Uuid::new_v4()),
    )
    .await;
    assert!(empty["items"].as_array().unwrap().is_empty());
    assert!(
        human_results::read(
            &f.state,
            f.actor,
            &Page {
                project_id: Some(Uuid::new_v4()),
                ..Default::default()
            },
            Some((report.report_id, receipt.report_digest.as_str()))
        )
        .await
        .is_err()
    );
    assert_eq!(
        get(&f, "/api/human/results?limit=26").await.0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        get(&f, "/api/human/results?limit=0").await.0,
        StatusCode::BAD_REQUEST
    );
    if std::env::var("AQ_TEST_RETAIN_RESULTS_DEMO").as_deref() == Ok("1") {
        // Explicit isolated preview fixture, after assertions. No provider run.
        let name: String = sqlx::query_scalar("SELECT current_database()")
            .fetch_one(&f.db.pool().await)
            .await
            .unwrap();
        std::mem::forget(f.db.clone());
        eprintln!(
            "retained_results_demo_database={name} report_id={} revision={}",
            report.report_id,
            receipt.report_digest.as_str()
        );
    }
}
#[tokio::test]
async fn stopped_without_report_is_empty_and_current_viewer_cannot_read_results() {
    let (f, _, report) = prepare(true, true).await;
    let (_, page) = get(&f, "/api/human/results").await;
    assert!(page["items"].as_array().unwrap().is_empty());
    let auth = context(&f).await;
    results::record(&f.state, &auth, &report).await.unwrap();
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(f.actor)
        .execute(&f.db.pool().await)
        .await
        .unwrap();
    assert_eq!(get(&f, "/api/human/results").await.0, StatusCode::FORBIDDEN);
    assert_eq!(
        get(&f, "/api/human/results?profile=recorded_native_v2")
            .await
            .0,
        StatusCode::FORBIDDEN
    );
    assert!(
        human_results::read(&f.state, f.actor, &Page::default(), None)
            .await
            .is_err()
    );
}
