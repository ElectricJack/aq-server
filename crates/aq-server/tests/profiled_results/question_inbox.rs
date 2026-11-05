//! F5 human question inbox: the owner lists and inspects question decisions;
//! answering moves a question from open to closed with its delivery route.
use super::*;
use aq_server::agent_access::questions;
use aq_server::dashboard_api::contract::{QuestionInboxDetail, QuestionInboxPage};
use axum::http::StatusCode;
use serde_json::Value;
use tower::ServiceExt;

const OWNER: &str = "owner@localhost";

async fn get(app: &axum::Router, path: &str, email: &str) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            axum::http::Request::get(path)
                .header("x-aq-dev-email", email)
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    if status == StatusCode::OK {
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"text": String::from_utf8_lossy(&bytes)})),
    )
}
fn ids(page: &Value) -> Vec<String> {
    page["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["decision_id"].as_str().unwrap().to_owned())
        .collect()
}
fn fixture_response(name: &str, value: &Value) {
    if std::env::var_os("AQ_DASHBOARD_WRITE_FIXTURES").is_some() {
        std::fs::write(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../protocol/dashboard-read/v1/fixtures")
                .join(format!("{name}.json")),
            format!("{}\n", serde_json::to_string_pretty(value).unwrap()),
        )
        .unwrap();
    }
}
/// Typed decode and re-encode is lossless: the response is the published DTO.
fn typed_page(value: &Value) {
    let page: QuestionInboxPage = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(&serde_json::to_value(page).unwrap(), value);
}

#[tokio::test]
async fn question_inbox_pages_open_questions_and_closes_them_on_answer() {
    let env = Box::pin(review_env(1)).await;
    let f = &env.f;
    let pool = f.db.pool().await;
    let (node, attempt) = (f.node.node_id, env.report.binding.attempt_id);
    let asked = questions::ask(
        &f.state,
        node,
        attempt,
        Uuid::new_v4(),
        "Which database should the migration target?",
    )
    .await
    .unwrap();
    let first: Uuid = asked["decision_id"].as_str().unwrap().parse().unwrap();
    // One attempt holds one wait, so two later questions of the same task are
    // recorded directly: the read projects rows, not waits.
    let mut later = Vec::new();
    for (n, text) in ["Keep the legacy lease format?", "Which branch?"]
        .into_iter()
        .enumerate()
    {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO question_decisions(decision_id,attempt_id,operation_id,node_id,project_id,workspace_id,task_id,session,attempt_generation,question,question_digest,subject_revision,decision_revision,state,expires_at_ms,wait_id,created_at_ms) SELECT $1,attempt_id,$2,node_id,project_id,workspace_id,task_id,session,attempt_generation,$3,'sha256:test',subject_revision,1,'pending',expires_at_ms,$4,created_at_ms+$5 FROM question_decisions WHERE decision_id=$6")
            .bind(id)
            .bind(Uuid::new_v4())
            .bind(text)
            .bind(Uuid::new_v4())
            .bind(1000 * (n as i64 + 1))
            .bind(first)
            .execute(&pool)
            .await
            .unwrap();
        later.push(id);
    }
    let app = aq_server::build_admin_router(f.state.clone());
    let project = f.selection.lineage.project_id;

    // Bootstrap advertises both question capabilities to the owner.
    let (status, boot) = get(&app, "/api/v1/bootstrap", OWNER).await;
    assert_eq!(status, StatusCode::OK, "{boot}");
    for name in ["questions.read", "questions.answer", "dashboard.actions"] {
        let cap = boot["data"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .unwrap();
        assert_eq!(cap["enabled"], true, "{name}");
    }

    // Open pages oldest asked first; the cursor continues exactly once.
    let (status, page) = get(&app, "/api/questions/decisions?state=open&limit=2", OWNER).await;
    assert_eq!(status, StatusCode::OK, "{page}");
    typed_page(&page);
    assert_eq!(page["api_version"], "question-inbox-1");
    assert_eq!(ids(&page), [first.to_string(), later[0].to_string()]);
    let item = &page["items"][0];
    assert_eq!(item["kind"], "question");
    assert_eq!(item["state"], "pending");
    assert_eq!(item["decision_revision"], "1");
    assert_eq!(item["task_id"], json!(f.selection.lineage.task_id));
    assert_eq!(item["project_id"], json!(project));
    assert_eq!(
        item["question"],
        "Which database should the migration target?"
    );
    assert_eq!(item["asked_at_ms"], json!(NOW));
    assert!(item["expires_at_ms"].as_u64().unwrap() > NOW);
    for field in ["answer", "answered_at_ms", "delivery", "task_title"] {
        assert!(item[field].is_null(), "{field}");
    }
    let cursor = page["next_cursor"].as_str().unwrap().to_owned();
    assert!(!cursor.contains(&later[0].to_string()));
    let (status, next) = get(
        &app,
        &format!("/api/questions/decisions?state=open&limit=2&cursor={cursor}"),
        OWNER,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{next}");
    assert_eq!(ids(&next), [later[1].to_string()]);
    assert!(next["next_cursor"].is_null());
    // A cursor is bound to its state, page size and project.
    for changed in [
        "state=closed&limit=2",
        "state=open&limit=3",
        &format!("state=open&limit=2&project_id={project}"),
    ] {
        let (status, body) = get(
            &app,
            &format!("/api/questions/decisions?{changed}&cursor={cursor}"),
            OWNER,
        )
        .await;
        assert_eq!(status, StatusCode::GONE, "{changed}: {body}");
        assert_eq!(body["code"], "cursor_restart_required");
    }
    // Project filter and refused queries.
    let (_, scoped) = get(
        &app,
        &format!("/api/questions/decisions?project_id={project}"),
        OWNER,
    )
    .await;
    assert_eq!(ids(&scoped).len(), 3);
    let (_, foreign) = get(
        &app,
        &format!("/api/questions/decisions?project_id={}", Uuid::new_v4()),
        OWNER,
    )
    .await;
    assert!(ids(&foreign).is_empty() && foreign["next_cursor"].is_null());
    for bad in [
        "limit=0",
        "limit=51",
        "state=expired",
        "order=impact",
        "project_id=00000000-0000-0000-0000-000000000000",
        "cursor=not-a-cursor",
    ] {
        let (status, body) = get(&app, &format!("/api/questions/decisions?{bad}"), OWNER).await;
        assert!(
            status == StatusCode::UNPROCESSABLE_ENTITY || status == StatusCode::GONE,
            "{bad}: {status} {body}"
        );
    }
    let (_, closed) = get(&app, "/api/questions/decisions?state=closed", OWNER).await;
    assert!(ids(&closed).is_empty());

    // Detail of a pending question.
    let (status, detail) = get(&app, &format!("/api/questions/decisions/{first}"), OWNER).await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    let _: QuestionInboxDetail = serde_json::from_value(detail.clone()).unwrap();
    assert_eq!(detail["item"], page["items"][0]);
    let (status, _) = get(
        &app,
        &format!("/api/questions/decisions/{}", Uuid::new_v4()),
        OWNER,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, _) = get(&app, "/api/questions/decisions/not-a-uuid", OWNER).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    fixture_response("question-inbox-open", &page);

    // The live attempt's answer is delivered by its wait.
    questions::answer(&f.state, f.actor, first, 1, Uuid::new_v4(), "Postgres")
        .await
        .unwrap();
    f.time.store((NOW + 5_000) as i64, Ordering::SeqCst);
    // After the attempt ends, an answer is queued as task mail.
    let l = &f.selection.lineage;
    sqlx::query("INSERT INTO agent_launch_bindings(attempt_id,node_id,binding,profile,context,revoked) VALUES($1,$2,$3,'{}','{}',TRUE)")
        .bind(attempt)
        .bind(node)
        .bind(json!({"task_id":l.task_id,"project_id":l.project_id,"workspace_id":l.workspace_id,"attempt_id":attempt,"session_id":Uuid::new_v4(),"generation":1}))
        .execute(&pool)
        .await
        .unwrap();
    let mailed = questions::answer(&f.state, f.actor, later[1], 1, Uuid::new_v4(), "main")
        .await
        .unwrap();

    let (_, open) = get(&app, "/api/questions/decisions?state=open", OWNER).await;
    assert_eq!(ids(&open), [later[0].to_string()]);
    let (status, closed) = get(&app, "/api/questions/decisions?state=closed", OWNER).await;
    assert_eq!(status, StatusCode::OK, "{closed}");
    typed_page(&closed);
    // Most recently answered first.
    assert_eq!(ids(&closed), [later[1].to_string(), first.to_string()]);
    let mail = &closed["items"][0];
    assert_eq!(mail["state"], "answered");
    assert_eq!(mail["decision_revision"], "2");
    assert_eq!(mail["answer"], "main");
    assert_eq!(mail["answered_at_ms"], json!(NOW + 5_000));
    assert_eq!(mail["delivery"]["kind"], "task_mail");
    assert_eq!(mail["delivery"], mailed["delivery"]);
    let woke = &closed["items"][1];
    assert_eq!(woke["answer"], "Postgres");
    assert_eq!(woke["answered_at_ms"], json!(NOW));
    assert_eq!(woke["delivery"], json!({"kind":"wait"}));
    let (status, detail) = get(
        &app,
        &format!("/api/questions/decisions/{}", later[1]),
        OWNER,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{detail}");
    assert_eq!(detail["item"], closed["items"][0]);
    fixture_response("question-inbox-closed", &closed);
    fixture_response("question-detail", &detail);
}

#[tokio::test]
async fn question_inbox_refuses_non_owners() {
    let env = Box::pin(review_env(1)).await;
    let f = &env.f;
    let asked = questions::ask(
        &f.state,
        f.node.node_id,
        env.report.binding.attempt_id,
        Uuid::new_v4(),
        "May I proceed?",
    )
    .await
    .unwrap();
    let id = asked["decision_id"].as_str().unwrap().to_owned();
    let pool = f.db.pool().await;
    let viewer = "question-viewer@localhost";
    sqlx::query("INSERT INTO users(id,email,role,issuer,subject,created_at) VALUES($1,$2,'viewer','dev',$2,now())")
        .bind(Uuid::new_v4())
        .bind(viewer)
        .execute(&pool)
        .await
        .unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let list = "/api/questions/decisions?state=open";
    let detail = format!("/api/questions/decisions/{id}");
    for path in [list, detail.as_str()] {
        assert_eq!(get(&app, path, OWNER).await.0, StatusCode::OK, "{path}");
        assert_eq!(
            get(&app, path, viewer).await.0,
            StatusCode::FORBIDDEN,
            "{path}"
        );
    }
    let (_, boot) = get(&app, "/api/v1/bootstrap", viewer).await;
    for name in ["questions.read", "questions.answer"] {
        let cap = boot["data"]["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .unwrap();
        assert_eq!(cap["enabled"], false, "{name}");
    }
    // Demoting the owner refuses both reads on the next request.
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(f.actor)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(get(&app, list, OWNER).await.0, StatusCode::FORBIDDEN);
    assert_eq!(get(&app, &detail, OWNER).await.0, StatusCode::FORBIDDEN);
}
