use super::*;

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
    let comment = check_principal_preference(&app, &pool, &old, report.report_id).await;
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
    check_resolution_replay(
        &app,
        &resolve_path,
        operation,
        json!({
            "result_id":report.report_id,"revision":receipt.report_digest.as_str()
        }),
    )
    .await;

    // Rebuild the router to exercise reload without an in-memory UI/route cache.
    let reloaded = aq_server::build_admin_router(f.state.clone());
    let new_detail = reloaded_detail(&reloaded, &old, &new, comment).await;
    assert_newer_assessment(&new_detail, &before);
    let (_, comments) = send(&reloaded, "GET", &format!("{old}/comments"), Value::Null).await;
    check_resolution(
        &comments,
        json!({"result_id":report.report_id,"revision":next.report_digest.as_str()}),
        corrected.report_id,
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

async fn check_principal_preference(
    app: &Router,
    pool: &sqlx::PgPool,
    old: &str,
    result: Uuid,
) -> Uuid {
    let comment = Uuid::new_v4();
    let (_, created) = send(
        app,
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
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO result_follows VALUES($1,$2,true)")
        .bind(result)
        .bind(other)
        .execute(pool)
        .await
        .unwrap();
    let (_, detail) = send(app, "GET", old, Value::Null).await;
    assert_eq!(detail["data"]["result"]["directly_following"], false);
    let follow = format!("/api/v1/results/{}/follow", result);
    for following in [true, false] {
        let (_, detail) = send(app, "GET", old, Value::Null).await;
        let (_, out) = send(app, "PUT", &follow, json!({
            "operation_id":Uuid::new_v4(),"expected_version":detail["data"]["result"]["version"],"following":following
        })).await;
        assert_eq!(out["data"]["disposition"], "confirmed");
        let (_, feed) = send(app, "GET", "/api/v1/results?mode=following", Value::Null).await;
        assert_eq!(feed["data"]["items"].as_array().unwrap().len(), 1);
        assert_eq!(
            feed["data"]["items"][0]["result"]["directly_following"],
            following
        );
    }
    comment
}

async fn reloaded_detail(reloaded: &Router, old: &str, new: &str, comment: Uuid) -> Value {
    let (_, old_detail) = send(reloaded, "GET", old, Value::Null).await;
    assert_eq!(
        old_detail["data"]["revision"]["addressed_feedback"],
        json!([])
    );
    let (_, new_detail) = send(reloaded, "GET", new, Value::Null).await;
    assert_eq!(
        new_detail["data"]["revision"]["addressed_feedback"],
        json!([comment])
    );
    assert_eq!(
        new_detail["data"]["follow_up_submission"],
        json!({"state":"unsupported"})
    );
    assert_eq!(new_detail["data"]["assessment"]["acceptance"], "unreviewed");
    new_detail
}

async fn check_resolution_replay(
    app: &Router,
    resolve_path: &str,
    operation: Value,
    original: Value,
) {
    let mut bad = operation.clone();
    bad["follow_up"]["revision"] = json!("0".repeat(64));
    assert_eq!(
        send(app, "POST", resolve_path, bad).await.0,
        StatusCode::NOT_FOUND
    );
    let (_, resolved) = send(app, "POST", resolve_path, operation.clone()).await;
    assert_eq!(resolved["data"]["disposition"], "confirmed");
    assert_eq!(
        send(app, "POST", resolve_path, operation.clone()).await.1["data"],
        resolved["data"]
    );
    let mut changed = operation;
    changed["follow_up"] = original;
    assert_eq!(
        send(app, "POST", resolve_path, changed).await.0,
        StatusCode::CONFLICT
    );
}

fn assert_newer_assessment(detail: &Value, before: &Value) {
    assert!(
        detail["data"]["assessment"]["version"]
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
}

fn check_resolution(comments: &Value, follow_up: Value, follow_up_report: Uuid) {
    let resolution = &comments["data"]["items"][0]["resolution"];
    assert_eq!(resolution["follow_up"], follow_up);
    assert_eq!(
        resolution["follow_up_report_id"],
        follow_up_report.to_string()
    );
    assert!(
        comments["data"]["items"][0]["feedback_links"]
            .as_array()
            .unwrap()
            .is_empty()
    );
}
