use super::*;

#[tokio::test]
async fn reads_use_existing_owner_routes_and_preserve_pages() {
    let project = "00000000-0000-4000-8000-000000000010";
    let id = "00000000-0000-4000-8000-000000000020";
    let cases = vec![
        (vec!["tasks", "list"], "/api/human/overview/tasks".into()),
        (
            vec!["tasks", "list", "--limit", "3", "--cursor", project],
            format!("/api/human/overview/tasks?task_limit=3&after={project}"),
        ),
        (
            vec![
                "tasks",
                "list",
                "--project",
                project,
                "--limit",
                "3",
                "--cursor",
                "a/b?c",
            ],
            format!("/api/human/projects/{project}/tasks?limit=3&cursor=a%2Fb%3Fc"),
        ),
        (
            vec!["tasks", "show", id, "--project", project],
            format!("/api/human/projects/{project}/tasks/{id}"),
        ),
        (vec!["decisions", "list"], "/api/reviews/decisions".into()),
        (
            vec!["decisions", "show", id],
            format!("/api/reviews/decisions/{id}"),
        ),
        (vec!["workers", "list"], "/api/workers".into()),
        (vec!["projects", "list"], "/api/v1/projects".into()),
        (
            vec!["projects", "show", project],
            format!("/api/projects/{project}"),
        ),
    ];
    for (args, path) in cases {
        let page = json!({"data":{"items":[{"id":id}]},"next_cursor":"next","has_more":true});
        let mut srv = server(vec![(StatusCode::OK, page.clone())]).await;
        assert_eq!(
            aqv2::execute(srv.client.clone(), &args).await.unwrap(),
            page
        );
        let read = srv.next().await;
        assert_eq!(read.method, "GET");
        assert_eq!(read.path, path);
    }
}

#[tokio::test]
async fn server_refusals_and_unavailable_reads_do_not_leak_error_bodies() {
    for (status, code) in [
        (StatusCode::FORBIDDEN, "http_error"),
        (StatusCode::NOT_FOUND, "unsupported"),
        (StatusCode::UPGRADE_REQUIRED, "unsupported"),
    ] {
        let mut srv = server(vec![(status, json!({"private":"sensitive server detail"}))]).await;
        let error = aqv2::execute(srv.client.clone(), &["projects", "list"])
            .await
            .unwrap_err();
        assert_eq!(error["code"], code);
        assert!(!error.to_string().contains("sensitive"));
        assert_eq!(srv.next().await.method, "GET");
        assert!(srv.seen.try_recv().is_err());
    }
}

#[tokio::test]
async fn chat_and_inbox_read_history_but_live_chat_refuses_the_unavailable_contract() {
    let discovery: Value = serde_json::from_str(include_str!(
        "../../../../protocol/human-supervisor-conversation/v1/discovery.example.json"
    ))
    .unwrap();
    let history: Value = serde_json::from_str(include_str!(
        "../../../../protocol/human-supervisor-conversation/v1/history.example.json"
    ))
    .unwrap();
    for args in [
        vec!["messages", "inbox", "--after", "0", "--limit", "3"],
        vec!["chat", "--after", "0", "--limit", "3"],
    ] {
        let mut srv = server(vec![
            (StatusCode::OK, discovery.clone()),
            (StatusCode::OK, history.clone()),
        ])
        .await;
        assert_eq!(
            aqv2::execute(srv.client.clone(), &args).await.unwrap(),
            history
        );
        assert_eq!(srv.next().await.path, "/api/human/conversations/global");
        let read = srv.next().await;
        assert_eq!(
            read.path,
            "/api/human/conversations/00000000-0000-0000-0000-000000000001/messages?limit=3&after=0"
        );
        assert_eq!(read.method, "GET");
    }
    let mut srv = server(vec![(StatusCode::OK, discovery)]).await;
    let err = aqv2::execute(
        srv.client.clone(),
        &["chat", "--once", "hello", "--timeout", "1"],
    )
    .await
    .unwrap_err();
    assert_eq!(err["code"], "unsupported");
    assert!(err["message"].as_str().unwrap().contains("F1b"));
    assert_eq!(srv.next().await.method, "GET");
    assert!(srv.seen.try_recv().is_err());
}

#[tokio::test]
async fn review_regression_rejects_duplicate_decisions() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("decision.json");
    let payload =
        serde_json::to_string(&fixture("request-decision-resolve.json")["payload"]).unwrap();
    for text in [
        payload.replace(
            "\"choice\":\"approve\"",
            "\"choice\":\"reject\",\"choice\":\"approve\"",
        ),
        payload.replace("\"digest\":", "\"digest\":\"other\",\"digest\":"),
    ] {
        std::fs::write(&path, text).unwrap();
        let mut srv = server(vec![]).await;
        let err = aqv2::execute(
            srv.client.clone(),
            &[
                "decisions",
                "respond",
                "00000000-0000-4000-8000-000000000020",
                "--request",
                path.to_str().unwrap(),
            ],
        )
        .await
        .unwrap_err();
        assert_eq!(err["code"], "invalid_request_file");
        assert!(srv.seen.try_recv().is_err());
    }
}

#[tokio::test]
async fn review_regression_preserves_project_not_found() {
    let mut srv = server(vec![(StatusCode::NOT_FOUND, json!({"code":"not_found"}))]).await;
    let err = aqv2::execute(
        srv.client.clone(),
        &[
            "tasks",
            "list",
            "--project",
            "00000000-0000-4000-8000-000000000099",
        ],
    )
    .await
    .unwrap_err();
    assert_eq!(err["code"], "not_found");
    assert_eq!(err["http_status"], 404);
    assert_eq!(srv.next().await.method, "GET");
    assert!(srv.seen.try_recv().is_err());
}
