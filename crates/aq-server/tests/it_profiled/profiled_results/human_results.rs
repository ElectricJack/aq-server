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

#[tokio::test]
async fn dashboard_search_uses_retained_result_metadata_and_digest() {
    let (f, _, mut report) = prepare(true, true).await;
    report.conclusion = Label::new("example result").unwrap();
    let auth = context(&f).await;
    let receipt = results::record(&f.state, &auth, &report).await.unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let response = app
        .oneshot(
            Request::get("/api/v1/search?q=example&types=result")
                .header("x-aq-dev-email", "owner@localhost")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 262144).await.unwrap()).unwrap();
    let group = body["data"]["groups"]
        .as_array()
        .unwrap()
        .iter()
        .find(|g| g["type"] == "result")
        .unwrap();
    assert_eq!(group["hits"].as_array().unwrap().len(), 1, "{body}");
    assert_eq!(group["hits"][0]["title"], "example result");
    assert_eq!(group["hits"][0]["ref"]["id"], json!(report.report_id));
    assert_eq!(
        group["hits"][0]["ref"]["revision"],
        format!("sha256:{}", receipt.report_digest.as_str())
    );
    assert_eq!(
        group["hits"][0]["project"]["id"],
        json!(f.selection.lineage.project_id)
    );
    for excluded in [
        "execution_spec",
        "launch_manifest",
        "credentials",
        "source_receipt_ids",
    ] {
        assert!(!body.to_string().contains(excluded));
    }
}
async fn corrupt_report_encoding(pool: &sqlx::PgPool, report_id: Uuid) {
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("ALTER TABLE profiled_result_reports DISABLE TRIGGER profiled_result_immutable")
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "UPDATE profiled_result_reports SET report_bytes=report_bytes||'\\x0a'::bytea, \
         report_digest=encode(sha256(report_bytes||'\\x0a'::bytea),'hex') WHERE report_id=$1",
    )
    .bind(report_id)
    .execute(&mut *tx)
    .await
    .unwrap();
    sqlx::query("ALTER TABLE profiled_result_reports ENABLE TRIGGER profiled_result_immutable")
        .execute(&mut *tx)
        .await
        .unwrap();
    tx.commit().await.unwrap();
}

#[tokio::test]
async fn project_listing_isolates_an_unreadable_report_and_keeps_projects_apart() {
    let (f, _, report) = prepare(true, true).await;
    let auth = context(&f).await;
    let receipt = results::record(&f.state, &auth, &report).await.unwrap();
    let mut correction = report.clone();
    correction.report_id = Uuid::new_v4();
    correction.operation_id = Uuid::new_v4();
    correction.supersedes = Some(report.report_id);
    results::record(&f.state, &auth, &correction).await.unwrap();
    let project = f.selection.lineage.project_id;
    let page = human_results::read_project(&f.state, project, None, None, None)
        .await
        .unwrap();
    assert_eq!(page.items.len(), 2);
    assert!(!page.incomplete && page.unavailable.is_empty());
    // Another project sees nothing, and cannot probe an exact revision here.
    let other = Uuid::new_v4();
    let page = human_results::read_project(&f.state, other, None, None, None)
        .await
        .unwrap();
    assert!(page.items.is_empty() && !page.incomplete);
    assert!(
        human_results::read_project(
            &f.state,
            other,
            None,
            None,
            Some((report.report_id, receipt.report_digest.as_str()))
        )
        .await
        .is_err()
    );
    // The original's stored bytes stop being its canonical encoding, with the
    // row's digest kept equal to the new bytes (so the row stays valid for
    // `profiled_result_reports_check`): its validation refuses. The correction
    // still lists, the page says it is incomplete.
    let pool = f.db.pool().await;
    corrupt_report_encoding(&pool, report.report_id).await;
    let page = human_results::read_project(&f.state, project, None, None, None)
        .await
        .unwrap();
    assert!(page.incomplete);
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].report_id, correction.report_id);
    assert_eq!(page.unavailable.len(), 1);
    assert_eq!(page.unavailable[0].report_id, report.report_id);
    let (status, human) = get(&f, "/api/human/results").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(human["incomplete"], true);
    assert_eq!(human["items"].as_array().unwrap().len(), 1);
    // An exact read of the unreadable report still fails as a whole.
    assert!(
        human_results::read_project(
            &f.state,
            project,
            None,
            None,
            Some((report.report_id, receipt.report_digest.as_str()))
        )
        .await
        .is_err()
    );
}

/// One GET through the supervisor read nest as `project`'s supervisor (the
/// global supervisor when `None`), approved by the fixture's owner.
async fn supervised(
    f: &Fixture,
    project: Option<Uuid>,
    path: &str,
) -> (StatusCode, serde_json::Value) {
    use aq_supervisor_control::read::{self, Permit, ReadOwner};
    use sqlx::Row;
    let pool = f.db.pool().await;
    let user = sqlx::query("SELECT email,issuer,subject FROM users WHERE id=$1")
        .bind(f.actor)
        .fetch_one(&pool)
        .await
        .unwrap();
    let owner = ReadOwner {
        id: f.actor,
        email: user.get("email"),
        issuer: user.get("issuer"),
        subject: user.get("subject"),
    };
    let auth: read::Auth = std::sync::Arc::new(move |_| {
        let (pool, owner) = (pool.clone(), owner.clone());
        Box::pin(async move {
            let guard = pool
                .begin()
                .await
                .map_err(aq_manual_sessions::Error::from)?;
            Ok(Permit {
                owner,
                project,
                guard,
            })
        })
    });
    let response = read::nest(human_results::routes(), auth)
        .with_state(f.state.clone())
        .oneshot(
            Request::builder()
                .uri(format!("/v1/supervisor/read{path}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 262144).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn a_supervisor_reads_only_the_results_its_scope_covers() {
    let (f, _, report) = prepare(true, true).await;
    let auth = context(&f).await;
    let receipt = results::record(&f.state, &auth, &report).await.unwrap();
    let (own, other) = (f.selection.lineage.project_id, Uuid::new_v4());
    let one = format!(
        "/api/human/results/{}/revisions/{}",
        report.report_id,
        receipt.report_digest.as_str()
    );
    let refused = |(status, body): (StatusCode, serde_json::Value)| {
        assert_eq!(body["error"], "supervisor_refused", "{body}");
        status
    };
    // The project's supervisor reads its own result; the global one any.
    for scope in [Some(own), None] {
        let (status, detail) = supervised(&f, scope, &one).await;
        assert_eq!(status, StatusCode::OK, "{detail}");
        assert_eq!(detail["items"][0]["report_id"], json!(report.report_id));
        let (status, page) = supervised(&f, scope, "/api/human/results").await;
        assert_eq!(status, StatusCode::OK, "{page}");
        assert_eq!(page["items"].as_array().unwrap().len(), 1, "{page}");
    }
    // Another project's supervisor is refused the result and sees none listed.
    for path in [one.clone(), one.replacen("/api/human/", "/api/v1/", 1)] {
        let status = refused(supervised(&f, Some(other), &path).await);
        assert_eq!(status, StatusCode::FORBIDDEN, "{path}");
    }
    let (status, page) = supervised(&f, Some(other), "/api/human/results").await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert!(page["items"].as_array().unwrap().is_empty(), "{page}");
    // It cannot widen the page to the project it does not supervise.
    let widened = format!("/api/human/results?project_id={own}");
    let status = refused(supervised(&f, Some(other), &widened).await);
    assert_eq!(status, StatusCode::FORBIDDEN);
    // A path no supervisor route serves is a supervisor refusal, never a
    // manual-session one.
    for scope in [Some(own), None] {
        let status = refused(supervised(&f, scope, "/api/human/nope").await);
        assert_eq!(status, StatusCode::NOT_FOUND);
    }
}
