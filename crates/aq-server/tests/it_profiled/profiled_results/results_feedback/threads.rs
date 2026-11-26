use super::*;

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
    // QA P1-09: the read carries the report's conclusion and process outcome.
    assert_eq!(
        detail["data"]["revision"]["summary"],
        json!(report.conclusion)
    );
    assert_eq!(
        detail["data"]["revision"]["process_outcome"],
        json!(report.outcome)
    );
    let path = format!("{root}/comments");
    let (id, thread) = corrected_comment(&app, &path).await;
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
    check_resolved_detail(&app, &root, id).await;
    check_resolved_thread(
        &app,
        &path,
        report.report_id,
        receipt.report_digest.as_str(),
    )
    .await;
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
    let first = check_comment_bounds(&app, &path).await;
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

async fn corrected_comment(app: &Router, path: &str) -> (Uuid, String) {
    let id = Uuid::new_v4();
    let body = json!({"operation_id":id,"expected_version":"1","body":"Fix the caption","anchor":{"kind":"result"},"follow":true});
    let (code, first) = send(app, "POST", path, body.clone()).await;
    assert_eq!(code, StatusCode::OK, "{first}");
    assert_eq!(first["data"]["disposition"], "confirmed");
    assert_eq!(
        send(app, "POST", path, body.clone()).await.1["data"],
        first["data"]
    );
    let mut changed = body.clone();
    changed["body"] = json!("changed");
    assert_eq!(
        send(app, "POST", path, changed).await.0,
        StatusCode::CONFLICT
    );
    let (_, comments) = send(app, "GET", path, Value::Null).await;
    let comment = &comments["data"]["items"][0];
    assert_eq!(comment["body"], "Fix the caption");
    let thread = comment["thread_id"].as_str().unwrap();
    let edit = format!("/api/v1/comments/{id}/revisions");
    let corrected = json!({"operation_id":Uuid::new_v4(),"expected_version":"1","body":"Fix caption and spacing"});
    assert_eq!(
        send(app, "POST", &edit, corrected).await.1["data"]["disposition"],
        "confirmed"
    );
    let (_, history) = send(app, "GET", path, Value::Null).await;
    assert_eq!(
        history["data"]["items"][0]["history"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    (id, thread.to_owned())
}

async fn check_resolved_detail(app: &Router, root: &str, id: Uuid) {
    let (_, detail) = send(app, "GET", root, Value::Null).await;
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
}

async fn check_resolved_thread(app: &Router, path: &str, result: Uuid, revision: &str) {
    let (_, comments) = send(app, "GET", path, Value::Null).await;
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
        revision
    );
    assert_eq!(
        comments["data"]["items"][0]["resolution"]["follow_up_report_id"],
        result.to_string()
    );
    assert_eq!(
        comments["data"]["items"][0]["resolution"]["reason"],
        "Caption checked"
    );
}

async fn check_comment_bounds(app: &Router, path: &str) -> Uuid {
    for body in ["".to_string(), "é".repeat(8193)] {
        let (code,v)=send(app,"POST",path,json!({"operation_id":Uuid::new_v4(),"expected_version":"1","body":body,"anchor":{"kind":"result"}})).await;
        assert_eq!(code, StatusCode::UNPROCESSABLE_ENTITY, "{v}");
    }
    for anchor in [
        json!({"kind":"image_region","artifact_id":Uuid::new_v4(),"digest":format!("sha256:{}","a".repeat(64)),"x":0.9,"y":0.0,"width":0.2,"height":0.1}),
        json!({"kind":"video_range","artifact_id":Uuid::new_v4(),"digest":"invalid","start_ms":1,"end_ms":2}),
    ] {
        assert_eq!(send(app,"POST",path,json!({"operation_id":Uuid::new_v4(),"expected_version":"1","body":"bad anchor","anchor":anchor})).await.0,StatusCode::UNPROCESSABLE_ENTITY);
    }
    let first = Uuid::new_v4();
    let body = json!({"operation_id":first,"expected_version":"1","body":"first","anchor":{"kind":"result"},"follow":true});
    assert_eq!(
        send(app, "POST", path, body.clone()).await.1["data"]["disposition"],
        "confirmed"
    );
    let reply = Uuid::new_v4();
    let response=send(app,"POST",path,json!({"operation_id":reply,"expected_version":"2","thread_id":first,"body":"reply","anchor":{"kind":"result"}})).await.1;
    assert_eq!(response["data"]["disposition"], "confirmed", "{response}");
    let (_, page) = send(app, "GET", &format!("{path}?limit=1"), Value::Null).await;
    assert_eq!(page["data"]["items"][0]["id"], first.to_string());
    let cursor = page["data"]["next_cursor"].as_str().unwrap();
    assert_eq!(
        send(
            app,
            "GET",
            &format!("{path}?limit=2&cursor={cursor}"),
            Value::Null
        )
        .await
        .0,
        StatusCode::CONFLICT
    );
    let (_, page2) = send(
        app,
        "GET",
        &format!("{path}?limit=1&cursor={cursor}"),
        Value::Null,
    )
    .await;
    assert_eq!(page2["data"]["items"][0]["id"], reply.to_string());
    assert!(page2["data"]["next_cursor"].is_null());
    first
}
