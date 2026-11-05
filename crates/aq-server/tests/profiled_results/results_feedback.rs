use super::*;
use axum::{
    Router,
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use serde_json::Value;
use tower::ServiceExt;

async fn send(app: &Router, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .header("x-aq-dev-email", "owner@localhost")
                .header("x-aq-request", "1")
                .header("content-type", "application/json")
                .body(Body::from(if body.is_null() {
                    String::new()
                } else {
                    body.to_string()
                }))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1_048_576).await.unwrap();
    (
        status,
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({"text":String::from_utf8_lossy(&bytes)})),
    )
}
#[tokio::test]
async fn media_upload_disabled_without_store_is_explicit() {
    let f = fixture().await;
    let app = aq_server::build_admin_router(f.state.clone());
    let path = format!(
        "/api/human/results/{}/revisions/{}/media-uploads",
        Uuid::new_v4(),
        "a".repeat(64)
    );
    let (status, body) = send(&app, "POST", &path, json!({})).await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{body}");
    assert_eq!(body["code"], "media_store_unconfigured");
    let (_, bootstrap) = send(&app, "GET", "/api/v1/bootstrap", Value::Null).await;
    let cap = bootstrap["data"]["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "results.media.upload")
        .unwrap();
    assert_eq!(cap["enabled"], false);
    assert_eq!(cap["reason"], "Result media store is not configured");
}
#[tokio::test]
async fn feedback_comments_replay_history_resolution_and_feeds() {
    let (f, _, report) = prepare(true, true).await;
    let receipt = results::record(&f.state, &context(&f).await, &report)
        .await
        .unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let root = format!(
        "/api/v1/results/{}/revisions/{}",
        report.report_id,
        receipt.report_digest.as_str()
    );
    let (code, detail) = send(&app, "GET", &root, Value::Null).await;
    assert_eq!(code, StatusCode::OK, "{detail}");
    assert_eq!(detail["data"]["assessment"]["acceptance"], "unreviewed");
    let id = Uuid::new_v4();
    let body = json!({"operation_id":id,"expected_version":"1","body":"Fix the caption","anchor":{"kind":"result"},"follow":true});
    let path = format!("{root}/comments");
    let (code, first) = send(&app, "POST", &path, body.clone()).await;
    assert_eq!(code, StatusCode::OK, "{first}");
    assert_eq!(first["data"]["disposition"], "confirmed");
    assert_eq!(
        send(&app, "POST", &path, body.clone()).await.1["data"],
        first["data"]
    );
    let mut changed = body.clone();
    changed["body"] = json!("changed");
    assert_eq!(
        send(&app, "POST", &path, changed).await.0,
        StatusCode::CONFLICT
    );
    let (_, comments) = send(&app, "GET", &path, Value::Null).await;
    let comment = &comments["data"]["items"][0];
    assert_eq!(comment["body"], "Fix the caption");
    let thread = comment["thread_id"].as_str().unwrap();
    let edit = format!("/api/v1/comments/{id}/revisions");
    let corrected = json!({"operation_id":Uuid::new_v4(),"expected_version":"1","body":"Fix caption and spacing"});
    assert_eq!(
        send(&app, "POST", &edit, corrected).await.1["data"]["disposition"],
        "confirmed"
    );
    let (_, history) = send(&app, "GET", &path, Value::Null).await;
    assert_eq!(
        history["data"]["items"][0]["history"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let (_, following) = send(&app, "GET", "/api/v1/results?mode=following", Value::Null).await;
    assert_eq!(following["data"]["items"].as_array().unwrap().len(), 1);
    // Thread opt-in includes the result in Following without changing its toggle.
    assert_eq!(
        following["data"]["items"][0]["result"]["directly_following"],
        false
    );
    let (_, needs) = send(
        &app,
        "GET",
        "/api/v1/results?mode=needs_review",
        Value::Null,
    )
    .await;
    assert_eq!(needs["data"]["items"].as_array().unwrap().len(), 1);
    let resolve = json!({"operation_id":Uuid::new_v4(),"expected_version":"1","follow_up":{"result_id":report.report_id,"revision":receipt.report_digest.as_str()},"reason":"Caption checked"});
    let resolved = send(
        &app,
        "POST",
        &format!("/api/v1/threads/{thread}/resolve"),
        resolve,
    )
    .await;
    assert_eq!(
        resolved.1["data"]["disposition"], "confirmed",
        "{resolved:?}"
    );
    let (_, needs) = send(
        &app,
        "GET",
        "/api/v1/results?mode=needs_review",
        Value::Null,
    )
    .await;
    assert!(needs["data"]["items"].as_array().unwrap().is_empty());
    let (_, detail) = send(&app, "GET", &root, Value::Null).await;
    assert_eq!(detail["data"]["assessment"]["acceptance"], "unreviewed");
    assert_eq!(
        detail["data"]["revision"]["addressed_feedback"],
        json!([id])
    );
    assert_eq!(
        detail["data"]["feedback_read_version"],
        "results-feedback-1.1"
    );
    assert_eq!(
        detail["data"]["follow_up_submission"],
        json!({"state":"unsupported"})
    );
    let (_, comments) = send(&app, "GET", &path, Value::Null).await;
    assert_eq!(
        comments["data"]["feedback_read_version"],
        "results-feedback-1.1"
    );
    assert_eq!(
        comments["data"]["follow_up_submission"],
        json!({"state":"unsupported"})
    );
    assert_eq!(
        comments["data"]["items"][0]["resolution"]["follow_up"]["revision"],
        receipt.report_digest.as_str()
    );
    assert_eq!(
        comments["data"]["items"][0]["resolution"]["follow_up_report_id"],
        report.report_id.to_string()
    );
    assert_eq!(
        comments["data"]["items"][0]["resolution"]["reason"],
        "Caption checked"
    );
}

#[tokio::test]
#[ignore = "requires explicit pinned AQ_TEST_GRAPH_BINARY and AQ_TEST_GRAPH_SHA256"]
async fn feedback_task_links_result_revision() {
    use sqlx::ConnectOptions;
    use std::os::unix::fs::PermissionsExt;
    let (mut f, _, report) = prepare(true, true).await;
    let receipt = results::record(&f.state, &context(&f).await, &report)
        .await
        .unwrap();
    let graph_db = aq_server::test_support::TestDb::create().await;
    let binary =
        std::env::var("AQ_TEST_GRAPH_BINARY").expect("pinned Graph binary for G6 acceptance");
    use sha2::Digest as _;
    assert_eq!(
        hex::encode(sha2::Sha256::digest(std::fs::read(&binary).unwrap())),
        std::env::var("AQ_TEST_GRAPH_SHA256").unwrap()
    );
    let dir = tempfile::tempdir().unwrap();
    let token = dir.path().join("token");
    std::fs::write(&token, "g6-disposable-graph-fixture-private-token").unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = socket.local_addr().unwrap();
    drop(socket);
    let mut child = tokio::process::Command::new(binary)
        .args([
            "serve",
            "--bind",
            &addr.to_string(),
            "--bootstrap-workspace",
            &f.selection.lineage.workspace_id.to_string(),
            "--bootstrap-token-file",
            token.to_str().unwrap(),
            "--pool-size",
            "2",
        ])
        .env_clear()
        .env("TOKIO_WORKER_THREADS", "2")
        .env("DATABASE_URL", graph_db.options().to_url_lossy().as_str())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let client = reqwest::Client::new();
    let origin = format!("http://{addr}");
    let mut ready = false;
    for _ in 0..200 {
        if client
            .get(format!("{origin}/health/ready"))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
        {
            ready = true;
            break;
        }
        if let Some(status) = child.try_wait().unwrap() {
            use tokio::io::AsyncReadExt;
            let mut diagnostics = String::new();
            child
                .stderr
                .take()
                .unwrap()
                .read_to_string(&mut diagnostics)
                .await
                .unwrap();
            panic!("Graph exited {status}: {diagnostics}");
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(ready);
    f.state.agent_access = Some(Arc::new(
        aq_server::agent_access::Runtime::new(&aq_server::agent_access::AgentAccessConfig {
            graph_url: origin,
            graph_token_file: token,
            mutation_actor_id: Some("local-admin".into()),
            credential_ttl_ms: 120000,
        })
        .await
        .unwrap(),
    ));
    let app = aq_server::build_admin_router(f.state.clone());
    let root = format!(
        "/api/v1/results/{}/revisions/{}",
        report.report_id,
        receipt.report_digest.as_str()
    );
    let comment = Uuid::new_v4();
    let (_,created)=send(&app,"POST",&format!("{root}/comments"),json!({"operation_id":comment,"expected_version":"1","body":"Fix caption","anchor":{"kind":"result"},"follow":true})).await;
    assert_eq!(created["data"]["disposition"], "confirmed", "{created}");
    let id = Uuid::new_v4();
    let draft = json!({"operation_id":id,"expected_version":"1","result_id":report.report_id,"revision":receipt.report_digest.as_str(),"thread_id":comment,"comment_id":comment,"comment_revision":"1","title":"Correct caption","description":"Use the reviewed caption","acceptance_criteria":"Caption matches feedback","project_id":f.selection.lineage.project_id,"expected_project_revision":"1","parent_id":null,"artifacts":[]});
    let pool = f.db.pool().await;
    sqlx::query("CREATE FUNCTION g6_test_link_loss() RETURNS trigger LANGUAGE plpgsql AS 'BEGIN RAISE EXCEPTION ''injected lost link''; END'").execute(&pool).await.unwrap();
    sqlx::query("CREATE TRIGGER g6_test_loss BEFORE INSERT ON result_feedback_links FOR EACH ROW EXECUTE FUNCTION g6_test_link_loss()").execute(&pool).await.unwrap();
    let (code, uncertain) = send(&app, "POST", "/api/v1/feedback-tasks", draft.clone()).await;
    assert_eq!(code, StatusCode::ACCEPTED, "{uncertain}");
    assert_eq!(uncertain["data"]["disposition"], "unknown");
    let (_, status) = send(
        &app,
        "GET",
        &format!("/api/v1/feedback-tasks/{id}"),
        Value::Null,
    )
    .await;
    assert_eq!(status["data"]["task_id"], id.to_string());
    assert_eq!(status["data"]["link_confirmed"], false);
    assert_eq!(status["data"]["creation_disposition"], "confirmed");
    sqlx::query("DROP TRIGGER g6_test_loss ON result_feedback_links")
        .execute(&pool)
        .await
        .unwrap();
    let (_, recovered) = send(
        &app,
        "GET",
        &format!("/api/v1/operations/{id}"),
        Value::Null,
    )
    .await;
    assert_eq!(recovered["data"]["disposition"], "confirmed", "{recovered}");
    let (code, linked) = send(&app, "POST", "/api/v1/feedback-tasks", draft.clone()).await;
    assert_eq!(code, StatusCode::OK, "{linked}");
    assert_eq!(linked["data"]["disposition"], "confirmed", "{linked}");
    assert!(
        linked["data"]["affected_entities"]
            .as_array()
            .unwrap()
            .iter()
            .any(|e| e["type"] == "task" && e["id"] == id.to_string())
    );
    assert_eq!(
        send(&app, "POST", "/api/v1/feedback-tasks", draft.clone())
            .await
            .1["data"],
        linked["data"]
    );
    let pool = f.db.pool().await;
    let pin: (Uuid, String, Uuid) = sqlx::query_as(
        "SELECT report_id,report_revision,task_id FROM result_feedback_links WHERE operation_id=$1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        pin,
        (report.report_id, receipt.report_digest.as_str().into(), id)
    );
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM human_graph_operations WHERE operation_id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
    let (_, comments) = send(&app, "GET", &format!("{root}/comments"), Value::Null).await;
    assert_eq!(comments["data"]["items"][0]["thread_state"], "open");
    let links = comments["data"]["items"][0]["feedback_links"]
        .as_array()
        .unwrap();
    assert_eq!(links.len(), 1);
    assert_eq!(links[0]["operation_id"], id.to_string());
    assert_eq!(links[0]["task_id"], id.to_string());
    assert_eq!(links[0]["origin"]["comment_id"], comment.to_string());
    assert_eq!(links[0]["origin"]["comment_revision"], "1");
    assert_eq!(
        links[0]["origin"]["revision"],
        receipt.report_digest.as_str()
    );
    assert!(comments["data"]["items"][0]["resolution"].is_null());
    let child_id = Uuid::new_v4();
    let mut child_draft = draft.clone();
    child_draft["operation_id"] = json!(child_id);
    child_draft["parent_id"] = json!(id);
    let (code, created_child) = send(&app, "POST", "/api/v1/feedback-tasks", child_draft).await;
    assert_eq!(code, StatusCode::OK, "{created_child}");
    assert_eq!(
        created_child["data"]["disposition"], "confirmed",
        "{created_child}"
    );
    let (code, graph) = send(
        &app,
        "GET",
        &format!(
            "/api/human/projects/{}/tasks/{child_id}/graph",
            f.selection.lineage.project_id
        ),
        Value::Null,
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{graph}");
    assert!(graph.to_string().contains("parent-child"), "{graph}");
    let orphan_id = Uuid::new_v4();
    let mut original = draft.clone();
    original["operation_id"] = json!(orphan_id);
    sqlx::query("CREATE TRIGGER g6_native_loss BEFORE INSERT ON human_graph_operations FOR EACH ROW EXECUTE FUNCTION g6_test_link_loss()").execute(&pool).await.unwrap();
    let (code, lost) = send(&app, "POST", "/api/v1/feedback-tasks", original.clone()).await;
    assert_eq!(code, StatusCode::ACCEPTED, "{lost}");
    sqlx::query("DROP TRIGGER g6_native_loss ON human_graph_operations")
        .execute(&pool)
        .await
        .unwrap();
    let (code, recovery) = send(
        &app,
        "POST",
        &format!("/api/v1/feedback-tasks/{orphan_id}/recover"),
        json!({}),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{recovery}");
    assert_eq!(recovery["data"]["disposition"], "confirmed", "{recovery}");
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM human_graph_operations WHERE operation_id=$1")
            .bind(orphan_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(count, 1);
    let link_id = Uuid::new_v4();
    let existing_link = json!({"operation_id":link_id,"expected_version":"1","result_id":report.report_id,"revision":receipt.report_digest.as_str(),"thread_id":comment,"comment_id":comment,"comment_revision":"1","task_id":id,"project_id":f.selection.lineage.project_id,"artifacts":[]});
    let (code, attached) = send(
        &app,
        "POST",
        "/api/v1/feedback-links",
        existing_link.clone(),
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{attached}");
    assert_eq!(attached["data"]["disposition"], "confirmed", "{attached}");
    assert_eq!(
        send(
            &app,
            "POST",
            "/api/v1/feedback-links",
            existing_link.clone()
        )
        .await
        .1["data"],
        attached["data"]
    );
    let mut foreign = existing_link.clone();
    foreign["operation_id"] = json!(Uuid::new_v4());
    foreign["project_id"] = json!(Uuid::new_v4());
    assert_eq!(
        send(&app, "POST", "/api/v1/feedback-links", foreign)
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let pin: (Uuid, String, Uuid) = sqlx::query_as(
        "SELECT report_id,report_revision,task_id FROM result_feedback_links WHERE operation_id=$1",
    )
    .bind(link_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        pin,
        (report.report_id, receipt.report_digest.as_str().into(), id)
    );
    let (_, comments) = send(&app, "GET", &format!("{root}/comments"), Value::Null).await;
    assert_eq!(comments["data"]["items"][0]["thread_state"], "open");
    let links = comments["data"]["items"][0]["feedback_links"]
        .as_array()
        .unwrap();
    // Creation, recovered creation and existing-task linking retain separate IDs.
    assert_eq!(links.len(), 4);
    let existing = links
        .iter()
        .find(|link| link["operation_id"] == link_id.to_string())
        .unwrap();
    assert_eq!(existing["task_id"], id.to_string());
    assert_eq!(existing["origin"]["comment_revision"], "1");
    assert_eq!(existing["origin"]["artifacts"], json!([]));
    assert!(comments["data"]["items"][0]["resolution"].is_null());
    assert_eq!(
        comments["data"]["follow_up_submission"],
        json!({"state":"unsupported"})
    );
    let (_, detail) = send(&app, "GET", &root, Value::Null).await;
    assert_eq!(detail["data"]["revision"]["addressed_feedback"], json!([]));
    let (_, corrected) = send(
        &app, "POST", &format!("/api/v1/comments/{comment}/revisions"),
        json!({"operation_id":Uuid::new_v4(),"expected_version":"1","body":"Updated wording after task creation"}),
    ).await;
    assert_eq!(corrected["data"]["disposition"], "confirmed");
    let (_, detail) = send(&app, "GET", &root, Value::Null).await;
    let reply = Uuid::new_v4();
    let (_, replied) = send(
        &app,
        "POST",
        &format!("{root}/comments"),
        json!({
            "operation_id":reply,"expected_version":detail["data"]["result"]["version"],"thread_id":comment,
            "body":"Another comment in this thread","anchor":{"kind":"result"}
        }),
    )
    .await;
    assert_eq!(replied["data"]["disposition"], "confirmed");
    let reloaded = aq_server::build_admin_router(f.state.clone());
    let (_, comments) = send(&reloaded, "GET", &format!("{root}/comments"), Value::Null).await;
    assert_eq!(comments["data"]["items"][0]["revision"], "2");
    for link in comments["data"]["items"][0]["feedback_links"]
        .as_array()
        .unwrap()
    {
        assert_eq!(link["origin"]["comment_revision"], "1");
    }
    assert_eq!(comments["data"]["items"][1]["feedback_links"], json!([]));
    serde_json::from_value::<aq_server::results_feedback::contract::CommentsPage>(
        comments["data"].clone(),
    )
    .unwrap();
    let mut correction = report.clone();
    correction.report_id = Uuid::new_v4();
    correction.operation_id = Uuid::new_v4();
    correction.supersedes = Some(report.report_id);
    let next = results::record(&f.state, &context(&f).await, &correction)
        .await
        .unwrap();
    let (_, fresh) = send(
        &reloaded,
        "GET",
        &format!(
            "/api/v1/results/{}/revisions/{}/comments",
            report.report_id,
            next.report_digest.as_str()
        ),
        Value::Null,
    )
    .await;
    assert_eq!(fresh["data"]["items"], json!([]));
    child.kill().await.unwrap();
}

#[tokio::test]
async fn capture_policy_is_pinned_and_waiver_is_exact() {
    let (f, _, report) = prepare(true, true).await;
    let pool = f.db.pool().await;
    let requirement = json!({"id":"desktop","policy_revision":"1","kind":"image","scenario":"results","steps":["Open Results"],"viewport":{"width_css":1280,"height_css":800,"dpr_milli":1000},"environment":"test","count":1,"required":true,"applicability":"Results page changed","gate_binding":"completion"});
    sqlx::query("INSERT INTO result_task_policies(project_id,task_id,revision,requirements) VALUES($1,$2,1,$3)").bind(f.selection.lineage.project_id).bind(f.selection.lineage.task_id).bind(json!([requirement])).execute(&pool).await.unwrap();
    let receipt = results::record(&f.state, &context(&f).await, &report)
        .await
        .unwrap();
    sqlx::query("INSERT INTO result_task_policies(project_id,task_id,revision,requirements) VALUES($1,$2,2,'[]')").bind(f.selection.lineage.project_id).bind(f.selection.lineage.task_id).execute(&pool).await.unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let root = format!(
        "/api/v1/results/{}/revisions/{}",
        report.report_id,
        receipt.report_digest.as_str()
    );
    let (_, detail) = send(&app, "GET", &root, Value::Null).await;
    assert_eq!(
        detail["data"]["revision"]["capture_policy"]["revision"],
        "1"
    );
    assert_eq!(detail["data"]["assessment"]["evidence"]["satisfied"], false);
    let path = format!("/api/v1/results/{}/evidence-waivers", report.report_id);
    let bad = json!({"operation_id":Uuid::new_v4(),"expected_version":"1","revision":receipt.report_digest.as_str(),"requirement_id":"desktop","policy_revision":"2","reason":"Owner waiver"});
    assert_eq!(
        send(&app, "POST", &path, bad).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let good = json!({"operation_id":Uuid::new_v4(),"expected_version":"1","revision":receipt.report_digest.as_str(),"requirement_id":"desktop","policy_revision":"1","reason":"Owner waiver"});
    assert_eq!(
        send(&app, "POST", &path, good).await.1["data"]["disposition"],
        "confirmed"
    );
    let (_, detail) = send(&app, "GET", &root, Value::Null).await;
    assert_eq!(detail["data"]["assessment"]["evidence"]["satisfied"], true);
    assert_eq!(detail["data"]["assessment"]["acceptance"], "unreviewed");
    let duplicate = json!({"operation_id":Uuid::new_v4(),"expected_version":detail["data"]["assessment"]["version"],"revision":receipt.report_digest.as_str(),"requirement_id":"desktop","policy_revision":"1","reason":"Second waiver cannot replace immutable decision"});
    let (code, refused) = send(&app, "POST", &path, duplicate).await;
    assert_eq!(code, StatusCode::OK, "{refused}");
    assert_eq!(refused["data"]["disposition"], "conflict", "{refused}");
    let retained: i64 =
        sqlx::query_scalar("SELECT count(*) FROM result_evidence_waivers WHERE report_id=$1")
            .bind(report.report_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(retained, 1);
}

#[tokio::test]
async fn feedback_bounds_anchors_replies_cursors_and_exact_corrections() {
    let (f, _, report) = prepare(true, true).await;
    let receipt = results::record(&f.state, &context(&f).await, &report)
        .await
        .unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let root = format!(
        "/api/v1/results/{}/revisions/{}",
        report.report_id,
        receipt.report_digest.as_str()
    );
    let path = format!("{root}/comments");
    for body in ["".to_string(), "é".repeat(8193)] {
        let (code,v)=send(&app,"POST",&path,json!({"operation_id":Uuid::new_v4(),"expected_version":"1","body":body,"anchor":{"kind":"result"}})).await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    }
    for anchor in [
        json!({"kind":"image_region","artifact_id":Uuid::new_v4(),"digest":format!("sha256:{}","a".repeat(64)),"x":0.9,"y":0.0,"width":0.2,"height":0.1}),
        json!({"kind":"video_range","artifact_id":Uuid::new_v4(),"digest":"invalid","start_ms":1,"end_ms":2}),
    ] {
        assert_eq!(send(&app,"POST",&path,json!({"operation_id":Uuid::new_v4(),"expected_version":"1","body":"bad anchor","anchor":anchor})).await.0,StatusCode::UNPROCESSABLE_ENTITY);
    }
    let first = Uuid::new_v4();
    let body = json!({"operation_id":first,"expected_version":"1","body":"first","anchor":{"kind":"result"},"follow":true});
    assert_eq!(
        send(&app, "POST", &path, body.clone()).await.1["data"]["disposition"],
        "confirmed"
    );
    let reply = Uuid::new_v4();
    let response=send(&app,"POST",&path,json!({"operation_id":reply,"expected_version":"2","thread_id":first,"body":"reply","anchor":{"kind":"result"}})).await.1;
    assert_eq!(response["data"]["disposition"], "confirmed", "{response}");
    let (_, page) = send(&app, "GET", &format!("{path}?limit=1"), Value::Null).await;
    assert_eq!(page["data"]["items"][0]["id"], first.to_string());
    let cursor = page["data"]["next_cursor"].as_str().unwrap();
    assert_eq!(
        send(
            &app,
            "GET",
            &format!("{path}?limit=2&cursor={cursor}"),
            Value::Null
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (_, page2) = send(
        &app,
        "GET",
        &format!("{path}?limit=1&cursor={cursor}"),
        Value::Null,
    )
    .await;
    assert_eq!(page2["data"]["items"][0]["id"], reply.to_string());
    assert!(page2["data"]["next_cursor"].is_null());
    let mut correction = report.clone();
    correction.report_id = Uuid::new_v4();
    correction.operation_id = Uuid::new_v4();
    correction.supersedes = Some(report.report_id);
    let revised = results::record(&f.state, &context(&f).await, &correction)
        .await
        .unwrap();
    let (_, old) = send(&app, "GET", &root, Value::Null).await;
    assert_eq!(
        old["data"]["revision"]["revision"],
        receipt.report_digest.as_str()
    );
    assert_eq!(
        old["data"]["result"]["current_revision"],
        revised.report_digest.as_str()
    );
    let (_, new) = send(
        &app,
        "GET",
        &format!(
            "/api/v1/results/{}/revisions/{}",
            report.report_id,
            revised.report_digest.as_str()
        ),
        Value::Null,
    )
    .await;
    assert_eq!(new["data"]["result"]["id"], report.report_id.to_string());
    assert_eq!(send(&app,"POST",&format!("/api/v1/comments/{first}/revisions"),json!({"operation_id":Uuid::new_v4(),"expected_version":"1","body":"old revision correction"})).await.1["data"]["disposition"],"confirmed");
    let (_, old_comments) = send(&app, "GET", &path, Value::Null).await;
    assert_eq!(
        old_comments["data"]["items"][0]["body"],
        "old revision correction"
    );
    let (_, feed) = send(&app, "GET", "/api/v1/results?mode=latest", Value::Null).await;
    assert_eq!(feed["data"]["items"].as_array().unwrap().len(), 1);
    assert_eq!(
        feed["data"]["items"][0]["revision"]["revision"],
        revised.report_digest.as_str()
    );
}

#[tokio::test]
async fn feedback_retained_image_anchor_and_principal_fence() {
    let (f, report, revision, owner, _root, actor) = super::media_owner::setup().await;
    let id = super::media_owner::retained(&owner, &actor, report.report_id, &revision).await;
    let app = aq_server::build_admin_router(f.state.clone());
    let path = format!(
        "/api/v1/results/{}/revisions/{revision}/comments",
        report.report_id
    );
    let digest = aq_result_media::Digest::of(&super::media_owner::png())
        .as_str()
        .to_string();
    let mut body = json!({"operation_id":Uuid::new_v4(),"expected_version":"1","body":"image feedback","anchor":{"kind":"image_region","artifact_id":id,"digest":digest,"x":0.1,"y":0.1,"width":0.2,"height":0.3}});
    let (code, created) = send(&app, "POST", &path, body.clone()).await;
    assert_eq!(code, StatusCode::OK, "{created}");
    assert_eq!(created["data"]["disposition"], "confirmed");
    body["operation_id"] = json!(Uuid::new_v4());
    body["expected_version"] = json!("2");
    body["anchor"]["digest"] = json!(format!("sha256:{}", "0".repeat(64)));
    assert_eq!(
        send(&app, "POST", &path, body).await.0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
    let pool = f.db.pool().await;
    sqlx::query("UPDATE users SET role='viewer' WHERE id=$1")
        .bind(actor.id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        send(&app, "GET", &path, Value::Null).await.0,
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(&app, "GET", "/api/v1/results", Value::Null).await.0,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn capture_counts_finalized_source_retention_and_keeps_revision_immutable() {
    use std::os::unix::fs::PermissionsExt;
    let (f, _, report) = prepare(true, true).await;
    let pool = f.db.pool().await;
    let requirement = json!({"id":"desktop","policy_revision":"1","kind":"image","scenario":"results","steps":["Open Results"],"viewport":{"width_css":1280,"height_css":800,"dpr_milli":1000},"environment":"test","count":2,"required":true,"applicability":"Results changed","gate_binding":"completion"});
    sqlx::query("INSERT INTO result_task_policies VALUES($1,$2,1,$3)")
        .bind(f.selection.lineage.project_id)
        .bind(f.selection.lineage.task_id)
        .bind(json!([requirement]))
        .execute(&pool)
        .await
        .unwrap();
    let receipt = results::record(&f.state, &context(&f).await, &report)
        .await
        .unwrap();
    let revision = receipt.report_digest.as_str();
    let app = aq_server::build_admin_router(f.state.clone());
    let root = format!("/api/v1/results/{}/revisions/{revision}", report.report_id);
    let (_, initial) = send(&app, "GET", &root, Value::Null).await;
    let actor = f
        .state
        .store
        .find_user_by_binding("dev", "owner@localhost")
        .await
        .unwrap()
        .unwrap();
    let dir = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let owner = aq_server::result_media::Owner::open(f.state.clone(), dir.path(), Uuid::new_v4())
        .await
        .unwrap();
    let source = initial["data"]["revision"]["source"]["base"].clone();

    for index in 0..3 {
        let id = Uuid::new_v4();
        let mut request: Value = serde_json::from_slice(&super::media_owner::request(id)).unwrap();
        request["capture"] = json!({"scenario":"results","captured_at_ms":NOW,"viewport":{"width_css":1280,"height_css":800,"dpr_milli":1000},"environment":"test","claimed_source":if index==0{Value::Null}else{source.clone()}});
        owner
            .reserve(
                &actor,
                report.report_id,
                revision,
                &serde_json::to_vec(&request).unwrap(),
            )
            .await
            .unwrap();
        let (_, before) = send(&app, "GET", &root, Value::Null).await;
        assert_eq!(before["data"]["assessment"]["evidence"]["satisfied"], false);
        owner
            .put(&actor, id, super::media_owner::png())
            .await
            .unwrap();
        owner.finalize(&actor, id).await.unwrap();
    }
    let (_, complete) = send(&app, "GET", &root, Value::Null).await;
    assert_eq!(
        complete["data"]["assessment"]["evidence"]["satisfied"],
        true
    );
    assert_eq!(
        complete["data"]["assessment"]["evidence"]["requirements"][0]["artifact_ids"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    assert_eq!(initial["data"]["revision"], complete["data"]["revision"]);
    assert_eq!(complete["data"]["assessment"]["acceptance"], "unreviewed");
    let version = complete["data"]["assessment"]["version"]
        .as_str()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    let expires: i64 = sqlx::query_scalar("SELECT max(retain_until_ms) FROM result_media_intents")
        .fetch_one(&pool)
        .await
        .unwrap();
    f.time.store(expires + 1, Ordering::SeqCst);
    let (_, expired) = send(&app, "GET", &root, Value::Null).await;
    assert_eq!(
        expired["data"]["assessment"]["evidence"]["satisfied"],
        false
    );
    assert!(
        expired["data"]["assessment"]["version"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            > version
    );
    assert_eq!(initial["data"]["revision"], expired["data"]["revision"]);
    assert!(
        sqlx::query("UPDATE result_task_policies SET requirements='[]'")
            .execute(&pool)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn feedback_desired_follow_version_and_task_filters() {
    let (f, _, report) = prepare(true, true).await;
    results::record(&f.state, &context(&f).await, &report)
        .await
        .unwrap();
    let app = aq_server::build_admin_router(f.state.clone());
    let path = format!("/api/v1/results/{}/follow", report.report_id);
    for (version, following) in [("1", true), ("2", false)] {
        let body =
            json!({"operation_id":Uuid::new_v4(),"expected_version":version,"following":following});
        let (code, out) = send(&app, "PUT", &path, body.clone()).await;
        assert_eq!(code, StatusCode::OK, "{out}");
        assert_eq!(out["data"]["disposition"], "confirmed");
        assert_eq!(send(&app, "PUT", &path, body).await.1["data"], out["data"]);
        let (_, feed) = send(&app, "GET", "/api/v1/results?mode=following", Value::Null).await;
        assert_eq!(
            feed["data"]["items"].as_array().unwrap().len(),
            usize::from(following)
        );
    }
    let (_, stale) = send(
        &app,
        "PUT",
        &path,
        json!({"operation_id":Uuid::new_v4(),"expected_version":"1","following":true}),
    )
    .await;
    assert_eq!(stale["data"]["disposition"], "conflict");
    let (_, other) = send(
        &app,
        "GET",
        &format!("/api/v1/results?task_id={}", Uuid::new_v4()),
        Value::Null,
    )
    .await;
    assert!(other["data"]["items"].as_array().unwrap().is_empty());
    assert_eq!(
        send(&app, "GET", "/api/v1/results?scope=project", Value::Null)
            .await
            .0,
        StatusCode::UNPROCESSABLE_ENTITY
    );
}

#[tokio::test]
async fn feedback_reload_keeps_exact_resolution_and_direct_principal_preference() {
    let (f, _, report) = prepare(true, true).await;
    let receipt = results::record(&f.state, &context(&f).await, &report)
        .await
        .unwrap();
    let pool = f.db.pool().await;
    let app = aq_server::build_admin_router(f.state.clone());
    let old = format!(
        "/api/v1/results/{}/revisions/{}",
        report.report_id,
        receipt.report_digest.as_str()
    );
    let comment = Uuid::new_v4();
    let (_, created) = send(
        &app,
        "POST",
        &format!("{old}/comments"),
        json!({
            "operation_id":comment,"expected_version":"1","body":"Check this revision",
            "anchor":{"kind":"result"},"follow":true
        }),
    )
    .await;
    assert_eq!(created["data"]["disposition"], "confirmed");
    let other = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO users(id,email,role,created_at) VALUES($1,'other@example.test','owner',now())",
    )
    .bind(other)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO result_follows VALUES($1,$2,true)")
        .bind(report.report_id)
        .bind(other)
        .execute(&pool)
        .await
        .unwrap();
    let (_, detail) = send(&app, "GET", &old, Value::Null).await;
    assert_eq!(detail["data"]["result"]["directly_following"], false);
    let follow = format!("/api/v1/results/{}/follow", report.report_id);
    for following in [true, false] {
        let (_, detail) = send(&app, "GET", &old, Value::Null).await;
        let (_, out) = send(&app, "PUT", &follow, json!({
            "operation_id":Uuid::new_v4(),"expected_version":detail["data"]["result"]["version"],"following":following
        })).await;
        assert_eq!(out["data"]["disposition"], "confirmed");
        let (_, feed) = send(&app, "GET", "/api/v1/results?mode=following", Value::Null).await;
        assert_eq!(feed["data"]["items"].as_array().unwrap().len(), 1);
        assert_eq!(
            feed["data"]["items"][0]["result"]["directly_following"],
            following
        );
    }
    let mut corrected = report.clone();
    corrected.report_id = Uuid::new_v4();
    corrected.operation_id = Uuid::new_v4();
    corrected.supersedes = Some(report.report_id);
    let next = results::record(&f.state, &context(&f).await, &corrected)
        .await
        .unwrap();
    let new = format!(
        "/api/v1/results/{}/revisions/{}",
        report.report_id,
        next.report_digest.as_str()
    );
    let (_, before) = send(&app, "GET", &new, Value::Null).await;
    assert_eq!(before["data"]["revision"]["addressed_feedback"], json!([]));
    let resolve_path = format!("/api/v1/threads/{comment}/resolve");
    let operation = json!({"operation_id":Uuid::new_v4(),"expected_version":"1",
        "follow_up":{"result_id":corrected.report_id,"revision":next.report_digest.as_str()},"reason":"Verified exact correction"});
    let mut bad = operation.clone();
    bad["follow_up"]["revision"] = json!("0".repeat(64));
    assert_eq!(
        send(&app, "POST", &resolve_path, bad).await.0,
        StatusCode::NOT_FOUND
    );
    let (_, resolved) = send(&app, "POST", &resolve_path, operation.clone()).await;
    assert_eq!(resolved["data"]["disposition"], "confirmed");
    assert_eq!(
        send(&app, "POST", &resolve_path, operation.clone()).await.1["data"],
        resolved["data"]
    );
    let mut changed = operation;
    changed["follow_up"]["result_id"] = json!(report.report_id);
    changed["follow_up"]["revision"] = json!(receipt.report_digest.as_str());
    assert_eq!(
        send(&app, "POST", &resolve_path, changed).await.0,
        StatusCode::CONFLICT
    );

    // Rebuild the router to exercise reload without an in-memory UI/route cache.
    let reloaded = aq_server::build_admin_router(f.state.clone());
    let (_, old_detail) = send(&reloaded, "GET", &old, Value::Null).await;
    assert_eq!(
        old_detail["data"]["revision"]["addressed_feedback"],
        json!([])
    );
    let (_, new_detail) = send(&reloaded, "GET", &new, Value::Null).await;
    assert_eq!(
        new_detail["data"]["revision"]["addressed_feedback"],
        json!([comment])
    );
    assert_eq!(
        new_detail["data"]["follow_up_submission"],
        json!({"state":"unsupported"})
    );
    assert_eq!(new_detail["data"]["assessment"]["acceptance"], "unreviewed");
    assert!(
        new_detail["data"]["assessment"]["version"]
            .as_str()
            .unwrap()
            .parse::<u64>()
            .unwrap()
            > before["data"]["assessment"]["version"]
                .as_str()
                .unwrap()
                .parse::<u64>()
                .unwrap()
    );
    let (_, comments) = send(&reloaded, "GET", &format!("{old}/comments"), Value::Null).await;
    let resolution = &comments["data"]["items"][0]["resolution"];
    assert_eq!(
        resolution["follow_up"],
        json!({"result_id":report.report_id,"revision":next.report_digest.as_str()})
    );
    assert_eq!(
        resolution["follow_up_report_id"],
        corrected.report_id.to_string()
    );
    assert!(
        comments["data"]["items"][0]["feedback_links"]
            .as_array()
            .unwrap()
            .is_empty()
    );
    serde_json::from_value::<aq_server::results_feedback::contract::Detail>(
        new_detail["data"].clone(),
    )
    .unwrap();
    serde_json::from_value::<aq_server::results_feedback::contract::CommentsPage>(
        comments["data"].clone(),
    )
    .unwrap();
    let (_, fresh_comments) = send(&reloaded, "GET", &format!("{new}/comments"), Value::Null).await;
    assert!(
        fresh_comments["data"]["items"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
